# Agent-Memory 实施交接说明（给下一个 Harness）

> **V2-S1 记忆域（2026-10-06 晚，分支 `Riko-Muse`）**：V2 迭代第一张卡 V2-S1 已实施——迁移 `0015_memory_domains.sql`（schema 15）、域注册/会话绑定/跨域授权/证据域映射存储层、`DomainScope` 读域集闭包、`[domains] enabled` 开关、7 个 `/v1/domains*` 管理端点、写路径与读路径接线，以及 8 项 `v2_domain_tests`。`cargo test --workspace` **168 passed / 0 failed**；临时库 HTTP 冒烟确认「主域猜 ID 读不到 side、授权后才可跨读、关闭域 409」。完整证据、接手时 208 个编译错误的修复过程、5 个真实缺陷与一次文件损坏事故的恢复验证见 [26](26-V2-S1记忆域交付记录.md)。**未接线**：Dream 作业域（`dream_worker.rs` 13 处标记）、CLI 子命令（17 处标记）；真实 DSH 闭环、真实模型、部署均未验证。dana/realtest 原库未打开、未升级。
>
> **Riko-Muse M1—M4（2026-10-06，分支 `Riko-Muse`）**：M1—M3（schema 14、迁移 0014、expire/rupture/repair/alignment）见 [24](24-Riko-Muse交付记录.md)；M4 `extract_v4/admit_v4` rewrite 已在 HEAD `8c02a68` 实施，同语料 Gemini 观察从 v3 的 0/14 active 到 v4 的 7/15 active，含检索/注入验证，详见 [25 §9](25-Riko-Muse真实模型验收.md)。该样本不代表整体记忆质量验收。真实 DSH 宿主闭环及 adapter 消费 alignment 仍未接线。当前工作树另有 Bridge `0.1.2` 修改，尚未验证或部署。

> **当前实施更新（2026-09-29）**：D6-11—D6-15 已实施并提交，schema 13。当前 Rust workspace `fmt/test/build` 均通过；适配器 `npm test` 20/20 通过。官方 DSH + 本地固定响应验证了受限 Dream child 的提取/裁决，以及主题页创建和基于已读页面版本的更新。Android 模型设置 Bridge `0.1.1` 已部署到生产 DSH `web` profile；补齐 profile 配置后重启了 DSH runtime。Android 模拟器现已通过认证连接线上 Riko preset，读取 health、model catalog、model-settings 与提供商状态成功。未调用模型 API；尚未验证 Android 中密钥写入、自定义提供商保存/发现后实际发起模型对话。dana/realtest 原库未触碰。详见 [23](23-Android-Bridge生产部署与连通验证.md)。

> **DSH 0.2 适配（2026-09-29）**：原本机官方 clone 有大量既存删除，已完整移至 `%LOCALAPPDATA%\Temp\deepseek-harness-prior-20260929` 保留；`deepseek-harness/` 已从官方仓库重新克隆为 `0.2.0-rc.2 / 639ed015397290b3745d163aafe02ffee4aa3f84`，工作树干净。`pnpm install --ignore-scripts` 与 `build:lib:host` 通过。Riko Memory 与 Riko-App Bridge 已在隔离 profile 安装，`--dump-config` 显示两个 bundle；官方 DSH rc.2 Web 实际启动，Bridge health 返回 200，Riko preset session create/list 成功。Memory adapter 随 preset 挂载，但未发送 prompt，因此 L0 事件写入、记忆召回/注入和模型行为未验证。全量 `pnpm run build` 的 Web Vite/esbuild 步骤遇到 Windows `Access is denied`，Web UI 全量构建未验证。Bridge health 响应仍报告旧的 `0.2.0-rc.1` 元数据版本。未调用模型 API，未操作服务器。完整记录见 [21](21-DSH-0.2适配.md) 与 [22](22-DSH-rc2本机安装验证.md)。

> **真实模型质量测试背景（2026-09-28，D6-11—15 开工前）**：D6 规范差距修复已按五组实施；当时 schema 11。受控 OpenRouter + 官方 DSH Dream 单样本已走到 Rust adjudication 和 Active memory；Gemini 真实响应解码与 3072 维向量保存通过。取消 bundle/query 固定延迟预算后，以 `gemini-embedding-001` 对同一合成零词法 query 复测 1 次：端到端 1084 ms，`semantic_status=ok`，目标以 `reason=semantic` 进入 retrieved。它证明旧 800 ms 截止不再提前降级该样本，不代表整体语义质量或正确沉默已通过。请求仍受 `embedding_timeout_secs`（默认 30 秒）保护；DSH bundle hook 跟随调用取消。该阶段未调用 OpenRouter/Chat，真实模型检索质量仍需更多有对照样本。完整证据见 [17](17-D6真实模型端到端验证.md) §8.9；首次失败与保护修复见 [17](17-D6真实模型端到端验证.md) §1—7 和 [18](18-D6真实模型验证保护修复.md)。dana/realtest 原库未触碰、未升级。

