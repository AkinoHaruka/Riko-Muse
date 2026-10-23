# 51 · F4′ 后台工序与遗忘闭包交付记录

> 对应任务卡：任务 F4′：文件线后台工序与 forget 闭包（Antigravity 执行）
> 实施日期：2026-10-24
> 分支：`Riko-Muse`
> 接手基线：`c8326891ebc564c7e671c6185854bb68832a82df`（F3′ 交付提交）
> 状态：forget 完整闭包、后台整理工序 upkeep、账本记账与幂等保障、到期物化、取代原子性复核及读写锁协议就绪；新增 6 项定向自动化测试全绿；全量工作区回归测试 **320 passed / 0 failed**。

---

## 1. 核心架构与功能落地总结

本卡遵循 `doc8/05`（收敛清单）、`doc8/06`（DeepSeek 采纳项第 7、9 条）以及砍名单纪律，在存储层与工序层落地了文件后端的完整闭包与后台工序：

1. **forget 完整闭包（文件线）**：
   - **条目状态落盘**：`status = "forgotten"`、`version += 1`、`updated_at = now()`、`reason_code = "forgotten"`；
   - **墓碑抑制全覆盖**：同时将 `memory_id`、原始内容哈希 `raw_content_hash`、规范化 claim 键 `claim_sha256`、规范化字符串哈希、以及全部来源证据的 `(evidence_id, content_sha256)` 写入 `memory/SUPPRESSIONS`；
   - **三处读不可见**：`search_memories`、`explain_memory`、`context_bundle`（以及 `get_claim` / `get_memory`）对被遗忘条目绝对不可见；
   - **派生与索引清除**：`MEMORY.md` 重新生成不含该条目；`memory/bank/*.md` 四分面重写剔除该条目；投影索引 `ProjectionIndex` 彻底清除该条目元数据、词法 tokens、CJK 二元字、关联 entry 及向量；
   - **抽取与直写拦截（双查机制）**：提供 `check_extraction_suppressed` 与 `remember` 拦截，被遗忘证据或命题在抽取候选阶段直接判为 `true` 阻断，再次直写坚决返回 `Err(StoreError::Suppressed)`。

2. **后台整理工序（确定性 upkeep）**：
   - **简单账本（已处理 evidence ID 集）**：扫描日常笔记未处理证据，排序落盘至 `memory/LEDGER`；无新证据时零写入，重跑严格幂等；
   - **物化到期**：扫描 active 条目，将 `valid_until <= now` 的条目物化更新为 `status = "expired"`，版本递增并落盘，读路径随之不可见；
   - **派生视图实时刷新与即时失效**：四分面 bank（`experience` / `opinions` / `reflections` / `world`）落盘并与存活条目一致；读取端（`get_bank_facet`）逐条复核存活源，条目在被遗忘或过期后**即时不可见**（`skipped_stale` 准确计数），无需等待刷新；
   - **无信号零动作**：在无新 evidence、无到期、无来源变化时，upkeep 进行内容指纹预判，**不修改任何文件**，文件树 SHA256 哈希完全一致。

3. **取代原子性复核（doc8/06 第 9 条）**：
   - **缺陷查实与修复**：原 F2′ 实现中 `supersede` 内部先调用了 `remember_locked` 导致新条目落盘并提前触发 overview/rebuild，随后才落盘旧条目，存在新旧条目短暂停留在“双 active”矛盾状态的不一致窗口。
   - **修复落地**：提取纯内存构建逻辑 `prepare_new_claim`；在同一把排他写锁（`lock_scope_exclusive`）内完成新旧两份文件落盘、overview 写入、bank 刷新与索引重建；
   - **单写多读锁协议**：所有读接口（`get_claim`、`search_memories`、`context_bundle`、`explain_memory` 等）均获取共享读锁（`lock_scope_shared`），读者在写锁持有期间被阻塞，释放后必然读取到完全一致的落盘快照，绝不暴露中间半状态。

---

## 2. 验收自证结果

原 Brief 规定的五项验收要求全部在 `crates/memory-store-file/src/tests.rs` 中通过自动化测试覆盖并自证：

