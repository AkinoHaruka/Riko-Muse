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
| 05-V2-P1-精读与explain施工规范.md | V2-P1 统一可见性谓词、`memory_explain` 读模型、`riko://` 稳定引用与验收（不新增迁移） |
| 06-V2-D1-蒸馏与投影施工规范.md | V2-D1 迁移 0016、`facet_v1`/`compact_v1` 规则、失效与 purge 闭包、只读投影与验收 |
| 07-V2-R1-关系图谱施工规范.md | V2-R1 迁移 0017、`entity_v1` 投影规则（两种句式与拒绝规则）、索引/get/resolve、政策边界与验收 |
| 08-V2-B1A1-后台闭环与修复行动施工规范.md | V2-B1/A1 迁移 0018、处理账本、调度纯函数与独立开关、`rupture_v2` 分类、修复行动授权与验收 |
| 09-V2-H1-宿主闭环施工规范.md | V2-H1 bundle 分段契约与预算、适配器分段消费与回退、工具与使用纪律、现场核对的 DSH seam 事实 |

## V2 卡登记（2026-10-06，依据 Muse-V2迭代开发文档 00/06）

实施顺序 V2-00 → S1 → P1 → R1/D1 → B1/A1 → H1 → Q1；每卡动工前先在本目录补施工规范。
新增迁移从 0015 顺序递增，实际号段施工卡批准时分配（S1=0015）。

| 卡 | 交付能力 | 状态 |
|---|---|---|
| V2-00 | 基线核对、证据纠偏、doc7 依赖规范先行 | 本文件 + 04 号规范 |
| V2-S1 | 记忆域与 side-chat 隔离（迁移 0015） | **内核与 HTTP 层已实施**：168 项 workspace 测试全绿、临时库 HTTP 冒烟通过，见 [交接 26](../doc-handoff/26-V2-S1记忆域交付记录.md)。未接线：Dream 作业域（`dream_worker.rs` 13 处 `/*DOM:dream-job-domain-pending*/`）、CLI 子命令（17 处 `/*DOM*/`）。真实 DSH 闭环与真实模型未验证 |
| V2-P1 | quote/speaker 多证据读取、explain、stable_ref | **已实施**（规范 05）：统一可见性谓词、`memory_explain`、`riko://` 稳定引用、`GET /v1/memories/{id}/explain`、适配器 `memory_explain` 工具；181 项 Rust 测试 + 19 项适配器测试全绿，临时库 HTTP 冒烟通过。见 [交接 27](../doc-handoff/27-V2-P1精读与explain交付记录.md) |
| V2-R1 | people/groups 索引、别名解析、详情延迟读取、关系维护任务 | **已实施**（规范 07，迁移 0017 / schema 17）：`entity_v1` 确定性投影、索引/get/resolve 三接口、分级排序、来源失效即时屏蔽、purge 闭包、域隔离；组表建而未启用。203 项测试全绿 + 临时库 HTTP 冒烟。**人物事实完整覆盖仍受 THIRD_PARTY 门限制**，见 [交接 29](../doc-handoff/29-V2-R1关系图谱交付记录.md) |
| V2-D1 | 四分面蒸馏、compact_memory、只读 Markdown 投影 | **已实施**（规范 06，迁移 0016 / schema 16）：`facet_v1` 确定性分面、compact 双预算与 Resident 去重、来源失效即时屏蔽、purge 闭包、`/v1/compact`+`/v1/facets`+`/v1/derived/refresh`、`memoryd export` 只读投影。191 项测试全绿 + 临时库 HTTP 冒烟。见 [交接 28](../doc-handoff/28-V2-D1蒸馏与投影交付记录.md) |
| V2-B1/A1 | upkeep/Relationships/nightly 复盘/quiet pass + 复盘→修复行动 | **调度与可靠性层已实施**（规范 08，迁移 0018 / schema 18）：处理账本、四项独立开关的纯函数调度判定、Rust 侧信号计数、`rupture_v2` 目标分类、修复行动授权与复发观察。218 项测试全绿 + 临时库 HTTP 冒烟。**内容生成未实现**（需真实模型），见 [交接 30](../doc-handoff/30-V2-B1A1后台闭环交付记录.md) |
| V2-H1 | DSH bundle 分段 + 工具 + 使用纪律 | **已实施**（规范 09，无迁移）：bundle 四段 + 统一预算 + 真去重、适配器按序注入并保留旧键回退、`memory_relationships`/`memory_facets` 工具。218 Rust + 22 适配器测试全绿 + 临时库 HTTP 冒烟。**真实 DSH + 真实模型闭环未验证**，见 [交接 31](../doc-handoff/31-V2-H1宿主闭环交付记录.md) |
| V2-Q1 | entry 上下文补充、检索质量对照 | 待施工（V2 文档 04/06） |
