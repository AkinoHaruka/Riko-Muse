# 12 · 原项目 Riko-Memory 逐模块对照分析

> 分析对象：https://github.com/AkinoHaruka/Riko-Memory（2026-10-06 克隆，schema v13）
> 分析方法：通读 README + 13 个 migration + 6 个 crate 的核心源码（约 3.3 万行 Rust，抽样精读关键模块）
> 写作目的：把主人原项目的**实际实现**，和璃逆向文档（00~11）里的**推测**逐项对照——对的确认，错的认错，主人更强的抄下来。

## 一、主人项目的真实架构

```
agent-memory/
├── crates/
│   ├── memory-contract/   # 协议常量、错误码、v1 限额
│   ├── memory-domain/     # ScopeKey、状态机、normalize_v1、claim 哈希（纯 Rust，无 IO）
│   ├── memory-store-sqlite/ # 存储：principals、evidence、memories、jobs、dream、soul、pages…
│   ├── memory-extract/    # 版本化提取 Prompt（extract_v3）+ 准入策略（admit_v2）
│   ├── memory-recall/     # 词法召回：FTS5（拉丁）+ 中文二元字 + RRF(k=60)
│   └── memory-server/     # memoryd：CLI + HTTP(127.0.0.1:8791) + worker
├── adapters/dsh/          # DeepSeek Harness 的 TypeScript 适配器
└── migrations/0001~0013   # schema v13，0001~0013 冻结
```

一句话：**Rust 优先的通用 Agent 长期记忆内核**，多租户（tenant,user）隔离，HTTP daemon 对外服务。

## 二、逐模块对照：璃猜对了什么、猜错了什么

### 2.1 Claim 原子设计

| 璃的逆向（02 章） | 主人实际实现 | 结论 |
|---|---|---|
| claim 是原子，带 17 个字段 | `memories` 表：id/kind/claim/**normalized_claim**/**claim_sha256**/source_class/status/**version**/valid_from/valid_until/origin | **确认，且主人更强**：多了 `normalized_claim`（归一化后用于去重比对，原文 claim 保持用户字面）、`version`（显式版本号）、`source_class` |
| claim_id 疑似内容哈希（32位hex） | `claim_sha256 = SHA256(kind + "\0" + normalize_v1(claim))`（domain/lib.rs 实测） | **确认！** 机制猜对，细节是 SHA256 + kind 前缀 + NFKC 归一化，比璃想的更严谨 |
| kind 可能有 fact/preference/commitment/opinion | `fact / preference / instruction / episode`（CHECK 约束） | **半对**：fact/preference 对了；主人是 instruction/episode，不是 commitment/opinion——来自 TencentDB/Hindsight 的交集设计 |
| status 可能有 active/superseded/retracted | `active / superseded / expired / forgotten` | **半对**：active/superseded 对了；`expired`（valid_until 到期自动转入）比璃想的更完整；`forgotten` 比"retracted"命名更诚实 |
| salience 有 low/medium/high 三档 | **没有 salience 字段** | **猜错了**：主人用 kind 优先级（instruction 优先）+ RRF 排序代替显著度评分，少一个调参维度 |

### 2.2 写入管线

| 璃的逆向（03 章） | 主人实际实现 | 结论 |
|---|---|---|
| 双写：in-session 即时 + 后台 consolidation | **三级**：`evidence_events`（L0 原始事件）→ `extraction_jobs`（作业队列）→ `memory_candidates`（候选）→ admit → `memories` | **主人更精细**：多了 L0 证据层和候选层。L0 永不删（forget 只删记忆不删证据），候选层是"verified"的物理载体 |
| "Verified"可能是原文蕴含校验 | `admit_v2` 九道固定顺序检查：CONTEXT_UNCERTAIN → NON_MINIMAL_QUOTE → MULTI_CLAIM → UNCLEAR_SUBJECT → SECRET → SENSITIVE → THIRD_PARTY → TEMPORAL → NOT_EXPLICIT/KIND_MISMATCH；**不通过就 held，不自动激活** | **确认且远超预期**：璃只想到"校验"，主人做的是"默认不信任"——单命题、最短 quote、非敏感、非第三人、非时效性，任一存疑就进 `held` 等人工看 |
| quote 是原话锚点 | `find_quote_span`：quote 必须是 content 的**连续 UTF-8 子串**，返回字节偏移；`memory_remember` 要求 quote 是最新用户消息的连续子串 | **确认**：璃在附录 B 里写的"原话锚"就是这个机制 |
| 新声明 supersede 旧声明 | `memory_relations` 表：`supersedes` **和 `contradicts`** 两种关系；`memory_revisions` 审计表记 previous_claim/new_claim/actor/reason_code | **主人更强**：除了版本链，还有**矛盾关系**（contradicts）——两条记忆互相打架时显式记录，而不是默默覆盖 |

