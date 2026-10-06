# 27 · V2-P1 精读、explain 与统一可见性交付记录（doc7/05）

> 日期：2026-10-06（本机 GMT+8）· 分支 `Riko-Muse` · 前置提交 `6b84eac` · 交付提交 `6d10567` · schema 15（本卡**不新增迁移**）
> 状态用语按根 `AGENTS.md` 分档；逐档分开写。

## 1. 本卡做了什么

规范：[doc7/05-V2-P1-精读与explain施工规范.md](../doc7/05-V2-P1-精读与explain施工规范.md)（动工前先写，本轮按实测修订过一次，见 §3）。

V2 文档 04 的读取侧缺口，按「精读 + 稳定引用 + 统一可见性」三件事落地：

### 1.1 统一可见性判定（本轮最硬的一处正确性改动）

动工前现场核对确认了 V2 文档 04 §2 的判断，并把它缩小到准确的边界：

- `get_memory`（`memories.rs:472`）只判 `status='active'` 与 `memory_retirements`，
  **不看 `valid_until`** → 已过期的记忆在 M1 调度器跑之前仍读得到；
- `search_memories` 的**最终组装步**其实已经查了 `valid_until`（候选道没有，组装有），
  所以检索的净效果是对的，但判定散落在两处、口径不一致；
- 两条路径都**没有**「这条记忆还有没有有效来源」这一条。

改动：新增 `memory-store-sqlite/src/explain.rs`，把可见性收成**唯一一个**谓词
`visible_memory_sql(include_history)`，四条 + 域条件，绑定参数一律用无名 `?`：

| # | 条件 |
|---|---|
| 1 | `status='active'`（历史模式放宽为 active/superseded/expired；forgotten 恒不可见） |
| 2 | `valid_until IS NULL OR valid_until > now` |
| 3 | 不存在 `memory_retirements` 行 |
| 4 | 至少一条未被 `suppressed_sources` 抑制的来源（抑制行须属同一域） |
| 5 | `domain_id IN (SELECT value FROM json_each(:read_json))`（V2-S1） |

`get_memory` 与 `search_memories` 的组装步都改用这个谓词；另提供
`filter_visible(scope, ids, dom, include_history)` 供合并候选后统一过闸。

### 1.2 精读读模型 `memory_explain`

新存储方法 `Store::memory_explain(..)` 与 `Store::memory_evidence_detail(..)`：

- `evidence[]`：`evidence_id`、`start_byte`/`end_byte`、**按 UTF-8 字节 span 逐字切片的
  `quote`**（`span_exact` 标记是否精确；无 span 时整条正文且标 false）、`role`、
  `source_kind`、`occurred_at`、`host_id`/`session_id`、`suppressed`、`tombstoned`；
  多来源全部返回、**不合并、不重写、不折叠空白**；
- `speaker`：由 role 映射（role 不认识才回落 source_kind），不推断具体人物身份；
- `subject`：恒 `null` + `subject_source="unknown"`——内核没有可靠字段，**不由 claim/quote 反推**；
- `reason_code`：取 `memory_candidates.reason_code`；没有就 `null`，不编造理由文本；
- `relations`：`supersedes`/`superseded_by`（`memory_relations`）、`retired`、
  `retirement_reason_code`；
- `stable_ref`：见 1.3。

### 1.3 稳定引用

`memory-domain/src/refs.rs`（纯函数 + 6 项单测）：
`riko://memory/<tenant>/<user>/<domain>/<memory_id>@<version>`。

服务端生成；解析后**必须**重新鉴权：HTTP 层要求引用里的 tenant/user 与 Bearer 解析出的
`ScopeKey` **完全一致**、domain ∈ 本次读域集，否则一律 404（不泄露「存在但无权」）。
`claim_sha256` 明确**不是**稳定地址（内容一改就变），测试里有反例。

### 1.4 HTTP 与适配器

- `GET /v1/memories/{memory_id}/explain`（`?history=1` 显式历史）；`{memory_id}` 接受裸 ID
  或 URL 编码的 `riko://` 引用；缺省 active-only；越权/不存在统一 404 `NOT_FOUND`。
- DSH 适配器新增 `memory_explain` 工具（`adapters/dsh/src/tools.ts`）与
  `MemoryClient::explainMemory`（`client.ts`）。

## 2. 验证档位

