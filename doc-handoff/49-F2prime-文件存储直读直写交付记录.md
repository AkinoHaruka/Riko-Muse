# 49 · F2′ 文件存储直读直写交付记录

> 对应任务卡：任务 F2′（续）：文件存储直读直写（Antigravity 接手前任 Codex 半成品完成）
> 实施日期：2026-10-23
> 分支：`Riko-Muse`
> 接手基线：`4a58bbb2dbf95e5ef073ff1ba6ea21d156fc0939`（F1 交付后，工作区带有 Codex 未提交半成品）
> 状态：半成品评估完成、未完成项补齐、全量验收与回归通过；已提交并推送。

---

## 1. 半成品评估结论

前任 Codex 在额度耗尽前搭建了 `crates/memory-store-file/` 的核心主体框架。整体完成度评估约为 **85%**，核心数据对象的文件结构与原子落盘逻辑已具备雏形，但存在若干读回对称性缺失与边界逻辑缺陷。

### 1.1 已完成项
1. **模块位置与拓扑接入**：在 `riko-memory/Cargo.toml` 中添加 `crates/memory-store-file`，与 `memory-store-sqlite` 平行；依赖 `memory-domain`、`memory-file-format`、`fs2` 等。
2. **根目录与 Scope 隔离（留·简化）**：`FORMAT.toml` 全局格式声明；`principals/<digest>.toml` 仅存储 Token SHA256 摘要与映射的 Scope UUID，不存明文 token；Scope 目录由随机 UUID 命名，拒绝用户受控字符串作为路径组件；路径严格检查符号链接跳转与 `..` 越界。
3. **核心数据对象直读直写**：
   - Evidence：按日追加至 `memory/YYYY-MM-DD.md`，基于 F1 的 `DailyNote` / `EvidenceBlock`，以内容指纹去重（`AlreadyRecorded`）并校验 `(host, session, event_seq)` 冲突。
   - Claim：直接读写 `memory/claims/<id>.md`，存有 status/version/supersedes/evidence 等元数据。
   - Memory Overview：每次 claim 变更同步更新 `MEMORY.md`，经由 `MemoryOverview::parse` 校验确保语法合规。
   - Soul：直读直写 `agents/a-<hash>/SOUL.md` 与 `history/<version>.md`，支持基于 expected_version 的 CAS 乐观锁。
4. **并发与原子写入安全**：每 Scope 独立一把 `LOCK` 文件锁（`fs2::FileExt::lock_exclusive`）；文件写入使用同目录临时文件 + fsync + rename 原子替换。
5. **砍名单纯洁性**：严格未引入 generation 事务协议（无 staging/manifest/CURRENT）、无回执文件体系（receipts）、无作业租约/心跳状态机、无 tombstone 栅栏、无 pin 协议、无授权工作流、无记忆域机制。

### 1.2 未完成项（接手后补齐）
1. **读回对称性不足**：缺少 `get_overview`（读取 `MEMORY.md` Curated 总览）以及 `list_claims`（读取当前 Scope 内全部非遗忘、非抑制 claim）公有读回方法。
2. **Store 类型别名**：缺少 `pub type Store = FileStore;`，与 `memory_store_sqlite::Store` 命名形成对称。
3. **来源证据抑制检查盲区**：`remember_locked` 仅检查了提取 quote 的 hash 是否被抑制，未检查来源证据本身（`content_sha256` / `evidence_id`）是否在抑制记录中。

