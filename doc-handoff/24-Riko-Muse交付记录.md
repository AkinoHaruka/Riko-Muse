# 24 · Riko-Muse 交付记录（doc7：M1/M2/M3）

> 日期：2026-10-06 · 分支：`Riko-Muse`（基于 main @ a870855，未提交，交付时以 `git status` 为准）
> 依据：用户决定——按 `Muse文档/`（璃对 Muse 系统的逆向 + 与本仓库源码的对照分析，第二版）实施。
> 施工规范：`doc7/README.md`、`doc7/01-数据模型与规则.md`、`doc7/02-施工任务卡.md`。

## 1. 范围（Muse文档 12 §五 合并建议的内核增量）

| 卡 | 内容 | Muse文档 依据 |
|---|---|---|
| M1 | `valid_until` 到期自动转 `expired`（调度器 + CLI + 审计 + 向量失效） | 12 §五 ➕1 |
| M2 | rupture（纠正/裂痕）事件 + repair（修复）线程（确定性规则，无 LLM） | 12 §五 ➕2、05 §5.2、08 §8.3-4 |
| M3 | alignment synthesis（相处指南）确定性派生 + compose opt-in 注入 | 12 §五 ➕3、04 §4.1、05 §5.2 |

**明确不做**（理由见 doc7/README §0）：向量检索层、bank//MEMORY.md 文件层、reason_text/memory_uri、
Muse 的 forget plan/confirm 宿主流程（内核已有 forget+suppressed_sources+purge）、 rupture 检测 LLM 化。

## 2. 改动清单

**schema 13 → 14**（`0001`—`0013` 未动；新迁移 `0014_muse_alignment.sql`）：

- `rupture_events`：确定性 cue 匹配的纠正事件，幂等键 `(scope, evidence_id, signal, start_byte)`，
  来源可反查（evidence_id + 字节 span）；`rupture_scan_cursors` 按 `evidence_events.rowid` 单调推进。
- `repair_threads`：7 天窗口归组（`REPAIR_THREAD_REGROUP_DAYS=7`，doc7/01 §1.3 冻结）；
  关线只经显式 API/CLI（actor 审计入 `audit_events`）。
- `alignment_synthesis`：版本化相处指南；指标对齐 Muse frontmatter（`rupture_turns` /
  `user_turns` / `correction_free_rate` / `open_repair_threads`）；`source_refs_json` 记录
  rupture/thread ID（「有来源派生知识」边界，AGENTS.md D6 产品边界）。

**代码**：

- `memory-contract`：`SCHEMA_VERSION=14`。
- `memory-domain/src/rupture.rs`（新）：`RUPTURE_CUES_V1`（10 条冻结清单，修改须建 V2）+
  `rupture_matches`（字节 span，同信号取最早）。纯 Rust 无 IO。
- `memory-store-sqlite/src/alignment.rs`（新）：`expire_due_memories`（CAS + memory_revisions
  `actor_kind='system'`/`reason_code='valid_until_expired'` + `stale_vectors_in_tx` +
  `mark_index_dirty`，与 retire/correct 同事务纪律）；`rupture_scan`（游标增量、INSERT OR IGNORE
  幂等、触碰线程按事实重算计数）；`alignment_synthesis_refresh`（再生成策略 doc7/01 §1.4，
  幂等）；线程列表/关线/rupture 列表/`all_scopes`。
- `memory-store-sqlite/src/purge.rs`：evidence 闭包扩展——删 rupture → 清空线程 →
  删引用被删 rupture/thread 的 synthesis 版本（SQLite `json_each`；synthesis 是纯派生物，
  与 pages purge 同待遇）。retention 复用同一闭包，自动覆盖。
