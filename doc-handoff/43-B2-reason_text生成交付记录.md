# 43 · B2 reason_text 生成交付记录

> 2026-10-22 UTC；分支 `Riko-Muse`，前序交付为 [42 · P5b 规则门 admit_v5 实施与回归交付记录](42-P5b-规则门admit_v5实施与回归交付记录.md)。本卡按任务卡 B2 实施确定性 `reason_text` 生成、落库与查询暴露。

---

## 1. 规范与设计

### 1.1 背景与目标

在现有机制中，准入裁决仅保留 `disposition`（`active` / `held` / `reject`）与英文原因码（如 `SENSITIVE`、`NOT_EXPLICIT`），缺乏人类可读且可追溯的理由文本。Muse 的对应能力是每条记忆可追溯「为什么是这个结论」。本卡补齐这一缺口：在裁决时生成确定性 `reason_text`，持久化落库，并向现有查询端点开放。

### 1.2 生成点与模板契约

- **生成位置**：Rust 记忆内核（`crates/memory-extract` 与 `crates/memory-store-sqlite`）。
- **生成原则**：严格确定性模板，引用命中的规则号、原因码及关键证据引用切片（UTF-8 字符边界安全截断），**绝不调用 LLM 自由发挥或编造理由**。
- **裁决阶段理由（`memory-extract`）**：
  - `held:SENSITIVE` (规则 8): `[规则8:SENSITIVE] 命中健康/疾病敏感词，拦截暂存（quote: {:?}）`
  - `held:UNCLEAR_SUBJECT` (规则 6): `[规则6:UNCLEAR_SUBJECT] 主语缺失或无法明确归属（quote: {:?}）`
  - `held:NOT_EXPLICIT` (规则 7): `[规则7:NOT_EXPLICIT] 缺乏第一人称显式事实陈述标志词（quote: {:?}）`
  - `held:UNVERIFIABLE_TEMPORAL` (规则 9): `[规则9:UNVERIFIABLE_TEMPORAL] 包含不可锚定的相对时间指示词（quote: {:?}）`
  - `held:BAD_TIMESTAMP` (规则 10): `[规则10:BAD_TIMESTAMP] 时间戳格式非法或无法解析为有效 RFC3339（quote: {:?}）`
  - `held:THIRD_PARTY_STRICT` (规则 11): `[规则11:THIRD_PARTY_STRICT] 涉及第三人称且未满足严格第三人称事实准入（quote: {:?}）`
  - `rejected:SOURCE_NOT_FOUND` (规则 1): `[规则1:SOURCE_NOT_FOUND] 引用证据事件不存在或无法在会话窗口中定位`
  - `rejected:SPEAKER_NOT_USER` (规则 2): `[规则2:SPEAKER_NOT_USER] 引用证据事件发言人不是用户`
  - `rejected:QUOTE_NOT_IN_SOURCE` (规则 3): `[规则3:QUOTE_NOT_IN_SOURCE] 引用原文在对应事件正文中未找到逐字匹配`
  - `rejected:EMPTY_CLAIM` (规则 4): `[规则4:EMPTY_CLAIM] 候选记忆陈述文本为空`
  - `rejected:OVERSPECIFIED_RELATION` (规则 5): `[规则5:OVERSPECIFIED_RELATION] 实体关系定义过度指定或不符合单一关系约束`
  - `active` (常规通过): `[规则通过:ACTIVE] 显式陈述准入通过（quote: {:?}）`
- **存储阶段理由（`memory-store-sqlite`）**：
  - `PROMOTED`（准入激活）: `[准入激活:PROMOTED] 显式陈述准入通过并激活为记忆（claim: {:?}）`
  - `DUPLICATE_ACTIVE`（重复）: `[存储规则:DUPLICATE_ACTIVE] 存在相同规整化文本的 active 记忆，跳过创建（已存在记忆 ID: {}）`
  - `POSSIBLE_CONFLICT`（冲突）: `[存储规则:POSSIBLE_CONFLICT] 属性键与已有 active 记忆冲突（冲突记忆 ID: {}）`
  - `SUPPRESSED_SOURCE`（被抑制）: `[存储规则:SUPPRESSED_SOURCE] 证据已被用户主动遗忘抑制，跳过激活`
- **容错保证**：若格式化异常或失败，`reason_text` 回退到默认兜底模板，绝不阻塞准入裁决。