交接时间：2026-09-25 上午（本机 GMT+8）。
> **进展更新（2026-09-25 下午）**：V2-3 注入复验、纠错/遗忘/取记忆宿主回路、跨用户 404、spool 离线恢复、V2-6 交付记录均已完成（含三项真实缺陷修复，见 [04](04-V2-6交付记录.md)）。03 的待办已清空。
> **真实模型验证（2026-09-25 晚）**：两条模型链路（提取 worker / DSH 会话）已接真实模型实测通过，「模型真实连通」不再是未验证项，见 [05](05-真实模型验证.md)。
> **OpenRouter + DSH 真实模型验证（2026-09-27）**：OpenRouter chat 单样本直连通过；全新 schema 11 临时库上，官方 DSH headless + OpenRouter 实际回答出只存在于 Resident 的合成代号，并遵循 Soul 格式；L0 spool 5/5 获得 receipt。全量 Rust/TS 回归通过。真实 Dream 子 Agent/准入裁决、memoryd embedding 集成与整体语义质量仍未验证，见 [16](16-真实模型小范围验证.md) §7—8。
> **Dream/embedding 真实集成尝试（2026-09-27）**：官方 DSH 用 `模型.txt` 中的 OpenRouter 模型发起了 1 次真实 Dream child 请求，Rust 持久化 1 条候选；Gemini embedding 阶段返回通用 `EMBEDDING_UNAVAILABLE`，未创建 adjudication job 或 Active memory。job generation/attempt 达到 14，发现 5 秒级自动重领；memoryd 与代理已停止。此次不算 Dream 闭环、语义检索或 embedding 接线通过。详见 [17](17-D6真实模型端到端验证.md)。
> **续验保护修复（2026-09-27）**：`dream.enabled=false` 现在仅停 Auto Dream 定时调度，显式 trigger 可由合法 runner 执行；provider 状态按安全类别记录，Embedding 失败进入 24 小时 `provider_wait`，避免快速重领。`cargo fmt --check`、`cargo test --workspace`（125 passed）、`cargo build --workspace` 通过；未做新的外网调用。详见 [18](18-D6真实模型验证保护修复.md)。
> **真实 Dream/embedding 续验（2026-09-27，历史实测）**：只使用 `模型.txt` 中的 OpenRouter 与 Gemini 模型，限额分别 8/8、5/5，全部 provider HTTP 200。Dream child→candidate→Rust adjudication→Active 单样本通过；首次 Google embedding 响应省略 `index`，现已修 decoder；实际 3072 维向量保存成功。零词法查询在当时的 800 ms 预算下降级且目标未召回。用户随后决定取消固定查询延迟预算；按 provider 配置超时等待的改动与新状态见 [17 §8.8](17-D6真实模型端到端验证.md)，未再调用真实模型。semantic job 与 L1 应用已同事务提交；复核后增加模型切换队列隔离、supersede 时旧索引失效和入队失败原子回滚覆盖。随后用官方 DSH + 本地固定响应端点复验了修复后的 apply→semantic enqueue→ready vector 路由；它不算真实模型复验。先前 130 项 workspace 测试、fmt 与 build 通过；延迟预算改动后的检查状态以最新交接记录为准。详见 [17 §8](17-D6真实模型端到端验证.md)。
> **无固定延迟预算后的零词法 query 复验（2026-09-28）**：用户明确要求继续后，仅新增 1 次 Gemini Embedding 请求。隔离 schema 11 synthetic 副本中的 Active 目标已有 `gemini-embedding-001 / 3072 / ready` 向量；相同零词法 query 返回 200，1084 ms，`semantic_status=ok`，目标以 `reason=semantic` 命中。OpenRouter/Chat 未调用。临时 key/token 已清理；工具输出曾意外显示未标注凭据行，已通知用户需轮换。详见 [17 §8.9](17-D6真实模型端到端验证.md)。
> **写入与召回质量探针（2026-09-28）**：轮换密钥后，本轮新增 Gemini 3 次、OpenRouter 5 次真实请求，均 HTTP 200。Gemini 对 2 个相关中文改写均召回目标，但 1 个无关自行车问题也误召回；离线词法探针发现英文短 gram 误召回及中文相关 query 对英文 claim 漏召回。OpenRouter Dream child 保存了 1 条 span 精确候选，但临时代理故障使 adjudication 未完成；不计作 Active 写入通过。整体写入/召回质量仍未验收，答案利用率未测。见 [19](19-D6记忆写入与召回质量探针.md)。
> **在线向量召回门槛（2026-09-28，确定性实现）**：`/v1/context/bundle` 的 query 向量候选现在在 top-K/RRF 前按 `similarity >= semantic_min_similarity` 过滤，默认 0.3；Dream 写入期语义候选扫描不变。阈值参考两个上游的初始配置值，非本项目质量校准结论。`cargo fmt --all -- --check`、`cargo test --workspace`（memory-store-sqlite 80 项全过）和 `cargo build --workspace` 通过；没有调用真实 API。尚未用 Gemini 重跑 19 号记录中的无关自行车 query，不能声称 E12 已通过；真实语义质量仍未验收。
> **0.3 门槛后的 Gemini query 复验（2026-09-28）**：同一隔离 synthetic 目标上，2 个相关中文改写均以 `reason=semantic` 命中；无关自行车问题仍以 `reason=semantic` 命中，E12 继续失败。新增 Gemini 4 次请求（3 次 memoryd bundle + 1 次分数 batch；无重试、未见 429/鉴权错误）。批量响应的 `index` 字段混杂，未能计算余弦分数；不据此调整阈值。详见 [19 §8](19-D6记忆写入与召回质量探针.md)。

