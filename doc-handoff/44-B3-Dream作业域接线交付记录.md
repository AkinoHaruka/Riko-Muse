# 44 · B3 Dream 作业域接线交付记录

## 1. 任务背景与目标

- **任务卡**：B3 · Dream 作业域接线（13 处 DOM 常量 + CLI 17 处）
- **仓库**：`~/workspace/src/Riko-Muse`，分支：`Riko-Muse`
- **目标**：解决 Dream 作业中 13 处 `/*DOM*/` / `/*DOM:dream-job-domain-pending*/` 占位符以及 CLI 17 处主域写死问题，使 Dream 作业的记忆读写与 CLI 管理操作按指定的作业域或上下文域执行，消除域混淆问题，落实域隔离。

---

## 2. 现场核对表与行为差异

### 2.1 `dream_worker.rs` 13 处 DOM 核对表

| # | 行号（修改前基线 `81724b2`） | 所在函数 / 场景 | 修改前行为 | 修改后行为 |
|---|---|---|---|---|
| 1 | L501 | `freeze_manual_redecision` (exact recalls) | `memories_by_exact_hash(..., &user_main())` | `memories_by_exact_hash(..., &dom)`，`dom = DomainScope::for_job(&dream_job.domain_id)` |
| 2 | L509 | `freeze_manual_redecision` (exact memory 验证) | `get_memory(..., &user_main())` | `get_memory(..., &dom)` |
| 3 | L527 | `freeze_manual_redecision` (lexical recalls) | `search_memories(..., &user_main())` | `search_memories(..., &dom)` |
| 4 | L543 | `freeze_manual_redecision` (semantic recalls) | `semantic_scan(..., &user_main())` | `semantic_scan(..., &dom)` |
| 5 | L551 | `freeze_manual_redecision` (semantic memory 验证) | `get_memory(..., &user_main())` | `get_memory(..., &dom)` |
| 6 | L731 | `freeze_adjudication` (exact recalls) | `memories_by_exact_hash(..., &user_main())` | `memories_by_exact_hash(..., &dom)`，`dom = DomainScope::for_job(&dream_job.domain_id)` |
| 7 | L739 | `freeze_adjudication` (exact memory 验证) | `get_memory(..., &user_main())` | `get_memory(..., &dom)` |
| 8 | L774 | `freeze_adjudication` (lexical recalls) | `search_memories(..., &user_main())` | `search_memories(..., &dom)` |
| 9 | L792 | `freeze_adjudication` (semantic recalls) | `semantic_scan(..., &user_main())` | `semantic_scan(..., &dom)` |
| 10 | L800 | `freeze_adjudication` (semantic memory 验证) | `get_memory(..., &user_main())` | `get_memory(..., &dom)` |
| 11 | L888 | `process_adjudication` (recalls target 验证) | `get_memory(..., &user_main())` | 从父 dream job 查出 `domain_id`，构 `dom = DomainScope::for_job(&parent.domain_id)`，调用 `get_memory(..., &dom)` |
| 12 | L1129 | `process_semantic_index` (memory text 读取) | `get_memory(..., &user_main())` | 通过 `store.memory_domain_id` 查出目标记忆所在域，构 `dom = DomainScope::for_job(&d)`，调用 `get_memory(..., &dom)` |
| 13 | L1148 | `process_semantic_index` (page text 读取) | `get_page(..., &user_main())` | 通过 `store.page_domain_id` 查出目标页面所在域，构 `dom = DomainScope::for_job(&d)`，调用 `get_page(..., &dom)` |

### 2.2 CLI 17 处主域核对表（`main.rs`）

在 commit `611ef7f`（V2-S1）引入时，`main.rs` 包含 17 处 `/*DOM*/` 标记：
- **3 处为 HTTP Handler**（L6094 `purge_confirm`、L6203 `pages_list`、L6318 `pages_show`）：已在 V2-S1 接线至请求解析的 `&dom`（包含域请求头与跨域授权）。
- **14 处为 CLI 子命令**：此前写死 `DomainScope::user_main()`。本次为 10 个相关 CLI 命令添加 `--domain <domain>` 参数（缺省 `user_main`，保持向后兼容），并通过 `cli_domain_scope(&store, &scope, &domain)?` 解析生效。