### 1.3 存储与版本隔离（硬边界）

- **迁移脚本**：新增 `migrations/0024_admission_reason_text.sql`（`SCHEMA_VERSION` 递增至 **24**）：
  - `ALTER TABLE memory_candidates ADD COLUMN reason_text TEXT;`
  - `ALTER TABLE memories ADD COLUMN reason_text TEXT;`
- **历史版本冻结隔离**：
  - `reason_text` 字段**仅在 `admission_version == "admit_v5"`** 写入。
  - 对于历史版本（`admit_v4`、`admit_v2`、`admit_v1`），数据库字段严格保持 `NULL`，确保历史回放行为完全不变。

### 1.4 查询端点暴露

1. **`GET /v1/candidates` (`list_candidates_http`)**：
   - 返回项中包含 `reason_text: Option<String>`，供审查暂存/拒绝原因。
2. **`GET /v1/memories/{id}/explain` (`explain_memory`)**：
   - 返回模型包含 `reason_text: Option<String>`，优先取 `memories.reason_text`，如记忆表中为空（如直接提取或迁移数据）则回退关联候选表的 `reason_text`。
3. **`GET /v1/memories/{id}` (`MemoryDetailResponse`)**：
   - 记忆详情返回中包含 `reason_text: Option<String>`。
4. **CLI `memoryd candidates show <id>`**：
   - 终端打印信息中增设 `Reason text: <text>`。

---

## 2. 验证档位与证据

| 验证档位 | 状态 | 详细证据 |
|---|---|---|
| **提取与准入单元测试** | **26 passed / 0 failed** (+2 golden) | `cargo test -p memory-extract`：新增 `b2_reason_text_generation_and_admit_v5_rules`，对 S08（心脏病）、S09（哮喘）验证命中规则 8 与敏感词截断；对 active 验证规则通过模板；对断源验证规则 1。 |
| **存储层与隔离集成测试** | **168 passed / 0 failed** | `cargo test -p memory-store-sqlite`：新增 `test_b2_reason_text_persisted_for_v5_and_null_for_v4`，验证：<br>1) S08/S09 在 `admit_v5` 下落库且 `reason_text` 包含 `[规则8:SENSITIVE]`；<br>2) active 样本落库且 `memories` 与 `explain` 均带 `[准入激活:PROMOTED]`；<br>3) 相同语料在 `admit_v4` 下落库，`reason_text` 严格为 `None`（`NULL`）。 |
| **工作区全量测试** | **全绿通过** | `cargo test --workspace`：全部通过，0 failed。 |
| **代码格式检查** | **通过** | `rustfmt --edition 2021 --check` 对所有改动 Rust 源码校验通过，exit code 0。 |
| **生产与隔离边界** | **绝对未触碰** | 1) 未打开或连接 `dana/realtest` 原库；<br>2) 未修改 `deepseek-harness/` 官方源码；<br>3) 无任何 API 凭据、私密文本进入提交。 |

---

## 3. 改动文件列表

- `riko-memory/migrations/0024_admission_reason_text.sql`：新增迁移（schema 24），加列 `reason_text`。
- `riko-memory/crates/memory-contract/src/lib.rs`：`SCHEMA_VERSION` 升级至 24。
- `riko-memory/crates/memory-extract/src/lib.rs`：`safe_truncate_chars`、`format_quote_excerpt`、`admit_v4_with_reason`、`admit_v5_with_reason`、`admit_for_with_reason` 及单元测试。
- `riko-memory/crates/memory-store-sqlite/src/diagnostics.rs`：`CandidateListItem` 与 `CandidateDetail` 扩展 `reason_text`。
- `riko-memory/crates/memory-store-sqlite/src/explain.rs`：`MemoryExplain` 扩展 `reason_text`，`memory_explain` 读取逻辑。
- `riko-memory/crates/memory-store-sqlite/src/memories.rs`：`MemoryRow` 扩展 `reason_text`。
- `riko-memory/crates/memory-store-sqlite/src/jobs.rs`：`save_candidate_with_reason`、存储冲突/重复/通过理由生成、v4/v5 版本隔离集成测试。
- `riko-memory/crates/memory-server/src/worker.rs`：worker 准入调度对接 `admit_for_with_reason` 与 `save_candidate_with_reason`。
- `riko-memory/crates/memory-server/src/main.rs`：HTTP 端点及 CLI 输出扩展 `reason_text`。
