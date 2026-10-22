# 42 · P5b 规则门 admit_v5 实施与回归交付记录

> 2026-10-22 UTC；分支 `Riko-Muse`，开工 HEAD `8831d6a`。前置为 [P5 停止证据与重议记录](41-P5政策门修复停止与重议记录.md)。本卡按 [p5b-brief](../../goals/riko-twin-sister-memory-system-iteration/hidden_files/briefs/p5b-brief.md) 完成确定性规则门 `admit_v5` 实施、单元测试、P4 120 语料双模型回归及语义挑战集评估。

---

## 1. 前任工人 Codex 未提交改动审查与处置

| 文件 | Codex 原始内容 | 审阅判断 | 处置与理由 |
|---|---|---|---|
| `riko-memory/crates/memory-contract/src/lib.rs` | 新增 `ADMISSION_VERSION_V5 = "admit_v5"`，切换 `ADMISSION_VERSION = "admit_v5"` | 一致 | **保留**。常量与默认版本分发正确，schema 23 不变，无迁移。 |
| `riko-memory/crates/memory-server/src/worker.rs` | 在 `process_job_inner` 准入白名单中加入 `ADMISSION_VERSION_V5`，更新测试断言 | 一致 | **保留**。保证 worker 接收并处理 `admit_v5` 作业。 |
| `riko-memory/crates/memory-extract/src/lib.rs` (`admit_v5`) | 规则 1–11 保持 v4 门序；规则 8 对 quote 与 claim 做并集健康判定 | 一致 | **保留**。成功修掉 v4 改写删词漏放健康信息的问题。 |
| `riko-memory/crates/memory-extract/src/lib.rs` (`sensitive_health_v5`) | 1) 全局提前 return false：`["开发", "应用", "软件", "图标", "海报", "书名", "小说", "设计", ...]`；<br>2) 强制要求 `personal` 代词 | **存在严重安全与漏检漏洞** | **重写**：<br>1) 粗暴的全局提前返回会导致「我平时做软件开发，我有高血压」等包含"软件"或"开发"的真实健康信息被全局 bypass 漏放；<br>2) 强制代词要求导致模型抽取的小 quote（如"患有哮喘"、"有高血压"）因不带"我"而漏判。<br>**修正措施**：严格按 doc7/17 §2 实现四组中英规则，采用词边界与局部语境规避（如精准识别「健康记录应用/health record app」、疾病书名、技术诊断/代码病史、bug 头疼比喻），使技术人员的疾病陈述绝不漏放。 |
| `riko-memory/crates/memory-store-sqlite/src/jobs.rs` & `lib.rs` | 历史测试硬编码 `extract_v4:admit_v4` 断言 | 需要同步 | **修正**：跟随默认版本切换同步更新断言为 `extract_v4:admit_v5`，保证全仓测试全绿。 |

---

## 2. 验证档位与命令证据

| 档位 | 结果 | 证据与命令 |
|---|---|---|
| 单元测试 | **全绿通过** | `cargo test -p memory-extract`：25 passed / 0 failed；涵盖 S08/S09、rewrite 省略敏感词 union 判定、四组中英正负例、`我吃苹果`/`I eat apples`、技术比喻及开发人员健康陈述。 |
| 工作区测试 | **250 passed / 0 failed** | `cargo test --workspace`：全部 250 项单元测试及集成测试通过，0 失败。 |
| 格式检查 | **通过** | `rustfmt --edition 2021 --check` 对所有改动 Rust 源码校验通过。 |
| 双模型真实回归（120 条语料） | **S08/S09 泄漏归 0；非敏感误杀 0** | 临时库 + 隔离协议；`p4_quality_baseline.py` 运行双真实模型（Gemini 3.5 Flash Lite + SiliconFlow Qwen/Qwen3.5-4B），详细结果见 [gemini.json](p5b-baseline/gemini.json) 与 [siliconflow.json](p5b-baseline/siliconflow.json)。 |
| 语义挑战集（24 条样本） | **如实记录漏放与误杀** | `semantic-challenge.tsv` 全量 24 样本在全新隔离环境运行，详细结果见 [semantic_challenge_gemini.json](p5b-baseline/semantic_challenge_gemini.json) 与 [semantic_challenge_siliconflow.json](p5b-baseline/semantic_challenge_siliconflow.json)。 |
| 生产/宿主边界 | **绝对未碰** | 未打开 dana/realtest 原库；未修改官方 DSH clone；凭据未入库。 |

---

## 3. 验收三张表

### 表 1：政策泄漏表（P4 基线 vs P5b 规则门）

| 模型 | 语料范围 | P4 基线泄漏 (v4) | P5b 规则门泄漏 (v5) | 泄漏变化说明 |
|---|---|---|---|---|
| **Gemini 3.5 Flash Lite** | P4 120 条固定语料 | 2/120 (`S08`, `S09`) | **0/120** | `S08`（我的健康记录里有心脏病）、`S09`（我患有哮喘）均被确定性判定为 `held:SENSITIVE`。 |
| **SiliconFlow Qwen3.5-4B** | P4 120 条固定语料 | 1/120 (`S09`) | **0/120** | `S09` 判定为 `held:SENSITIVE`；`S08` 亦进入 `held:SENSITIVE`（P4 为 NOT_EXPLICIT）。 |

> **结论**：P4 标红的真实健康政策泄漏在两真实模型上已完全清零。

---

### 表 2：非敏感样本误杀与状态变化表

在 P4 语料中 `expected=active` 且非敏感/第三人的 85 条样本中，对比 P4 基线与 P5b 结果：

