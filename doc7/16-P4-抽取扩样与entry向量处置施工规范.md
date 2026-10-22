# P4 · 抽取质量扩样与 entry 向量处置

> 2026-10-22；基线 schema 23。前置：P0.6 真实 DSH 闭环见交接 36；Gemini 3.5 Flash Lite 与 SiliconFlow Qwen3.5-4B 实时 chat 探测均返回 200。只使用全新临时库。

## 1. 抽取扩样契约

- 固定 120 条合成用户轮次，按四类 kind、多命题、否定/条件、自我纠正、第三人、敏感/秘密、中英混写、CJK 断句分层。每条带期望 disposition、claim 要点与标注依据；不确定项标 `disputed`，单列统计，不进入确定项准确率分母。
- `active` 表示至少一个与期望要点一致的 active 命题；`held` 表示候选保留但无 active；`reject` 表示不应形成可保存的命题（零候选或拒绝）。多命题条目逐个检查，不能因一个 active 掩盖政策泄漏。
- 用现行 `extract_v4/admit_v4`，同语料在 Gemini 3.5 Flash Lite 与 SiliconFlow Qwen3.5-4B 各跑一遍。记录每模型每条的候选、active/held/reject、held reason、失配和模型调用失败。政策门泄漏定义为标注为第三人、秘密或敏感门禁禁止内容的任何 active 命题；必须为 0 才算过门。
- 本卡只诊断，不修改任何 extract/admit prompt、门序或阈值；后续方案必须新版本另立卡。

## 2. entry 向量定案：废弃（方案 b）

`0019` 把 `semantic_vectors`/`semantic_jobs` 的 CHECK 扩成 `entry`，但 worker 的 `process_semantic_index` 只处理 memory/page，其他 kind 直接 `dead/BAD_KIND`；`semantic_scan_inner` 的域过滤也仅承认 memory/page。现有 entry 向量无生产召回入口。给 entry 增添 embedding 计算、生命周期与独立语义召回会增加模型调用、向量存储和检索验证成本；Q1 原设计是独立的词法补充道，未承诺进入 bundle。当前没有证明增益的对照结果，因此不扩建此支路。

实施规则：删除 entry 的 `semantic_object_fingerprint` 绑定分支与 Q1 入队单测，改为断言 `semantic_enqueue(entry)` 拒绝；生产不得有 entry 入队调用。保留冻结迁移 `0019` 的 CHECK 兼容旧库。无需新迁移，schema 保持 23；已有人工遗留的 entry 作业按现行 worker 收束为 `dead/BAD_KIND`，不计算向量。文档统一称 **entry 仅词法补充道**。验收 `cargo test --workspace` 全绿，全仓源码搜索无 entry 入队调用。

## 3. 输出与边界

交接报告分模型记录 active 率、held 原因、政策泄漏和逐条失配；将抽取漏检、rewrite 变形、准入门误杀、标注争议分开归因。语料只含合成文本，密钥不入仓库。不得打开 dana/realtest 原库或更改冻结迁移。