| 验收项 | 对应测试 | 验证事实与自证依据 | 结果 |
|---|---|---|---|
| **1. forget 闭包** | `forget_closure_full_invisibility_derived_and_index_cleared_reextract_blocked` | 1. `search`、`explain`、`bundle`、`get_claim`、`get_memory` 三处读完全不可见（0 命中 / None）；<br>2. `MEMORY.md` 与 `bank/opinions.md` 派生文件不含该条目；<br>3. `index/lexical.db` 中 `projected_items`、`fts_items`、`cjk_grams` 均为 0 残留；<br>4. `check_extraction_suppressed` 判定为 true，`remember` 重复写入返回 `Err(StoreError::Suppressed)` | **PASS** |
| **2. upkeep 幂等** | `upkeep_processes_ledger_idempotently_and_second_run_is_zero_diff` | 第一次运行处理 2 条新证据并生成 `memory/LEDGER`；紧接着连续第二次运行 upkeep，`files_modified == 0`，递归计算前后 scope 目录全部文件 SHA256 哈希逐字节 100% 相同 | **PASS** |
| **3. 到期物化** | `upkeep_materializes_expired_claims_to_expired_status_and_invisible` | 构造 `valid_until` 在过去的 active 条目，执行 upkeep 后条目状态物化为 `"expired"`，版本从 1 升为 2，`reason_code` 为 `"expired"`，读路径三处均不可见 | **PASS** |
| **4. 无信号零动作** | `upkeep_zero_signal_zero_action_preserves_tree_hash` | 稳态下执行 upkeep，`new_evidence == 0`、`expired == 0`、`derived_refreshed == false`、`files_modified == 0`，前后全目录文件树哈希映射完全一致 | **PASS** |
| **5. 取代原子性复核** | `supersede_atomicity_and_consistency_no_conflicting_window`<br>`bank_derived_views_instant_invalidation_before_refresh` | 新旧条目在同一排他锁内落盘，旧条目变为 superseded 且进 reflections bank，新条目变为 active 且进入 curated 与目标分面，无矛盾窗口；来源变更后派生读即时屏蔽并准确记录 `skipped_stale` | **PASS** |
| **工作区全量回归** | `cargo test --workspace` | 基线 314 项 + 新增 6 项 = **320 passed / 0 failed** | **PASS** |

---

## 3. 详细测试记录与命令

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# 1. memory-store-file 专项测试（30 passed / 0 failed）
cd riko-memory && cargo test -p memory-store-file

# 输出截取：
# running 30 tests
# test tests::atomic_kill_child ... ok
# test tests::atomic_replace_failure_before_rename_keeps_old_target ... ok
# test tests::atomic_replace_survives_process_kill ... ok
# test tests::bank_derived_views_instant_invalidation_before_refresh ... ok
# test tests::concurrent_mixed_operations_do_not_corrupt_scope ... ok
# test tests::content_fingerprint_deduplicates_evidence_and_claims ... ok
# test tests::context_bundle_segments_order_budget_empty_and_deduplication ... ok
# test tests::direct_layout_contains_none_of_the_cut_protocol_layers ... ok
# test tests::explain_memory_returns_verbatim_spans_speaker_and_relations ... ok
# test tests::forget_claim_is_idempotent_and_does_not_bump_version_again ... ok
# test tests::forget_closure_full_invisibility_derived_and_index_cleared_reextract_blocked ... ok
# test tests::concurrent_scope_writers_do_not_lose_confirmed_writes ... ok
# test tests::forget_suppresses_reads_and_rewrites ... ok
# test tests::forgotten_and_expired_items_are_invisible_in_search_explain_and_bundle ... ok
# test tests::get_overview_and_list_claims_reflect_curated_and_filter_forgotten ... ok
# test tests::golden_search_lexical_vector_and_hybrid_rrf ... ok
# test tests::index_stale_fallback_walks_direct_files_with_degraded_flag ... ok
# test tests::rebuild_index_restores_identical_active_set_versions_sources_and_ranking ... ok
# test tests::rejects_symlink_jump ... ok
# test tests::round_trip_evidence_claim_and_soul_preserves_body_bytes ... ok
# test tests::scopes_are_isolated_without_user_controlled_path_components ... ok
# test tests::soul_history_and_version_checks_are_direct_files ... ok
# test tests::supersede_atomicity_and_consistency_no_conflicting_window ... ok
# test tests::supersede_rejects_already_superseded_or_forgotten_target ... ok
# test tests::supersede_updates_old_and_links_new ... ok
# test tests::suppressed_evidence_cannot_be_remembered ... ok
# test tests::token_is_only_stored_as_digest_and_agent_path_is_encoded ... ok
# test tests::upkeep_materializes_expired_claims_to_expired_status_and_invisible ... ok
# test tests::upkeep_processes_ledger_idempotently_and_second_run_is_zero_diff ... ok
# test tests::upkeep_zero_signal_zero_action_preserves_tree_hash ... ok
# test result: ok. 30 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.95s

# 2. 全量工作区回归测试（320 passed / 0 failed）
cargo test --workspace
# 统计：
# memory_domain: 21 passed
# memory_extract: 26 passed
# golden_v4: 2 passed
# memory_file_format (tests): 34 passed
# memory_recall: 4 passed
# memoryd: 34 passed
# memory_store_file: 30 passed
# memory_store_sqlite: 169 passed
# 总计：320 passed / 0 failed
```

---

## 4. 纪律核对与停止条件

1. **砍名单不许复活**：
   - 无作业租约/心跳状态机；
   - 无 mutation receipts 文件；
   - 无记忆域与跨域授权层；
   - 账本仅用单文件 `memory/LEDGER` 记录已处理 evidence ID 集合。
2. **数据库与服务端隔离**：
   - 未改写 `migrations/`，现网 SQLite 内核保持 schema 24 冻结；
   - 未触碰真实生产数据库；
   - 未修改 `memory-server` 默认后端。
3. **插件隔离**：
   - 未改动 `riko-dsh`，既存未跟踪 `riko-dsh/aki_riko-dsh-0.5.2.tgz` 保持未动。
4. **停止条件**：
   - 现行准入、取代、遗忘与后台整理工序在文件形态下均完整映射并闭环，未触发任何停止条件。
