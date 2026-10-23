# 50 · F3′ 读路径与投影索引交付记录

> 对应任务卡：任务 F3′：文件后端读路径与投影索引（Antigravity 执行）
> 实施日期：2026-10-24
> 分支：`Riko-Muse`
> 接手基线：`3112710dce393695edb5249909dcba97ae88f32a`（F2′ 交付提交）
> 状态：投影索引与向量分片落盘、全量重建协议、可见性读路径（get/explain/search/bundle）、过期直扫兜底全部就绪；验收测试全绿；全量工作区测试 314 passed / 0 failed。

---

## 1. 索引形态一句话总结

**文件存储内的投影索引由三部分构成：纯投影 SQLite 库（FTS5 词法 + CJK 二元字 + 对象元数据加速表，可随时安全删除）、文件分片向量存储与 Manifest（按 `(object_kind, object_id, version, content_sha256, model_id, dim)` 绑定）、以及记录全部规范源文件 SHA256 指纹集的索引 Manifest；指纹不符或缺失即视为过期并自动走规范文件直扫兜底。**

---

## 2. 核心架构与模块分工

### 2.1 投影索引与向量分片 (`index.rs` & `vectors.rs`)
- **投影 SQLite (`index/lexical.db`)**：
  - `fts_items`: FTS5 虚拟表，索引拉丁文 tokens（同时包含 claim 与 entry）；
  - `cjk_grams`: Unicode CJK 二元字倒排表，主键去重 `(object_id, object_kind, gram)`；
  - `projected_items`: 投影元数据缓存，仅用于加速，不作为事实真相源。
- **向量分片与 Manifest (`index/vectors/`)**：
  - `manifest.json`: 记录模型 ID、维度及各分片中条目的 `(object_id, object_kind, version, content_sha256)` 绑定元数据；
  - `shards.json`: 存储向量记录；提供 `scan_vectors` 余弦相似度扫描。
- **指纹与过期检查 (`check_freshness`)**：
  - 遍历 scope 下全部规范文件（`memory/YYYY-MM-DD.md` 日常笔记、`memory/claims/*.md` 条目、`memory/SUPPRESSIONS` 抑制表、`MEMORY.md`、`alignment.md` 等），计算当前 SHA256 指纹集并与 `index/manifest.json` 比对。
  - 任何文件的新增、修改、删除或哈希不符立即返回 `IndexFreshness::Stale`。

### 2.2 重建协议 (`ProjectionIndex::rebuild`)
- 加 scope 独占锁，删除已有 `lexical.db` 与 `manifest.json`；
- 重新顺序遍历规范文件，过滤被 forget 抑制的条目；
- 全量重建 `fts_items`、`cjk_grams` 与 `projected_items`；
- 向量对账：保留在规范源中仍然存活且哈希/版本匹配的向量记录，剔除已删除或变更的孤立记录；
- 重新计算并落盘最新来源文件指纹集 `manifest.json`；
- 返回 `IndexRebuildReport`（统计 indexed claims、entries 及保留的 vectors）。

### 2.3 读路径与可见性 (`read.rs`)
- **可见性判断 (`is_claim_visible`)**：
  - 状态：默认仅 `active` 可见；`include_history=true` 时允许 `superseded`/`expired`；`forgotten` 绝对不可见；
  - 过期：若存在 `valid_until` 且 `<= now`，默认不可见；
  - 抑制：检查条目 ID 或内容哈希是否在 `SUPPRESSIONS` 中；
  - 来源：claim 的证据引用必须在日常笔记中存在且未被抑制。
- **`explain_memory`**：
  - 从规范文件读取 claim，应用可见性与抑制过滤；
  - 来源证据逐字按 `[start_byte..end_byte]` 切片提取 quote，提供 `span_exact` 精确度标记；不做复杂逐字复核机器；
  - 映射 `speaker`（role/source_kind 映射）；`subject` 恒为 `None`，`subject_source` 为 `"none"`；
  - 读取取代双向链（`supersedes` 与 `superseded_by`）；返回稳定引用 `riko://memory/<id>`。
