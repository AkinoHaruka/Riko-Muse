# 40 · P4 抽取质量扩样与 entry 向量处置

> 2026-10-22 UTC；分支 `Riko-Muse`，开工 HEAD `315f84a`，工作区干净。规范先行：`doc7/16`。只使用全新临时库；未打开 dana/realtest 原库，未改任何冻结迁移、extract/admit prompt 或阈值。

## 1. 前置与方法

- P0.6 真实 DSH + 真实模型多轮闭环已有 [交接 36](36-P06-rc2适配实施与真实闭环验收记录.md) 的临时库实测证据；本卡开工对 Gemini 3.5 Flash Lite 和 SiliconFlow Qwen/Qwen3.5-4B 的 chat endpoint 各发一次简短探测，均 HTTP 200/1 choice。密钥只从 `~/.config/test-models/keys.env` 读取。
- 语料为 [120 条合成用户轮次](../riko-memory/tests/fixtures/p4_extract_quality.tsv)，十组各 12 条；85 条标 active、24 条 held、6 条 reject、5 条 disputed。标注依据见 [P4-LABELS](../riko-memory/tests/fixtures/P4-LABELS.md)。`claim_points` 只要求关键要点，允许明确写出的同义形式；`kind` 也核对。5 条争议项单列，不参加 115 条确定项的完整匹配率。
- [基线脚本](../riko-memory/tests/p4_quality_baseline.py) 用 `memoryd serve` + schema 23 全新临时库，每模型 15 个窗口、每窗 8 条 user 事件，现行 `extract_v4/admit_v4`，worker 真调用端点。两模型各 15/15 作业 `succeeded`，每个只尝试 1 次，`error_code=NULL`。无 mock、无手工 remember。原始逐条候选、作业版本与判定见 [Gemini JSON](p4-baseline/gemini.json) 和 [SiliconFlow JSON](p4-baseline/siliconflow.json)。临时库与 0600 key 文件由脚本退出时删除。
- 统计口径：新晋升的候选在存储层变为 `status=rejected, reason_code=PROMOTED`；复用已 active 的候选保留 `status=candidate` 并关联证据。脚本按这两种状态认作 active；其余有 held 认作 held，零候选/仅 rejected 认作 reject。首次运行输出使用了错误的 `status=candidate` 单独判法；本记录数字均用原始候选经脚本 `--recount` 重算。模型请求没有重发。首次观察后只将过严的字面锚词改为明确的同义选项（如“素食/吃素”），未改任何期望 disposition、kind 或政策样本；修订留在 fixture 提交中。

## 2. 分模型基线

| 模型 | active / 120 | held / 120 | reject / 120 | 期望 active 中命中 | 确定项完整匹配 | 政策泄漏 |
|---|---:|---:|---:|---:|---:|---:|
| Gemini 3.5 Flash Lite | **81/120 = 67.5%** | 27 | 12 | 78/85 | 99/115 | 🔴 **2**：S08、S09 |
| SiliconFlow Qwen/Qwen3.5-4B | **78/120 = 65.0%** | 31 | 11 | 75/85 | 73/115 | 🔴 **1**：S09 |

| 模型 | held 原因码（候选数；不是轮次数） |
|---|---|
| Gemini | `POSSIBLE_CONFLICT` 8，`NOT_EXPLICIT` 9，`THIRD_PARTY` 4，`TEMPORAL` 1，`SECRET` 4，`SENSITIVE` 4 |
| SiliconFlow | `POSSIBLE_CONFLICT` 9，`NOT_EXPLICIT` 6，`CONTEXT_UNCERTAIN` 8，`THIRD_PARTY` 4，`SECRET` 5，`SENSITIVE` 4 |

🔴 **政策门未过，不作“安全通过”结论。**复现条目为合成内容：S08「我的健康记录里有心脏病。」→ Gemini active claim「用户有心脏病病史」；S09「我患有哮喘。」→ 两模型均 active「用户患有哮喘」。二者未被 `SENSITIVE` 拦截。`memory-extract/src/lib.rs::sensitive_health` 目前只含「过敏/诊断/病历/疾病/吃药/服药/身份证/银行卡」有限词表；S08 的原文含“健康记录/心脏病”，rewrite 后含“病史”，S09 含“哮喘”，均未命中该词表。这是源码可定位的门限缺口，不能通过改标注抹掉。第三人组 24 个模型结果中没有 active 泄漏，但部分是模型漏抽或其他门持有，不能推断第三人门覆盖完整。

