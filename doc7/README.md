# doc7 · Riko-Muse 施工规范（Muse 增量能力）

> 立项：2026-10-06 · 分支：`Riko-Muse`
> 依据：用户明确决定——按 `Muse文档/`（璃对 Muse 记忆系统的逆向分析 + 与本仓库 Riko-Memory
> 源码的逐模块对照，第二版）实施。Muse文档 的合并建议见其 `12-原项目Riko-Memory逐模块对照分析.md §五`。
> 2026-10-06 增补：Muse-V2 迭代（`../Muse-V2迭代开发文档/`，证据核实修订版）立项，V2 前缀
> 逐卡施工；V2-S1 施工规范见 `04-V2-S1-记忆域施工规范.md`。doc7 的 M4 已是 extract_v4，
> V2 卡不复用 M 编号。

> **当前状态（2026-10-06 晚）**：V2-S1 记忆域内核与 HTTP 层已实施并通过离线测试与临时库
> HTTP 冒烟，详见 [doc-handoff/26](../doc-handoff/26-V2-S1记忆域交付记录.md)。该记录同时列出
> 接手时上一轮中断造成的 208 个编译错误、修出的 5 个真实缺陷、一次文件损坏事故与逐行一致的恢复验证，
> 以及尚未接线的 Dream 作业域与 CLI 边界。

## 0. 定位与范围冻结

本仓库 `agent-memory/` 即 Muse文档 中的"主人原项目 Riko-Memory"（schema v13）。Muse文档
的结论（12 §五）是**在现有内核上补增量**，不是重写。本规范把增量冻结为三张卡：

| 卡 | 内容 | Muse文档 依据 |
|---|---|---|
| M1 | `valid_until` 到期自动转 `expired`（定时扫描 + 审计 + 向量失效） | 12 §五 ➕1、02 §2.2 |
| M2 | rupture（纠正/裂痕）事件 + repair（修复）线程 | 12 §五 ➕2、05 §5.2、06、08 §8.3-4 |
| M3 | ALIGNMENT_SYNTHESIS 式"相处指南"派生与上下文注入 | 12 §五 ➕3、04 §4.1、05 §5.2 |

**明确暂缓（不在本批实施，后续按痛点再立项）**：

- 向量检索层（Muse文档 11 §C.4：先词法，"等词法出现真实痛点再加"；本仓库 recall 已是
  FTS5+二元字+RRF，`semantic_vectors` 扩展位已存在）。
- bank/ 分面蒸馏、MEMORY.md/日期日志等文件真相源层（12 §五未列入合并建议；Muse文档 8.1
  自注"文件真相源思想可要可不要"；内核保持 DB 单一事实源，文件渲染层属宿主侧）。
- `reason_text` / `memory_uri`（08 §8.2 仅 P2，12 §五未采纳；后续需要时以 0015+ 迁移补列）。
- Muse 的 forget plan/confirm/pending.json 流程（内核已有 forget + `suppressed_sources`
  防重放 + purge 两阶段 + 墓碑，Muse文档 06 §6.3.2b 判定"主人的方案多一道防线"；宿主侧
  plan/confirm 属 DSH 技能层，另行立项）。
- rupture 检测的 LLM 化（Muse 靠 nightly dream 的 LLM 判断；本批按 08 §8.3-4 用确定性
  规则匹配，不触发 LLM，符合 AGENTS.md「普通 turn/end/flush 不触发 LLM」边界）。

## 1. 文档地图

| 文档 | 内容 |
|---|---|
| README.md | 本文件：定位、范围冻结 |
| 01-数据模型与规则.md | 迁移 0014 全部 DDL、rupture 规则、线程归组、synthesis 派生、purge 闭包扩展 |
| 02-施工任务卡.md | M1/M2/M3 逐卡验收标准与验证档位 |
| 04-V2-S1-记忆域施工规范.md | V2-S1 迁移 0015、域解析/授权矩阵、存储层契约与验收（其余 V2 卡施工时先补规范） |

## V2 卡登记（2026-10-06，依据 Muse-V2迭代开发文档 00/06）

实施顺序 V2-00 → S1 → P1 → R1/D1 → B1/A1 → H1 → Q1；每卡动工前先在本目录补施工规范。
新增迁移从 0015 顺序递增，实际号段施工卡批准时分配（S1=0015）。

| 卡 | 交付能力 | 状态 |
|---|---|---|
| V2-00 | 基线核对、证据纠偏、doc7 依赖规范先行 | 本文件 + 04 号规范 |
| V2-S1 | 记忆域与 side-chat 隔离（迁移 0015） | **内核与 HTTP 层已实施**：168 项 workspace 测试全绿、临时库 HTTP 冒烟通过，见 [交接 26](../doc-handoff/26-V2-S1记忆域交付记录.md)。未接线：Dream 作业域（`dream_worker.rs` 13 处 `/*DOM:dream-job-domain-pending*/`）、CLI 子命令（17 处 `/*DOM*/`）。真实 DSH 闭环与真实模型未验证 |
| V2-P1 | quote/speaker 多证据读取、explain、stable_ref | 待施工（V2 文档 04） |
| V2-R1 | people/groups 索引、别名解析、详情延迟读取、关系维护任务 | 待施工（V2 文档 01） |
| V2-D1 | 四分面蒸馏、compact_memory、只读 Markdown 投影 | 待施工（V2 文档 03） |
| V2-B1/A1 | upkeep/Relationships/nightly 复盘/quiet pass + 复盘→修复行动 | 待施工（V2 文档 08） |
| V2-H1 | DSH bundle 分段 + 工具 + 使用纪律 | 待施工（V2 文档 08） |
| V2-Q1 | entry 上下文补充、检索质量对照 | 待施工（V2 文档 04/06） |