- **`search_memories` 混合检索与会话摘录并入**：
  - 词法道：拉丁文 FTS5 排名 + CJK 二元字覆盖度排名 + 单字有界降级；
  - 向量道：余弦相似度过滤；
  - 会话摘录（entries 语义）：直接由日常笔记的 evidence blocks 投影，作为普通检索候选并入统一流，不单独成道、不单独生命周期；
  - RRF 混合融合：按 `sum(1 / (60 + rank))` 计算全局得分，同分时 claim 优先于 entry；
  - 规范源复核：候选条目在返回前逐条经过规范源真实状态与可见性复核。
- **过期兜底（Stale Fallback 直扫）**：
  - 索引过期、缺失或损坏时，读路径自动对规范文件进行有界直接扫描与过滤；
  - 结果保证完全正确，并在返回与日志中明确标明 `degraded: true` / `"index_degraded"`。
- **Context Bundle（四段预算与真去重）**：
  - 严格保持四段固定顺序：`compact` → `alignment` → `relationships` → `retrieved`；
  - 预算控制：总上限 8000 字符（compact ≤ 1200，alignment ≤ 600，relationships ≤ 8 项）；
  - 空段保证：未配置 alignment 或 relationships 时，对应段仍然以空段结构完整呈现；
  - 真去重：在 compact 中已覆盖的条目，在检索候选阶段天然从 retrieved 段中剔除，且 `retrieved.deduped` 准确计数。

---

## 3. 服务端接线评估（停止条件核对）

- **结论**：沿用 F2′ 交付记录结论，`memory-server` 当前**尚未建立 backend 切换缝**（全局仅依赖 `memory-store-sqlite::Store`）。
- **处理**：按任务范围第 5 项硬性纪律（“没有缝就只做到存储层并在交付记录写明”），文件后端的读路径完整实现在 `memory-store-file` 存储层内，未侵入改动服务器代码，未切换服务器默认后端，保持解耦纯洁性。
- **停止条件**：未触发。

---

## 4. 验收逐项自证（五项验收全部 PASS）

原 brief 规定的验收项全部在 `memory-store-file/src/tests.rs` 中通过自动化测试覆盖并自证：

| 验收项 | 对应测试 | 验证事实与自证依据 | 结果 |
|---|---|---|---|
| **1. Golden 查询** | `golden_search_lexical_vector_and_hybrid_rrf` | 中文 CJK 词法精准命中、拉丁文 FTS5 命中、会话摘录 entry 并入命中、注入向量余弦相似度命中；多通道 RRF 融合排序与通道标记 `["lexical", "semantic"]` 均与预期一致 | **PASS** |
| **2. 删索引重建** | `rebuild_index_restores_identical_active_set_versions_sources_and_ranking` | 写入 active claims、entries 与向量后，彻底删除 `lexical.db` 与 `manifest.json`；调用 `rebuild_index` 全量重建后，命中的 active ID 集合、条目版本、来源 quote 详情、向量命中及排序稳定性与重建前逐项严格相等 | **PASS** |
| **3. 遗忘与过期隔离** | `forgotten_and_expired_items_are_invisible_in_search_explain_and_bundle` | 被 `forget` 遗忘的条目以及 `valid_until` 过期的条目，在 `search_memories`、`explain_memory`（以及 `get_memory`）、`context_bundle`（compact 与 retrieved 两处）中**均绝对不可见**；正常存活条目在三处均正常可见 | **PASS** |
| **4. Bundle 四段与去重** | `context_bundle_segments_order_budget_empty_and_deduplication` | 1. 四段顺序固定为 compact→alignment→relationships→retrieved；<br>2. alignment/relationships 为空时依然输出标准空段（`char_count=0, complete=true`）；<br>3. 总预算不超过 8000 字符；<br>4. 在 compact 中已包含的条目被 retrieved 自动去重剔除，`retrieved.deduped` 正确记录去重数 | **PASS** |
| **5. 过期兜底与直扫** | `index_stale_fallback_walks_direct_files_with_degraded_flag` | 外部直接修改规范文件造成指纹不匹配时，`check_index_freshness` 准确检测出 `Stale`；搜索读路径自动降级直扫规范源，返回正确匹配结果并明确标记 `degraded: true`；bundle 标明 `lexical_status: "index_degraded"`；调用 `rebuild_index` 后恢复为 `Fresh` 且 `degraded: false` | **PASS** |
| **全量工作区回归** | `cargo test --workspace` | 基线 308 项 + 新增 6 项 = **314 passed / 0 failed** | **PASS** |

