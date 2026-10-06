# Riko-Muse

> [Riko-Memory](https://github.com/AkinoHaruka/Riko-Memory) 记忆内核的 **Muse 增量线**：`Riko-Muse` 分支独立成库，承载依 Muse 逆向文档实施的三项内核能力（M1/M2/M3，schema 13 → 14）。

Riko-Memory 是 Rust 优先的通用 Agent 长期记忆内核：claim（声明）为原子单位，Rust 服务端 `memoryd`（SQLite + HTTP v1）是唯一裁决者，模型输出只是候选；同一用户的多个 Agent 共享记忆，不同用户按服务端 Bearer 令牌隔离。

## 背景

`Muse文档/` 是对 Muse 记忆系统的逆向分析，以及与本仓库源码（schema 13）的逐模块对照。其结论（第 12 篇 §五"合并建议"）：**不重写**——保留 Riko-Memory 已验证的体系（三级写入、admit_v2 九道准入、claim_sha256 去重、suppressed_sources 遗忘黑名单、principals 租户隔离、job 租约等），只补齐它缺的三块。本分支就是这三块：

| 卡 | 能力 | 说明 |
|---|---|---|
| **M1** | `valid_until` 到期自动转 `expired` | 15 分钟 tick 调度器 + CLI `expire-run`；CAS 转换、`memory_revisions` 审计（actor=system）、语义向量失效、索引标脏 |
| **M2** | rupture（纠正/裂痕）+ repair（修复线程） | 确定性规则匹配用户消息（`RUPTURE_CUES_V1` 冻结清单，**无 LLM**）；线程 7 天窗口归组，显式关线写审计 |
| **M3** | alignment synthesis（相处指南） | 确定性派生、版本递增、来源可溯（`source_refs_json` 记录 rupture/线程 ID）；经 compose **opt-in** 注入上下文 |

依据 Muse 文档自身的证伪结论，刻意**不做**：salience 显著度评分（已证伪）、向量检索主路径（词法 FTS5+中文二元字+RRF 优先，`semantic_vectors` 扩展位保留）、bank//MEMORY.md 等文件真相源层、Muse 的 forget plan/confirm 宿主流程（内核已有 forget + 防重放 + purge 两阶段）。完整范围冻结见 [doc7/README.md](doc7/README.md)。

## schema 14（迁移 `0014_muse_alignment.sql`）

| 表 | 用途 |
|---|---|
| `rupture_events` | 纠正事件，幂等键 `(scope, evidence_id, signal, start_byte)`，字节级来源可反查 |
| `repair_threads` | 修复线程：open/closed、归组计数、关线原因 |
| `alignment_synthesis` | 相处指南：版本化，指标对齐 Muse `hatch_alignment_dream_v1`（`rupture_turns` / `user_turns` / `correction_free_rate` / `open_repair_threads`） |
| `rupture_scan_cursors` | 扫描游标，按 `evidence_events.rowid` 单调推进 |

`0001`—`0013` 冻结不动；purge/retention 闭包已扩展到新表（删证据 → 清 rupture → 清空线程 → 清引用它们的 synthesis 版本）。

## 新增接口

```
GET  /v1/alignment/synthesis              # 最新相处指南
POST /v1/alignment/synthesis/refresh      # 按再生成策略刷新（幂等）
GET  /v1/repair/threads?status=open       # 修复线程列表
POST /v1/repair/threads/{id}/close        # 显式关线 {"reason": "…"}
GET  /v1/ruptures?limit=50                # 纠正事件（诊断）

POST /v1/context/compose                  # 既有端点，新增可选字段：
     {"include_alignment": true}          #   缺省 false，响应与旧版同键同序；
                                          #   true 时前置 <alignment_synthesis> 块
```

CLI：`memoryd expire-run --config <f>`、`memoryd muse-scan --config <f> --tenant <t> --user <u>`。

## 验证状态（档位如实分档）

| 层 | 状态 |
|---|---|
| Rust 构建/测试 | ✅ `cargo build --workspace`、`cargo test --workspace` 172 项全过（含 `muse_tests` 10 项 + `rupture` 4 项） |
| 本机冒烟 | ✅ 临时库 CLI（扫描/过期/synthesis 指标）+ HTTP（4 端点、compose 两态兼容、purge 闭包） |
| 真实模型 / 真实 DSH | ⚠️ 未验证——本批**无任何模型调用路径**，rupture 与 synthesis 均为确定性 Rust |
| DSH 适配器消费 `alignment` | ⚠️ 未接线（须现场核对 DSH seam 后另立项） |
| 用户库部署 | ⚠️ dana/realtest 原库未触碰（注意：本分支二进制会把库自动迁移到 schema 14） |

## 快速开始

```bash
cd agent-memory
cargo build --workspace && cargo test --workspace

./target/debug/memoryd serve --config <config.toml>     # 默认 127.0.0.1:8791
./target/debug/memoryd principal add --tenant <t> --user <u> \
  --token-out <file> --db <db> --migrations <migrations-dir>
./target/debug/memoryd muse-scan --config <f> --tenant <t> --user <u>
./target/debug/memoryd expire-run --config <f>
```

配置样例见 `agent-memory/config.example.toml`；模型/embedding 端点均未配置时，自动提取与语义支路不启用，M1/M2/M3 不依赖它们。

## 目录与文档

```
agent-memory/   # 内核唯一实现：crates/（Rust）、migrations/、adapters/dsh/
doc7/           # 本分支施工规范：范围冻结、数据模型与规则、施工任务卡
doc-handoff/    # 交接记录；本次交付见 24-Riko-Muse交付记录.md
doc6/、doc/     # 既有 D6 / v1 规范（Muse 增量不回改其语义）
```

- 架构与硬性规则：[AGENTS.md](AGENTS.md)
- 内核自述与运行手册：[agent-memory/README.md](agent-memory/README.md)
- 本分支范围冻结：[doc7/README.md](doc7/README.md) · 数据模型与规则：[doc7/01](doc7/01-数据模型与规则.md) · 任务卡与验证记录：[doc7/02](doc7/02-施工任务卡.md)
- 交付记录：[doc-handoff/24](doc-handoff/24-Riko-Muse交付记录.md)