- `memory-server/src/main.rs`：
  - 新路由：`GET/POST /v1/alignment/synthesis`、`GET /v1/repair/threads`、
    `POST /v1/repair/threads/{id}/close`、`GET /v1/ruptures`。
  - `POST /v1/context/compose` 新可选字段 `include_alignment`（缺省 false 时响应与旧版
    同键同序；true 时前置 `<alignment_synthesis version="N">` 块并附 `alignment` 对象）。
    v1 冻结契约以 opt-in 扩展，不回改旧语义。
  - serve 新增 15 分钟 tick Muse 调度器（expire → 逐 scope rupture 扫描 → synthesis 刷新；
    纯 Rust 无 LLM，与 retention 调度器同模式）。
  - CLI：`memoryd expire-run --config <f>`、`memoryd muse-scan --config <f> --tenant <t> --user <u>`。

## 3. 验证记录（档位：Rust 构建/测试通过 + 本机临时库 CLI/HTTP 冒烟）

| 验证 | 命令/方式 | 结果 |
|---|---|---|
| 构建 | `cargo build --workspace` | 通过（新增代码零警告；余 4 条既有警告与本次无关） |
| 测试 | `cargo test --workspace` | 172 项全过（含新增 `muse_tests` 10 项 + `memory-domain::rupture` 4 项；`d69_tests` 版本钉值 13→14 按迁移纪律同步） |
| 迁移 | 临时库 `Store::open` 自动应用 0014 | `迁移完成：schema_version=14` |
| CLI | `muse-scan` / `expire-run`（临时库） | 扫描 2 事件→1 rupture→1 线程；synthesis v2 指标正确（纠正 1/2、无纠正率 0.50） |
| HTTP | 临时库 serve + curl | GET/POST synthesis 200（幂等不重复出版本）、PUT 405、无凭证 401、threads/ruptures 200、关线 `changed:true` |
| compose 兼容 | `include_alignment` 缺省 vs true | 缺省响应键与旧版一致（5 键）；true 时多 `alignment` 键且 text 前置对齐块 |
| purge 闭包 | `muse_tests::purge_closure_…` | 删 evidence → rupture/空线程/引用它的 synthesis 版本一并清理 |

**冒烟期间发现并修复的问题**：

1. 初版建线程时 `rupture_count=0` 违反 CHECK `>0`（临时库冒烟暴露；测试先行通过的教训：
   当时 exe 未重编，详见 §5）。修复：建线程插 1，事件循环后按事实重算触碰线程计数（自愈防漂移）。
2. `muse_tests` 初版两处测试自身错误（claim 非证据连续子串触发 QuoteMismatch；synthesis
   测试漏跑 rupture_scan），已修正。

## 4. 未验证 / 未接线（不得当作已完成）

- **真实 DSH 闭环 / 真实模型连通：未验证**（本批无任何模型调用路径；rupture 与 synthesis
  均为确定性 Rust）。
- **DSH 适配器消费 `alignment`：未接线**。宿主每轮注入需改 `adapters/dsh` 的上下文组装，
  按 AGENTS.md 须现场核对 DSH seam 后另立项。
- **dana/realtest 用户库：未触碰**。`Store::open` 会自动迁移到 14；未获独立部署指令禁止用
  新二进制打开原库（先只读快照）。
- **规模/性能：未验证**（个人量级设计，无基准）。
- **环境异常记录**：冒烟期间本机回环出现两次瞬时 POST 404（空响应体，同路径稍后 200，
  受控复测 5+ 次均正常，与强杀 server 后残留套接字/本机沙箱回环相关）。生产部署如复现，
  先排除端口残留进程再查应用。

## 5. 给下一个 Harness 的注意事项

- `target/debug/memoryd.exe` 不随 `cargo test` 重编；改完 Rust 后跑 CLI/HTTP 冒烟前必须
  `cargo build --workspace`（本次 CHECK 约束假象即由此而来）。
- rupture cue 清单是冻结常量（doc7/01 §2）：改清单必须新建 `RUPTURE_CUES_V2` 并处理
  扫描游标语义，禁止原地改 V1。
- Muse 调度器 tick 为 15 分钟（tokio interval 首 tick 立即执行）；判断"为什么刚启动就扫了"
  时看这里。
- `doc7/02-施工任务卡.md` 已按本记录勾选完毕。