---

## 5. 验证记录

### 5.1 命令与测试结果

```bash
export PATH="$HOME/.cargo/bin:$PATH"

# 1. memory-store-file 专项测试（24 passed / 0 failed）
cd riko-memory && cargo test -p memory-store-file

# 输出：
# running 24 tests
# test tests::atomic_kill_child ... ok
# test tests::atomic_replace_failure_before_rename_keeps_old_target ... ok
# test tests::atomic_replace_survives_process_kill ... ok
# test tests::concurrent_mixed_operations_do_not_corrupt_scope ... ok
# test tests::content_fingerprint_deduplicates_evidence_and_claims ... ok
# test tests::context_bundle_segments_order_budget_empty_and_deduplication ... ok
# test tests::direct_layout_contains_none_of_the_cut_protocol_layers ... ok
# test tests::explain_memory_returns_verbatim_spans_speaker_and_relations ... ok
# test tests::forget_claim_is_idempotent_and_does_not_bump_version_again ... ok
# test tests::forget_suppresses_reads_and_rewrites ... ok
# test tests::concurrent_scope_writers_do_not_lose_confirmed_writes ... ok
# test tests::get_overview_and_list_claims_reflect_curated_and_filter_forgotten ... ok
# test tests::forgotten_and_expired_items_are_invisible_in_search_explain_and_bundle ... ok
# test tests::golden_search_lexical_vector_and_hybrid_rrf ... ok
# test tests::index_stale_fallback_walks_direct_files_with_degraded_flag ... ok
# test tests::rejects_symlink_jump ... ok
# test tests::round_trip_evidence_claim_and_soul_preserves_body_bytes ... ok
# test tests::scopes_are_isolated_without_user_controlled_path_components ... ok
# test tests::soul_history_and_version_checks_are_direct_files ... ok
# test tests::supersede_rejects_already_superseded_or_forgotten_target ... ok
# test tests::rebuild_index_restores_identical_active_set_versions_sources_and_ranking ... ok
# test tests::supersede_updates_old_and_links_new ... ok
# test tests::token_is_only_stored_as_digest_and_agent_path_is_encoded ... ok
# test tests::suppressed_evidence_cannot_be_remembered ... ok
# test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s

# 2. 全量工作区回归测试（314 passed / 0 failed）
cargo test --workspace

# 统计：
# memory_domain: 21 passed
# memory_extract: 26 passed
# golden_v4: 2 passed
# memory_file_format (tests): 34 passed
# memory_recall: 4 passed
# memoryd: 34 passed
# memory_store_file: 24 passed
# memory_store_sqlite: 169 passed
# 总计：314 passed / 0 failed
```

---

## 6. 变更边界与纪律合规

1. **砍名单未复活**：严格无 generation / manifest 指针协议，无正式回执体系（receipts），无作业租约状态机。索引纯为投影，删除后可全量重建。
2. **数据库与服务端隔离**：未触碰真实生产数据库，未切服务器默认后端，SQLite 历史代码与 schema 保持 24 冻结。
3. **插件未碰**：未触碰 `riko-dsh`，既存未跟踪 `riko-dsh/aki_riko-dsh-0.5.2.tgz` 保持未动。
4. **提交纪律**：严禁 `git add .`，仅显式暂存属于本卡的文件。