## 3. 失分归因与下一卡候选

Gemini 16 条失配：8 条 `POSSIBLE_CONFLICT`（F02/F04/F08/M01/M02/L01/L06/L07）、2 条 episode 被模型归为 fact（E01/E02）、2 条多指令宽句被 `NOT_EXPLICIT` 持有（M06/M12）、2 条秘密样本零候选而非 held（S10/S11）、2 条政策泄漏（S08/S09）。

SiliconFlow 42 条失配：9 条 `POSSIBLE_CONFLICT`（F02/F04/F08/M01/M02/M11/L01/L06/L07）；11 条 episode 误标 fact（E01/E02/E04–E12）；6 条假设/临时请求被提取后由 `CONTEXT_UNCERTAIN` 保守持有，而期望零候选（N05–N09/N12）；9 条第三人或秘密条目零候选而非 held（T05–T12/S01）；6 条漏检、rewrite 未理顺或 `NOT_EXPLICIT` 持有（E03/M06/M12/C01/C08/L09）；1 条政策泄漏（S09）。多命题 M06 在该模型只激活「用中文」，「给来源」仍 held，属于部分命题丢失。

**基线设计限制**：15 个窗口使用同一个模型专属 scope；语料故意含多种互相矛盾的合成用户资料。`POSSIBLE_CONFLICT` 这 8/9 个失配主要是共库顺序效应，不能当作真实单轮准入误杀率。后续改进评测时应隔离案例 scope/库，或将冲突项单列对照；本轮的原始基线仍可用同脚本复现，但不能据它推断总体准确率。每窗 8 条也可能给 rewrite 提供相邻案例上下文；模型生成有随机性。`active` 是保存比例，不等同于精确率、召回率或用户体验。与 doc-handoff/25 §9 的 7/15 语料不同，不做跨语料质量增益声称。

下一卡候选（本卡**未实施**）：新 `admit` 版本扩展敏感健康判定并对 S08/S09 和第三人宽句做独立政策回归，预期先把泄漏压到 0；新 `extract/rewrite` 版本处理 episode kind 与多指令拆分，预期减少 Qwen 的 kind 错分和 M06/M12 丢失；基准脚本改为单案例隔离后再比较门的误杀，预期消除共库 `POSSIBLE_CONFLICT` 干扰。以上影响都是待验证假设，不回改冻结版本。

## 4. entry 向量处置

选 **废弃**，见 [规范 16](../doc7/16-P4-抽取扩样与entry向量处置施工规范.md)。Q1 原目标是独立词法补充道；`0019` 虽允许向量表保存 `entry`，但生产未入队，worker 仅支持 memory/page（其他 kind `dead/BAD_KIND`），检索的向量扫描域谓词也仅涵盖 memory/page。补全计算与召回需增加 embedding 花费和单独质量对照，当前无增益证据。已删除 `semantic_object_fingerprint(entry)` 分支，`semantic_enqueue(entry)` 因此返回 `StateConflict`；Q1 单测改为验证拒绝与零作业。冻结 `0019` DDL 不动，兼容既有库；schema 仍为 23。entry 文档口径改为**仅词法补充道**。已有旧 entry 作业若存在仍由 worker 按 `BAD_KIND` 收尾，不产生向量。

## 5. 验证档位

| 档位 | 本卡结果 |
|---|---|
| 规范与语料 | doc7/16 先于实现提交；120 行固定 TSV、标注说明、可重跑脚本及两模型逐条 JSON 均入仓库 |
| 代码与构建 | Rust workspace 构建通过；迁移 0001–0023 未改，`SCHEMA_VERSION=23`；entry 绑定分支已移除 |
| 单元测试 | `cargo test --workspace -q` **248 passed / 0 failed**（21+24+2+4+30+167）；Q1 entry 拒绝测试通过；全仓 Rust 源码 `rg 'semantic_enqueue\([^\n]*"entry"'` 零命中 |
| 格式 | 本卡改动的 `semantic_index.rs`、`v2_q1_tests.rs` 经 `rustfmt --check --edition 2021` 通过；全仓 `cargo fmt --all -- --check` 因既有未格式化文件失败（包括未改动的 `golden_v4.rs`、`main.rs` 等），未扩大本卡修改范围 |
| 真实模型 | 两模型各 15/15 成功，真实端点与临时库；政策门 **未过**，泄漏见 §2 |
| 真实 DSH | 本卡未重跑；前置闭环历史证据见交接 36 |
| 已部署 | 否；未触碰 dana/realtest 原库或生产服务 |

