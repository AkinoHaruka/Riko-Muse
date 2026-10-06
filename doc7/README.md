# doc7 · Riko-Muse 施工规范（Muse 增量能力）

> 立项：2026-10-06 · 分支：`Riko-Muse`
> 依据：用户明确决定——按 `Muse文档/`（璃对 Muse 记忆系统的逆向分析 + 与本仓库 Riko-Memory
> 源码的逐模块对照，第二版）实施。Muse文档 的合并建议见其 `12-原项目Riko-Memory逐模块对照分析.md §五`。

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