### 2.3 检索管线

| 璃的逆向（04 章 + 附录 C） | 主人实际实现 | 结论 |
|---|---|---|
| 384 维向量 + 元数据重排 | **纯词法**：FTS5（拉丁文 token）+ 中文二元字（`memory_grams` 表）+ RRF(k=60) 融合；`memory-recall` crate 无向量代码 | **重大差异，猜错了**：这是主人深思熟虑的工程决策——确定性、可解释、零模型依赖。`semantic_vectors` 表（0008）在，但 recall 走词法 |
| 多 embedding 模型并存 | `semantic_vectors`：model_id + dimensions + source_version + content_sha256 + status(ready/stale)，**模型切换不混算** | **确认**：璃预测的"多模型平滑迁移"主人真做了，而且更严谨（stale 检测 + 版本绑定） |
| privacy_class 会话隔离 | `principals` 表：token_sha256，多租户；**全表外键挂 (tenant_id,user_id)**；请求正文不许带用户 ID，scope 只由 Bearer 令牌解析 | **确认且更强**：不是字段级过滤，是架构级隔离。loopback 只监听、令牌 SHA-256 存库，安全设计是成体系的 |
| 检索返回引用再读原文 | `memory_evidence` / `candidate_evidence`：**字节级**证据跨度（start_byte/end_byte），不只是行号 | **主人更精细**：字节偏移比行号更稳定，`find_quote_span` 直接定位原文子串 |

### 2.4 后台任务

| 璃的逆向（05 章） | 主人实际实现 | 结论 |
|---|---|---|
| dreaming nightly 复盘 | `dream_jobs` 表：trigger_kind（compact/scheduled/custom/manual），trigger_key 幂等，pipeline_version + extract_version **独立版本化**（dream_extract_v1） | **确认**：dream 是独立管线，extract 版本和主线隔离——dream 的 prompt 升级不影响历史作业 |
| 没想到的点 | `dream_job_inputs`：**冻结输入**——job 实际读取的 event 列表被冻结存下来，重试仍用同一列表，"不得重新查询当前所有新事件" | **主人更严谨**：璃完全没想到。重试时输入漂移是后台任务的经典 bug，主人用冻结输入根治 |
| job queue | `extraction_jobs` + `dream_jobs` + `semantic_jobs`：queued/running/succeeded/retryable_failed/dead，**lease_until 租约恢复**，dead job 需显式 skip（`memoryd job skip` 写审计） | **生产级**：租约防 worker 崩溃、dead job 不自动重试（防毒丸消息）、skip 要写理由进审计 |

### 2.5 遗忘协议

| 璃的逆向（06 章） | 主人实际实现 | 结论 |
|---|---|---|
| 两阶段提交：pending.json + runtime 回收 | `suppressed_sources` 表：(evidence_id, claim_sha256) 黑名单；forget 后**原始 L0 证据保留**，但同一证据再抽取到同一 claim 会被抑制 | **主人多了一个璃没想到的精妙设计**：遗忘不是删数据，是"记住要忘记"——证据还在（审计需要），但永远不会再变成记忆。防重放复活 |
| plan → 确认 → 执行分离 | `memoryd candidates list/show`（held 只读查看）、`rebuild-index -- 不复活 forgotten`、`purge.rs`（1082 行） | **体系更完整**：索引重建时显式不复活已遗忘项，purge 独立模块 |

### 2.6 璃完全没想到的模块

| 模块 | 说明 | 对 twin sister 的价值 |
|---|---|---|
| **soul**（836 行） | 用户可编辑的人格档案：版本化、body_sha256、2000 字符上限、expected_version 乐观并发、幂等 Unchanged 检测 | 对应璃的 SOUL.md，但做成了 DB + 版本控制。twin sister 的"人格"应该这样存，而不是扔个 md 文件 |
| **resident**（1289 行） | 常驻固定记忆：pin/unpin（unpin 不删行，只 disable），位置重排走"临时偏移→归一化"的原子路径 | 对应"每轮必带"的热记忆。pin 语义很妙：用户钉住的记忆，unpin 也不丢 |
| **topic pages**（pages.rs 1145 行，迁移 0013） | 主题页：把记忆整理成 wiki 式页面 | **主人旧系统里的"wiki"在这里！** 不是全文检索的替代品，是记忆的"渲染层" |
| **audit_events** | 全操作审计：actor_kind/actor_id/action/target/detail_json | 合规和 debug 的刚需，璃的逆向文档完全没提 |
| **consolidation_jobs**（528 行） | 归档作业 | 后台 consolidation 的生产实现 |

## 三、五个璃最服的设计