这份目录只做交接，不改规范。规范来源优先级以仓库根 `AGENTS.md` 为准：D6 新能力以 `doc6/` 为规范；DSH 接线以每次现场核对的官方源码为准，冲突先记录再更新文档。

## 阅读顺序

| 文件 | 用途 |
|---|---|
| [01 环境与复现](01-环境与复现手册.md) | 本机已装好的一切：路径、构建状态、profile、patch、mock、环境变量、故障定位 |
| [02 已完成与证据](02-已完成与证据.md) | 每张卡改了什么、验证到什么程度（区分「代码存在／构建通过／配置预览／真实 DSH 闭环」） |
| [03 待办与已知冲突](03-待办与已知冲突.md) | 剩下的活、最后一个失败现象与假设、文档冲突、禁区 |
| [08 收口复验](08-收口复验记录.md) | doc4 收口：真实 DSH 宿主复验、dana 库副本迁移 0003 演练、定向源码复核发现与分类 |
| [09 D5 交付记录](09-D5交付记录.md) | doc5 记忆质量规则实施（D5-0—D5-6）、迁移 0004 副本演练、HTTP 冒烟、离线召回探针 |
| [10 D5 真实模型验证](10-D5真实模型验证.md) | 真实 DSH 闭环复验（mock）+ 真实模型首轮观察（extract_v3/admit_v2 真实候选分布、remember 拒绝的宿主表现、召回探针） |
| [11 决策 A/B 实施与凭据闸门设计](11-决策AB实施与凭据闸门设计.md) | 决策 A 端点窄映射（ea23c29）、决策 B 直写单命题粒度门（4cb7eb5）实施与验收；保存凭据闸门源码核对与最小设计提案 |
| [12 直写护栏解除](12-直写护栏解除.md) | **最新事实**：用户产品决定（2026-09-25 深夜）——直写内容护栏全部解除（四类门/相邻指令/复合命题门），Agent 可主动写入；保留协议级校验；护栏待用户统一重写 |
| [13 D6-0 源码核对与运行补充](13-D6源码核对.md) | 官方 DSH 源码基线、运行时 compact 事件、prompt/子 Agent seam 和本轮宿主复验 |
| [14 D6 原始交付记录](14-D6交付记录.md) | D6-0—D6-10 首轮交付时点记录；其中过时结论由 15 号记录覆盖 |
| [15 D6 规范差距修复](15-D6规范差距修复.md) | 当前五组提交、C01—C09 修复结果、E01—E25 验收矩阵、验证边界 |
| [16 D6 真实模型小范围验证](16-真实模型小范围验证.md) | Gemini/SiliconFlow 历史 API 探针，以及 OpenRouter + 官方 DSH 本轮真实调用和当前未验证边界 |
| [17 D6 真实模型端到端验证](17-D6真实模型端到端验证.md) | Dream child → Rust candidate 的真实 OpenRouter 部分链路；Gemini embedding 阶段失败、自动重领与限流边界 |
| [18 D6 真实模型续验保护修复](18-D6真实模型验证保护修复.md) | runner 开关语义、provider 错误分类/冷却、本地验收与续验门槛 |
| [19 D6 记忆写入与召回质量探针](19-D6记忆写入与召回质量探针.md) | 写入候选忠实度、词法/语义召回命中与误召回；区分本轮实测、历史样本和未完成链路 |
| [20 D6-11—D6-15 交付记录](20-D6-11-15交付记录.md) | 受限 Dream child、job-scoped 只读、页面创建/更新、FTS/grams 修复、本机与官方 DSH 固定响应验收 |
| [21 DSH 0.2 适配](21-DSH-0.2适配.md) | 本地 clone 更新、bundle 兼容范围、公开发布、隔离 profile 安装与组合配置验收 |
| [22 DSH rc.2 本机安装验证](22-DSH-rc2本机安装验证.md) | 官方 clone 重克隆、rc.2 构建、隔离安装、Web/Bridge/session 实测与边界 |
| [23 Android Bridge 生产部署与连通验证](23-Android-Bridge生产部署与连通验证.md) | 生产 DSH profile 更新、缺失配置修复、回滚备份与 Android 模拟器连接验证 |
| [24 Riko-Muse 交付记录](24-Riko-Muse交付记录.md) | doc7（M1/M2/M3）交付：迁移 0014、rupture/repair、alignment synthesis、compose opt-in、purge 闭包扩展、验证档位与未接线边界 |
| [25 Riko-Muse 真实模型验收](25-Riko-Muse真实模型验收.md) | 三轮真实模型观察；含 extract_v3 的 0-active 问题、extract_v4 的同语料 7/15 active 结果、保护门核对与清理记录 |
| [26 V2-S1 记忆域交付记录](26-V2-S1记忆域交付记录.md) | **最新**：接手上一轮中断（208 编译错误）后的修复、V2-S1 内核与 HTTP 层实施、5 个真实缺陷、168 项测试与临时库 HTTP 冒烟证据、未接线边界 |