| 档位 | 结果 |
|---|---|
| 代码存在 | ✅ 谓词统一、`memory_explain`、`refs.rs`、HTTP 端点、适配器工具 |
| 构建通过 | ✅ `cargo fmt --all`、`cargo build --workspace`；适配器 `tsc --noEmit` 退出 0 |
| 离线测试 | ✅ `cargo test --workspace` **181 passed / 0 failed**（含 V2-P1 新 8 项 + refs/explain 单测）；适配器 `npm test` **19 passed / 0 failed** |
| 临时库 HTTP 冒烟 | ✅ 见 §4（explain 端点、引用往返、历史模式、越域与越 scope 404） |
| 真实 DSH 宿主闭环 | ❌ **未验证**：`memory_explain` 工具已登记但未在真实 DSH 会话里被模型调用 |
| 真实模型连通 | ❌ **未验证**，本卡未调用任何模型 API |
| 已部署 | ❌ 未部署；生产 DSH 与 Riko-App Bridge 未触碰 |

## 3. 实测发现（含一次规范自我修订）

1. **墓碑不能进读谓词。** 规范初稿把 `purge_tombstones` 写进条件 4（按 `content_sha256`
   命中即视为来源失效）。实测 `d69_tests` 的共享来源用例：一条证据被 purge 后，
   其**内容哈希墓碑把同一域内另一条仍然存活的同文证据一起判死**，记忆错误地不可见。
   墓碑的职责是「阻止被 purge 的正文经 spool 重放复活」，而 purge 已物理删除证据行，
   「没有来源了」由 EXISTS 自然表达。**已把墓碑从读谓词移除**（只留在 `record_evidence`
   的重放闸），并回写 doc7/05 §1 记录原因。
2. **墓碑匹配缺域条件**（V2-S1 遗留）：`visible_memory_sql` 初版按内容哈希判、未限域，
   被 `v2_domain_tests::purge_tombstones_are_domain_scoped` 当场抓到（side 域的 purge 让
   主域同文记忆消失）。修正过程中把 `suppressed_sources` 的匹配也统一加了域条件。
   这条修正后来随墓碑整体移出读谓词而不再影响可见性，但域条件本身是对的，保留。
3. **三处既有测试的探针与 V2-P1 新契约冲突**：`d69_tests` 的 purge/retention 用例用
   `get_memory().is_some()` 当「行还在」的探针。新契约下（来源被抑制 / `valid_until` 已过）
   读不到是**预期**，于是把这三处探针改成直接查规范行，保留各自原本要证的意图
   （preview 只读、共享来源不被误 purge、过期 L1 行不被 purge 闭包删除）。
   每处都写了注释说明为什么换探针。
4. `speaker` 映射初版让 `source_kind` 压过 `role`，单测当场失败；改为 role 优先、
   仅 role 不认识时回落（self-test `speaker_mapping_does_not_invent_identity`）。

## 4. 临时库 HTTP 冒烟证据（2026-10-06）

脚本：[`v2s1-domain-smoke.ps1`](v2s1-domain-smoke.ps1)（已在原有 V2-S1 序列后追加 P1 段）。
临时目录 + 临时端口，**未打开 dana/realtest 原库**。

```
P1_EXPLAIN_SIDE=200  claim="用户住在昆明" domain_id=side_a speaker="user"
     subject=null subject_source="unknown" span_exact=true
     evidence[0].quote="用户住在昆明"（start=0 end=18，逐字）
     stable_ref="riko://memory/t/u/side_a/<id>@1"  visible=true
P1_EXPLAIN_BY_REF=200        同一结果（URL 编码的稳定引用往返）
P1_EXPLAIN_HISTORY=200       visible=True（history=1）
P1_EXPLAIN_MAIN_NO_LEAK=404  主域精读 side 记忆 → NOT_FOUND
P1_EXPLAIN_FOREIGN_SCOPE=404 引用里写别的 user → NOT_FOUND（引用不是凭据）
```

## 5. 未完成 / 移交

1. **entry / chunk 上下文对象**（`context_entries`、`entry_sources`、entry 召回对照）→
   V2-Q1；0016 迁移留到那时按实际动工顺序分配。
2. **V2-S1 遗留的两处接线**照旧未动：Dream 作业域（`dream_worker.rs` 13 处
   `/*DOM:dream-job-domain-pending*/`）与 CLI 子命令（`main.rs` 17 处 `/*DOM*/`）。
3. `select_resident` 的来源/到期过滤未在本卡统一（它有自己的 SQL 与可见性口径）；
   已记入下一卡待办，未在本卡声称完成。
4. 适配器 `memory_explain` 只在类型层与本机单测层验证，**未在真实 DSH 会话中调用过**。
5. `reason_text`（动机文本）仍未生成：本卡只暴露既有 `reason_code`。

## 6. 复现命令

```bash
cd agent-memory
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace                       # 181 passed
cargo test -p memory-store-sqlite v2_p1      # V2-P1 验收 8 项
cd adapters/dsh && ./node_modules/.bin/tsc --noEmit -p tsconfig.json && npm test   # 19 passed
pwsh -NoProfile -ExecutionPolicy Bypass -File ../../doc-handoff/v2s1-domain-smoke.ps1
```