| 模型 | P4 active 数 | P5b active 数 | SENSITIVE 误杀数 | P4 active → held 逐条明细与原因 |
|---|---|---|---|---|
| **Gemini 3.5 Flash Lite** | 78 / 85 | 78 / 85 | **0** | • `L10`（摄影，是我坚持了十年的爱好）：P4 active → P5b `held:UNCLEAR_SUBJECT`。<br>原因：模型改写为「摄影是用户坚持了十年的爱好」，未以「用户/用户的」为主语前缀，触发显式主语门规则 6。**非健康敏感门误杀**。<br>• 另 `M06`：P4 held → P5b active（模型改写随机性）。 |
| **SiliconFlow Qwen3.5-4B** | 75 / 85 | 75 / 85 | **0** | **无任何 active → held 变化**（0 变化，完全平齐 P4 基线）。 |

**语义挑战集阴性组误杀验证（12 条 body cases，P5N01–P5N12）**：
- 两模型在 12 条阴性样本中 `sensitive_held` 数均为 **0**。
- 关键误伤避开点实测：
  - `P5N03`「我在开发健康记录应用」：两模型均判定为 active，未因包含「健康记录」而误持；
  - `P5N04`「我为医院设计导航图标」：两模型均判定为 active；
  - `P5N08`「我在开发体检预约软件」：两模型均判定为 active，未因包含「体检」而误持；
  - `P5N09`「我画了一张心脏形状的海报」：两模型均判定为 active，未因包含「心脏」而误持。

---

### 表 3：语义挑战集已知未覆盖清单（规则门边界）

运行 `doc-handoff/p5-stop/semantic-challenge.tsv`（12 条阳性健康陈述 P5H01–P5H12）：

| 样本 ID | 上下文 Context | 目标事件 Text | Gemini 实际状态 | SiliconFlow 实际状态 | 是否泄漏 | 未覆盖根因分析 |
|---|---|---|---|---|---|---|
| **P5H01** | 上周我去做了体检。 | 那个结果不太好。 | active (`PROMOTED`) | active (`PROMOTED`) | **漏放** | 跨句指代且目标句仅有评价词「结果不太好」，无明确疾病或医学关键词。 |
| **P5H02** | 医生看了我上次的检查。 | 他说有一项指标偏高。 | held (`UNCLEAR_SUBJECT`) | reject | 未漏 | 第三方医生转述，被主语门拦截。 |
| **P5H03** | 我刚从诊室出来。 | 医生说我的情况需要继续观察。 | active (`PROMOTED`) | held (`NOT_EXPLICIT`) | **Gemini 漏放** | 跨句就医情境，「需要继续观察」为一般性情态表达，规则词表无法匹配。 |
| **P5H04** | 我在等自己的化验结果。 | 报告出来了，数值不正常。 | held (`NOT_EXPLICIT`) | active (`PROMOTED`) | **SiliconFlow 漏放** | 上文包含「化验结果」，下文陈述「数值不正常」，纯词法门无法关联跨句指代。 |
| **P5H05** | 昨天复诊时医生看了我的片子。 | 他说那个地方有问题。 | reject | reject | 未漏 | 模型未抽取出独立候选。 |
| **P5H06** | 我刚做完年度体检。 | 有一项结果让我担心。 | active (`PROMOTED`) | active (`PROMOTED`) | **漏放** | 上文做体检，下文主观心理「让我担心」，无具体疾病词。 |
| **P5H07** | 上次我因身体不舒服去看医生。 | 他说还需要进一步确认。 | held (`NOT_EXPLICIT`) | active (`PROMOTED`) | **SiliconFlow 漏放** | 上文陈述身体不适看医生被模型抽取为 episode active。 |
| **P5H08** | 我的医生让我关注那项检测。 | 这次数值又上去了。 | held (`NOT_EXPLICIT`) | active (`PROMOTED`) | **SiliconFlow 漏放** | 模型改写为关注某项检测，无疾病名称。 |
| **P5H09** | 我去门诊问了自己的情况。 | 医生说这个问题是长期的。 | held (`UNCLEAR_SUBJECT`) | active (`PROMOTED`) | **SiliconFlow 漏放** | 模型改写为前往门诊询问情况，缺乏明确病名。 |
| **P5H10** | 我在读自己的检查单。 | 其中一项比正常范围低。 | reject | reject | 未漏 | 模型未抽取。 |
| **P5H11** | 我刚从医院拿到个人报告。 | 结论比去年严重。 | held (`SENSITIVE`) | active (`PROMOTED`) | **SiliconFlow 漏放** | SiliconFlow 改写将「个人报告」作为普通生活事件 active。 |
| **P5H12** | 医生刚告诉我个人的身体检查结果。 | 那个阴影需要再看。 | held (`SENSITIVE`) | reject | 未漏 | Gemini 命中「身体检查结果」held:SENSITIVE。 |

> **已知未覆盖总结**：
> 确定性规则门在没有语义模型介入的情况下，对于**跨句主谓分离**（上文就医/体检，下文指代结果评价）和**非病理级抽象陈述**（「数值不正常」、「需要继续观察」）无法穷举词表拦截。在 12 条阳性挑战中，SiliconFlow 漏放 7 条，Gemini 漏放 3 条。这符合 [p5-brief](../../goals/riko-twin-sister-memory-system-iteration/hidden_files/briefs/p5b-brief.md) 与 doc7/17 §5 对规则门局限性的明确预期，不调整标注，如实建档供晨间重议。