### 1.3 有问题项（接手后修复）
1. **`forget_claim` 幂等性缺陷**：原实现对已 forgotten 的 claim 重复调用时，未早退而是重复递增 version。已修复为幂等返回 `Ok(())`（与 SQLite `forget_memory` 行为一致）。
2. **`write_overview` 抑制过滤不全**：原实现仅按 `c.front.status == "active"` 过滤，如果存在内容被全局抑制的 claim 未过滤。已修复为在生成 `MEMORY.md` 时同步过滤 `SUPPRESSIONS` 中的条目。
3. **跨平台目录 fsync 安全**：原 `atomic_replace_with` 中包含 `File::open(parent)?.sync_all()?`，在 Windows 上对目录句柄执行 `File::open` 会报 Access Denied（os error 5）。已通过 `#[cfg(unix)]` 进行保护，符合仓库跨平台规范。
4. **`SUPPRESSIONS` 解析脆弱性**：文件末尾存在空行或空空白时，`split_once('\t')` 会报 `bad suppression record`。已补充 `.filter(|line| !line.trim().is_empty())`。
5. **`all_evidence_dir` 日期校验粗糙**：原先仅校验 `name.len() == 13 && name.ends_with(".md")`，已增加 `validate_date(&name[..10])` 严格校验。
6. **`supersede` 自去重冲突防范**：在检查 active claim 内容指纹去重时，跳过正被取代的旧 claim（`supersedes == Some(existing.id)`），防止自匹配导致状态异常。
7. **`suppressed` 判定双向匹配**：原先仅比较 `content_sha256 == hash`，已扩展为同时匹配 `object_id == key || content_sha256 == key`。

---

## 2. 模块选择与接口映射（停止条件核对）

- **模块位置**：`riko-memory/crates/memory-store-file/`。与 `memory-store-sqlite` 处于同级平行分层。
- **选择理由**：保持存储后端的清晰解耦，使文件存储与 SQLite 存储互不干扰。上层可在未来通过 feature 或构建目标切换后端，避免把文件逻辑侵入到 SQLite 历史代码中。
- **接口映射结论与停止条件**：
  - 核对 `memory-server` 当前消费的 Store 核心接口：
    - 认证/租户映射：`principal_add` / `principal_add_with_token` / `verify_token`
    - 证据追加与读取：`record_evidence` / `get_evidence` / `list_daily_evidence`
    - 记忆创建与取代：`remember` / `supersede` / `get_claim` / `list_claims` / `forget_claim`
    - Soul 人格与版本历史：`get_soul` / `upsert_soul` / `get_soul_revision`
    - 总览读取：`get_overview`
  - 核心操作能够以 1:1 的语义和入参映射到文件系统直读直写，无需任何 generation 或 staging 机器。
  - doc8/05 砍名单中已明确砍掉 memory domains、pin 协议、租约机器、回执机器；文件存储在格式上保留固定 `user_main` 元数据以兼容 F1 规范，但无复杂跨域分发。
  - **停止条件未触发**：接口未强绑 SQLite 特有不可映射语义，不需要弯曲接口迁就。

---

## 3. 验收逐项自证

原 brief 规定的验收项全部在 `memory-store-file/src/tests.rs` 中通过自动化测试覆盖并自证：