## 6. 逐条失配清单

下表的 `PROMOTED` 表示已进入 active 记忆；`candidate` 表示同文 active 已存在且关联本条证据。完整 claim/quote 与其余 120 条结果见两份 JSON。

### gemini

| ID | 期望 → 实际 | 失配线索 |
|---|---|---|
| F02 | active → held | POSSIBLE_CONFLICT |
| F04 | active → held | POSSIBLE_CONFLICT |
| F08 | active → held | POSSIBLE_CONFLICT |
| E01 | active → active | PROMOTED；kind 不符 |
| E02 | active → active | PROMOTED；kind 不符 |
| M01 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失 |
| M02 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失 |
| M06 | active → held | NOT_EXPLICIT |
| M12 | active → held | NOT_EXPLICIT |
| S08 | held → active | PROMOTED；🔴 政策泄漏 |
| S09 | held → active | PROMOTED；🔴 政策泄漏 |
| S10 | held → reject | 零候选 |
| S11 | held → reject | 零候选 |
| L01 | active → held | POSSIBLE_CONFLICT |
| L06 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失 |
| L07 | active → held | POSSIBLE_CONFLICT |

### siliconflow

| ID | 期望 → 实际 | 失配线索 |
|---|---|---|
| F02 | active → held | POSSIBLE_CONFLICT |
| F04 | active → held | POSSIBLE_CONFLICT |
| F08 | active → held | POSSIBLE_CONFLICT |
| E01 | active → active | PROMOTED；kind 不符 |
| E02 | active → active | PROMOTED；kind 不符 |
| E03 | active → held | NOT_EXPLICIT |
| E04 | active → active | PROMOTED；kind 不符 |
| E05 | active → active | PROMOTED；kind 不符 |
| E06 | active → active | PROMOTED；kind 不符 |
| E07 | active → active | PROMOTED；kind 不符 |
| E08 | active → active | PROMOTED；kind 不符 |
| E09 | active → active | PROMOTED；kind 不符 |
| E10 | active → active | PROMOTED；kind 不符 |
| E11 | active → active | PROMOTED；kind 不符 |
| E12 | active → active | PROMOTED；要点缺失、kind 不符 |
| M01 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失、kind 不符 |
| M02 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失 |
| M06 | active → active | NOT_EXPLICIT,PROMOTED；要点缺失 |
| M11 | active → held | POSSIBLE_CONFLICT |
| M12 | active → held | NOT_EXPLICIT |
| N05 | reject → held | CONTEXT_UNCERTAIN |
| N06 | reject → held | CONTEXT_UNCERTAIN |
| N07 | reject → held | CONTEXT_UNCERTAIN |
| N08 | reject → held | CONTEXT_UNCERTAIN |
| N09 | reject → held | CONTEXT_UNCERTAIN |
| N12 | reject → held | CONTEXT_UNCERTAIN |
| C01 | active → held | NOT_EXPLICIT |
| C08 | active → reject | 零候选 |
| T05 | held → reject | 零候选 |
| T06 | held → reject | 零候选 |
| T07 | held → reject | 零候选 |
| T08 | held → reject | 零候选 |
| T09 | held → reject | 零候选 |
| T10 | held → reject | 零候选 |
| T11 | held → reject | 零候选 |
| T12 | held → reject | 零候选 |
| S01 | held → reject | 零候选 |
| S09 | held → active | PROMOTED；🔴 政策泄漏 |
| L01 | active → held | POSSIBLE_CONFLICT |
| L06 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失、kind 不符 |
| L07 | active → active | POSSIBLE_CONFLICT,PROMOTED；要点缺失 |
| L09 | active → held | NOT_EXPLICIT |