| # | 行号（修改前基线 `81724b2`） | 涉及 CLI 命令 | 修改前行为 | 修改后行为 |
|---|---|---|---|---|
| 1 | L1719 | `Retire` (verify latest quote) | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 2 | L1743 | `Retire` (retire memory) | `DomainScope::user_main()` | 同上，使用 `dom` |
| 3 | L1788 | `Restore` (restore memory) | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 4 | L1812 | `PurgePreview` | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 5 | L1841 | `PurgeConfirm` | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 6 | L1909 | `MuseScan` (scan) | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 7 | L1918 | `MuseScan` (apply repair) | `DomainScope::user_main()` | 同上，使用 `dom` |
| 8 | L2241 | `Resident List` | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 9 | L2277 | `Resident Export` (list) | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 10 | L2293 | `Resident Export` (compose) | `DomainScope::user_main()` | 同上，使用 `dom` |
| 11 | L2333 | `select_consolidation_inputs` (select_resident) | `DomainScope::user_main()` | 继承传入的 `dom` |
| 12 | L2342 | `select_consolidation_inputs` (search_memories) | `DomainScope::user_main()` | 继承传入的 `dom` |
| 13 | L2501 | `Pages List` | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 14 | L2544 | `Pages Show` | `DomainScope::user_main()` | 解析 `--domain` 参数构 `cli_domain_scope` |
| 15 | L2665 | `Consolidate Enqueue` | 传递 `user_main()` 给 select inputs | 解析 `--domain` 参数传递给 `select_consolidation_inputs` |
| 16 | L6094 | HTTP `purge_confirm` | 已在 V2-S1 接线 | 保持 `&dom`（域请求头判定） |
| 17 | L6203, L6318 | HTTP `pages_list`, `pages_show` | 已在 V2-S1 接线 | 保持 `&dom`（域请求头判定） |

---

## 3. 架构与改动细节

1. **`dream_jobs` 与 `consolidation_jobs` 行模型补全**：
   - `DreamJobRow` 补充 `pub domain_id: String`（第 14 列）；
   - `map_dream_row` 读取第 13 索引 `domain_id`；
   - `dream_enqueue`、`dream_manual_redecision_enqueue` 写入目标作业域；
   - `ConsolidationJobRow` 补充 `pub domain_id: String`，`consolidation_get`、`consolidation_list` 映射 `domain_id`。
2. **`adjudication` 裁决落地继承作业域**：
   - 裁决在应用提案写入 `memories`（无论 create 还是 supersede 动作）时，此前未显式指定 `domain_id`，导致 fallback 成 `user_main` 默认值。
   - 修复：从关联的父 `dream_jobs` 查得 `domain_id`，在 `INSERT INTO memories` 时显式写入该 `domain_id`。
3. **`dream_worker` 读路径全面接线**：
   - 候选 exact / lexical / semantic 召回与校验，全部使用 `DomainScope::for_job(&dream_job.domain_id)`；
   - 异步 semantic index worker 通过 `store.memory_domain_id` / `store.page_domain_id` 获取被索引对象的所属域并构造作用域。
4. **CLI 参数扩展**：
   - `Retire`、`Restore`、`PurgePreview`、`PurgeConfirm`、`MuseScan`、`Resident(List/Export)`、`Pages(List/Show)`、`Consolidate(Enqueue)` 均增设 `#[arg(long, default_value = "user_main")] domain: String`。

---

## 4. 验证与回归结果

### 4.1 新增域隔离测试

在 `crates/memory-store-sqlite/src/v2_domain_tests.rs` 中新增 `dream_job_and_cli_domain_isolation`：
1. **主域与 side 域同一事实写入互不可见**：
   - 主域、`side_a`、`side_b` 分别写入事实；
   - 主域只能读到主域记忆，完全读不到 `side_a`、`side_b`；
   - `side_a` 可以读到主域和 `side_a`，读不到 `side_b`；
   - `side_b` 可以读到主域和 `side_b`，读不到 `side_a`。
2. **Dream 作业在作业域（`side_a`）执行与裁决入库**：
   - 领取到的 Dream 作业 `domain_id == "side_a"`；
   - 裁决落地生成的 Memory 确认 `domain_id == "side_a"`；
   - 主域 `get_memory` 与 `search_memories` 均无法访问该记忆；
   - `side_b` 也无法访问该记忆；
   - `side_a` 可通过 `get_memory` 与 `search_memories` 正常访问。

### 4.2 回归测试记录

- **Rust Workspace 全量单元与集成测试**：
  ```bash
  cd riko-memory && cargo test --workspace
  # 结果：
  # memory_server: 30 passed; 0 failed
  # memory_store_sqlite: 169 passed; 0 failed
  # 全部测试通过 (199 passed; 0 failed)
  ```
- **代码格式化检查**：
  ```bash
  cd riko-memory && rustfmt --check --edition 2021 crates/memory-server/src/dream_worker.rs crates/memory-server/src/main.rs crates/memory-store-sqlite/src/adjudication.rs crates/memory-store-sqlite/src/consolidation_jobs.rs crates/memory-store-sqlite/src/dream_jobs.rs crates/memory-store-sqlite/src/memories.rs crates/memory-store-sqlite/src/pages.rs crates/memory-store-sqlite/src/v2_domain_tests.rs
  # 退出码 0，检查通过
  ```
- **DSH 适配器测试**：
  ```bash
  cd riko-dsh && npm test
  # 结果：50 passed; 0 failed
  ```

---

## 5. 硬边界自查

- [x] 未打开 `dana/realtest` 用户生产库（仅使用内存及临时 SQLite 数据库验证）。
- [x] 未修改官方 DSH clone 源码。
- [x] 未修改冻结迁移（`0001`–`0021` 冻结不变，本卡未引入新迁移）。
- [x] 未改动 `extract_v1/v2/v3/v4` 或 `admit_v1/v2/v3/v4/v5` 的历史行为。
- [x] 无任何凭据或私密信息提交入库。