## 30 秒状态

```
根仓库 branch/HEAD：开工现场检查；本次观察时为 `Riko-Muse` / `611ef7f`（V2-S1 交付提交）。官方 DSH HEAD `639ed015397290b3745d163aafe02ffee4aa3f84`（0.2.0-rc.2）。
当前实现：schema 15。Muse M1—M4 已提交（详见 [24](24-Riko-Muse交付记录.md)、[25 §9](25-Riko-Muse真实模型验收.md)）；**V2-S1 记忆域已实施并提交（`611ef7f`）**，见 [26](26-V2-S1记忆域交付记录.md)。Bridge `0.1.2` 正在工作树修改中，尚未验证/部署；生产仍为 `0.1.1`，状态和边界见 [23](23-Android-Bridge生产部署与连通验证.md)。
验证：`cargo test --workspace` **168 passed / 0 failed**（含 V2-S1 的 8 项 `v2_domain_tests`）；`cargo fmt --all`、`cargo build --workspace` 通过；V2-S1 另有临时库 HTTP 冒烟（域管理端点、域头读、绑定写路径、授权跨读）。历史档位：M1—M3 的 172 项测试见 [24](24-Riko-Muse交付记录.md)，M4 的 156 项记录见 [25 §9](25-Riko-Muse真实模型验收.md)。未验证：真实 DSH 宿主闭环、adapter 对 alignment/域选择器的消费、Dream 作业域接线、整体记忆质量、规模性能、dana/realtest 用户库升级和 memoryd 生产部署。dana/realtest 原库未打开、未升级。
数据库边界：dana/realtest 原库未打开或升级；任何迁移演练只用临时库/副本
用户工作区：官方 `deepseek-harness/` 已重克隆为干净 rc.2 clone，保持未跟踪/只读；原有带删除状态的 clone 完整保留在 `%LOCALAPPDATA%\Temp\deepseek-harness-prior-20260929`。`MiMo-Code/`、`doc2/`、`doc3/`、`doc5/` 仍为本地参考目录，不纳入本项目提交。Riko-App 根目录不是 Git 仓库，其本地 Android 源码不在本仓库提交范围内；其 debug APK 已安装于 `emulator-5554` 并成功连通生产 Bridge（详细边界见 23）。
```

## 最重要的三条禁区

1. **不要 `git add .` 或递归暂存根目录** —— `deepseek-harness/` 是官方 clone（未跟踪、只读参考），`doc2/` 只在确认时单独暂存。
2. **不要改官方 clone 与三个上游仓库**（`EverOS/`、`hindsight/`、`tencentdb-agent-memory/`）。本机 clone 只做过 `pnpm install --ignore-scripts` 与 `build:lib:host`，并在根 `.npmrc` 追加了 `verify-deps-before-run=false`（本机便利项，不要提交）。
3. **不要把 mock 模型的运行结果写成「真实模型连通」**。本地 mock 只证明宿主回路；本轮真实端点探针证据见 16，但不代表 memoryd/DSH 已接入真实模型或质量达标。