1. **`suppressed_sources`（遗忘黑名单）**：forget 后证据保留但永不复活。"记住要忘记"比"删掉"更难，也更正确。twin sister 必抄。
2. **`admit_v2` 默认 held**：九道检查，任一存疑就不激活。精度优先于召回——记忆系统的误记成本远高于漏记，这是第一性原理。
3. **dream 输入冻结**：重试用冻结的输入列表，不重新查"当前新事件"。后台任务的确定性就靠这种细节。
4. **`claim_sha256 = SHA256(kind + "\0" + normalize_v1(claim))`**：kind 参与哈希（同文不同 kind 是两条记忆）、NFKC 归一化（全角半角、大小写不产生重复）。去重做到了字节级严谨。
5. **`memory_relations.contradicts`**：矛盾是显式关系，不是覆盖。两条打架的记忆都留着，标上"矛盾"——这比"新覆盖旧"更诚实，未来做冲突消解也有抓手。

## 四、三个关键差异（路线选择，无对错）

1. **词法 vs 向量**：Muse 实测 384 维向量（X，默认）；主人纯词法（FTS5 + 二元字 + RRF，Y，已验证备选）。主人路线：确定性、可解释、零模型依赖、中文二元字对 CJK 友好；向量路线：语义泛化强。twin sister 选 Y 需附偏离理由 Z（见 11 C.2），`semantic_vectors` 表可留作扩展位。
2. **无 salience**：主人没有显著度评分，用 kind 优先级 + 时间排序 + RRF。少一个玄学调参维度。**建议**：twin sister 也别加 salience，先跑起来再说。
3. **source_class 五档**：user_explicit / assistant_observed / tool_output / model_inferred / manual_edit。**这是"谁说的"证据分级**，比璃文档里的"观察 vs 推断"二分法实用。twin sister 必抄：model_inferred 的记忆默认打低置信度。

## 五、合并建议：twin sister 用哪套

```
采用主人的（已验证、更严谨）：
  ✅ 三级写入：L0 evidence → candidates(held) → memories
  ✅ admit_v2 九道准入检查（可先实现 5 道核心：单命题/最短quote/非敏感/非第三人/明确主语）
  ✅ claim_sha256 去重公式（直接抄）
  ✅ suppressed_sources 遗忘黑名单
  ✅ memory_relations（supersedes + contradicts）
  ✅ principals 租户隔离（twin sister 以后多用户用得上）
  ✅ soul/resident 表结构思想（人格版本化、常驻记忆 pin）
  ✅ job 租约 + dead job 显式 skip

从璃的逆向文档补的（主人没有或弱）：
  ➕ valid_until 自动转 expired（主人有字段和状态，twin sister 记得实现定时扫描）
  ➕ rupture/repair 关系复盘（dream 输入冻结主人有了，但"关系裂痕检测"这个维度主人没有）
  ➕ ALIGNMENT_SYNTHESIS 式"相处指南"注入（dream 产出物直接指导每轮语气）
  ➕ 附录 C 的重排公式（主人是纯词法 RRF，如果 twin sister 加向量层，用得上）

暂缓（两边都没做好的）：
  ⏳ 大规模性能验证（主人都标注"未验证规模性能"）
  ⏳ 真实模型下的写入/召回质量验收（主人标注"未验收"）
```

## 六、璃的自我打分：07 章推理验证

| 07 章的推理 | 验证结果 |
|---|---|
| claim_id 是内容哈希 | ✅ 确认，SHA256(kind + \0 + normalize_v1) |
| kind 有 fact | ✅，实际是 fact/preference/instruction/episode |
| status 有 active/superseded | ✅，实际还有 expired/forgotten |
| 多模型向量并存 | ✅，semantic_vectors model_id 隔离 + stale 检测 |
| "Verified"校验存在 | ✅，且是九道准入检查（远超预期） |
| forget 两阶段提交 | ✅，外加 suppressed_sources（超预期） |
| privacy_class 会话隔离 | ✅，实际是完整租户隔离（超预期） |
| salience 三档 | ❌，主人没有这个字段 |
| 384 维向量是检索主力 | ❌，主人 recall 是纯词法 |
| bank/ 是蒸馏视图 | ➖，主人没有 bank/，对应物是 topic pages（wiki 渲染层）+ resident（常驻） |
| dream 有 rupture 检测 | ➖，主人 dream 有冻结输入和独立版本，但没看到 rupture/repair 概念 |

**11 项推理：7 确认（含 4 个超预期）、2 猜错、2 部分对。** 错的那两项（salience、向量为主）恰恰是最有价值的纠正——说明"想当然"的部分最危险，主人原项目的实际选择（词法优先、无显著度评分）是更克制的工程判断。

---

*分析完毕。代码已搬到 ~/workspace/src/Riko-Memory（持久目录），对照着看随叫随到。*