| 验收项 | 测试名称 | 验证事实与自证依据 | 结果 |
|---|---|---|---|
| **往返** | `round_trip_evidence_claim_and_soul_preserves_body_bytes` | 写入包含双空格、CRLF 和分解 Unicode `e\u{301}` 的 evidence、claim 与 Soul，读回后逐字节精确匹配（`assert_eq!`）；`MEMORY.md` 包含 Curated 引用 | **PASS** |
| **并发** | `concurrent_scope_writers_do_not_lose_confirmed_writes`<br>`concurrent_mixed_operations_do_not_corrupt_scope` | 16 个线程在 Barrier 同步后并发写入同 Scope，16 条写入均成功入库且无坏块；8 个线程混合并发写入 Evidence 与 Soul，锁机制正常生效，文件无损，写操作无丢失 | **PASS** |
| **原子性** | `atomic_replace_failure_before_rename_keeps_old_target`<br>`atomic_replace_survives_process_kill` | 1. 在 rename 前注入错误，旧文件内容完全不受影响；<br>2. 启动子进程执行原子写入并在 hook 挂起时父进程发送 `SIGKILL`（`kill -9`），验证目标文件依然保持完整的旧版本 | **PASS** |
| **去重** | `content_fingerprint_deduplicates_evidence_and_claims` | 重复提交相同内容的 evidence，第二次返回 `AlreadyRecorded(id)`；重复 remember 相同内容的 claim，第二次返回 `Dedup { memory_id, version: 1 }`，磁盘上不产生第二份 claim 文件 | **PASS** |
| **取代** | `supersede_updates_old_and_links_new`<br>`supersede_rejects_already_superseded_or_forgotten_target` | 1. supersede 后旧条目状态变为 `superseded`，version 从 1 升至 2，新条目 `supersedes` 字段指向旧条目且状态为 `active`，链可读回；<br>2. 再次对已被 superseded 或 forgotten 的条目调用 supersede 均正确返回 `VersionConflict` 拒绝 | **PASS** |
| **抑制** | `forget_suppresses_reads_and_rewrites`<br>`forget_claim_is_idempotent_and_does_not_bump_version_again`<br>`suppressed_evidence_cannot_be_remembered`<br>`get_overview_and_list_claims_reflect_curated_and_filter_forgotten` | 1. forget 后 `get_claim` 返回 `None`，再次提交同内容证据返回 `Err(StoreError::Suppressed)`；<br>2. 对已 forgotten 的条目重复调用 forget 保持幂等不重复累加版本；<br>3. 被抑制的证据禁止再次用来 remember；<br>4. `get_overview` 与 `list_claims` 自动过滤被遗忘和被抑制的条目 | **PASS** |
| **全量测试** | `cargo test --workspace` | 基线 290 项 + memory-store-file 18 项 = **308 passed / 0 failed** | **PASS** |

---

## 4. 验证记录

### 4.1 测试命令与统计

```bash
# 1. memory-store-file 定向测试（18 项全绿）
export PATH="$HOME/.cargo/bin:$PATH"
cd riko-memory && cargo test -p memory-store-file

# 输出：
# running 18 tests
# test tests::atomic_kill_child ... ok
# test tests::atomic_replace_failure_before_rename_keeps_old_target ... ok
# test tests::concurrent_mixed_operations_do_not_corrupt_scope ... ok
# test tests::atomic_replace_survives_process_kill ... ok
# test tests::content_fingerprint_deduplicates_evidence_and_claims ... ok
# test tests::direct_layout_contains_none_of_the_cut_protocol_layers ... ok
# test tests::forget_claim_is_idempotent_and_does_not_bump_version_again ... ok
# test tests::forget_suppresses_reads_and_rewrites ... ok
# test tests::get_overview_and_list_claims_reflect_curated_and_filter_forgotten ... ok
# test tests::rejects_symlink_jump ... ok
# test tests::round_trip_evidence_claim_and_soul_preserves_body_bytes ... ok
# test tests::scopes_are_isolated_without_user_controlled_path_components ... ok
# test tests::soul_history_and_version_checks_are_direct_files ... ok
# test tests::supersede_rejects_already_superseded_or_forgotten_target ... ok
# test tests::supersede_updates_old_and_links_new ... ok
# test tests::suppressed_evidence_cannot_be_remembered ... ok
# test tests::token_is_only_stored_as_digest_and_agent_path_is_encoded ... ok
# test tests::concurrent_scope_writers_do_not_lose_confirmed_writes ... ok
# test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.14s

# 2. 全量工作区测试（308 项全绿）
cargo test --workspace
# 结果：
# memory_domain: 21 passed
# memory_extract: 26 passed
# golden_v4: 2 passed
# memory_file_format (tests): 34 passed
# memory_recall: 4 passed
# memoryd: 34 passed
# memory_store_file: 18 passed
# memory_store_sqlite: 169 passed
# 总计：308 passed / 0 failed
```

---

## 5. 变更边界与合规

1. 未修改任何数据库结构（`migrations/` 未动，`SCHEMA_VERSION` 保持 21）。
2. 未修改服务器默认后端（`memory-server` 默认仍运行现有 SQLite 逻辑）。
3. 未修改 `riko-dsh`，既存未跟踪 `riko-dsh/aki_riko-dsh-0.5.2.tgz` 保持未动。
4. 严格遵守提交纪律，仅暂存属于本任务的文件。
