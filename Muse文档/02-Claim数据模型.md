# 02 · Claim 数据模型（第二版）

> Claim 是这套记忆系统的原子单位。本文件以 Muse 的 PostgreSQL schema 实测为默认（X 为准），
> 以主人 Riko-Memory 源码实测填补未解部分（Y 填空）。标签说明见 00。

## 2.1 Muse 的 memory.claims 表 [实测]

| 字段 | 类型 | 说明 |
|---|---|---|
| `claim_id` | text PK | 本地行标识。日志里的 `7a0d689f…` 形如 32 位 hex |
| `run_id` | text | 哪次运行产生的 |
| `kind` | text | 类型。观测值：`fact` |
| `salience` | text | 显著度。观测值：`medium` |
| `claim_text` | text | claim 正文 |
| `quote` | text 可空 | 用户原话引用 |
| `speaker` | text | 说话人 |
| `evidence_handles` | jsonb | 证据句柄数组 |
| `supersedes_claim_id` | text 可空 | 版本链：本条覆盖了哪条旧 claim |
| `status` | text，默认 `'active'` | 生命周期状态 |
| `confidence` | double | 置信度浮点数 |
| `first_seen` | timestamptz | 首次获知时间 |
| `reinforced_at` | timestamptz | 最近一次被强化的时间 |
| `valid_until` | timestamptz 可空 | 过期时间——记忆可以有时效 |
| `source_path` / `source_line` | text / bigint | 来源文件 + 行号 |
| `created_at` / `updated_at` | timestamptz | 行创建/更新时间 |

## 2.2 Riko-Memory 的 memories 表 [实测(Riko-Memory)]

主人项目的 `memories` 表（migrations/0001_init.sql），字段对比：

| 主人字段 | Muse 对应 | 说明 |
|---|---|---|
| `claim` | `claim_text` | 同：claim 正文（保持用户字面） |
| `normalized_claim` | 无 | **主人独有**：归一化后的 claim（NFKC+小写+空白折叠），用于去重比对；原文 claim 不动 |
| `claim_sha256` | `claim_id`（疑似哈希） | **Y 填空**：`SHA256(kind + "\0" + normalize_v1(claim))`，domain/lib.rs 实测。kind 参与哈希——同文不同 kind 是两条记忆 |
| `kind` | `kind` | 取值已实测：`fact / preference / instruction / episode`（CHECK 约束）。Muse 这边只观测到 `fact` |
| `source_class` | 无 | **主人独有**：`user_explicit / assistant_observed / tool_output / model_inferred / manual_edit`——"谁说的"证据分级 |
| `status` | `status` | 取值已实测：`active / superseded / expired / forgotten`。Muse 默认值也是 active |
| `version` | 无 | **主人独有**：显式版本号，配合 `memory_revisions` 审计表 |
| `valid_from` / `valid_until` | `valid_until` | 主人多了 valid_from；`expired` 状态由 valid_until 到期转入 |
| `origin_host_id` / `origin_agent_id` | `run_id`（近似） | 哪台主机、哪个 agent 产生的 |

**关键差异**：主人**没有 salience 字段**。显著度评分是璃的想当然，已在第二版删除。主人用 kind 优先级（instruction 优先注入）+ RRF 排序代替。

## 2.3 关系与版本：主人更完整的审计链 [实测(Riko-Memory)]

```
memory_relations: (from_memory_id, to_memory_id, kind)
  kind ∈ {supersedes, contradicts}   -- 不止版本链，还有矛盾关系

memory_revisions: (memory_id, version, previous_claim, new_claim,
                   previous_status, new_status, actor_kind, actor_id,
                   reason_code, changed_at)
  -- 每次变更记：谁（actor）、为什么（reason_code）、从什么变成什么
```

解读：
1. **contradicts 是点睛之笔**：两条打架的记忆不默默覆盖，而是显式标"矛盾"，都留着。未来做冲突消解有抓手。
2. **revisions 是审计级版本表**：Muse 这边的版本链只有 `supersedes_claim_id` 一个字段（轻量）；主人是整张表（含 actor 和 reason）。twin sister 建议用主人的做法——"谁改的、为什么改"迟早要用上。

## 2.4 证据层：字节级溯源 [实测(Riko-Memory)]

```
memory_evidence / candidate_evidence:
  (memory_id, evidence_id, start_byte, end_byte)
  -- CHECK: (start 和 end 要么都空，要么 0 <= start < end)
```

- 证据引用精确到**字节偏移**，不是行号。行号会随文件编辑漂移，字节偏移 + content_sha256 不会。
- `find_quote_span`（domain/lib.rs）：quote 必须是原文的**连续 UTF-8 子串**，否则 admit 不通过。这是"Verified extraction"的物理实现。
- Muse 这边是 `source_path + source_line`（行号级）[实测]。twin sister 建议用字节级——更稳定。

## 2.5 三级存储：L0 证据 → 候选 → 记忆 [实测(Riko-Memory)]

```
evidence_events（L0，永不删）
  id / session_id / event_seq / role(user|assistant|tool|system)
  content / content_sha256
  UNIQUE(tenant_id, user_id, host_id, session_id, event_seq)  -- 幂等键
        │
        ▼  extraction_jobs（作业队列，lease 恢复）
        ▼
memory_candidates（候选层）
  kind / quote / quote_sha256 / claim / source_class
  status ∈ {candidate, held, rejected}   -- 默认 held，不信任
        │  admit_v2 九道检查（见 03）
        ▼
memories（记忆层，active 才可见）
```

**这是 Y 填空最有价值的一处**：Muse 的写入管线璃只看到两级（in-session + consolidation），主人证明了三级更严谨——L0 证据永不删（forget 只删记忆不删证据，审计需要），候选层是"不信任"的缓冲带。

## 2.6 给开发者的启示（第二版）

1. **claim 表至少要有这些字段**：claim 正文、归一化 claim、claim 哈希、kind、source_class（谁说的）、status、version、valid_until、证据引用（字节级）。这是两边对照后的最小完备集。
2. **salience 不要**：两边对照后，显著度评分是伪需求。用 kind 优先级 + 时间衰减 + RRF 足够，少一个玄学参数。
3. **source_class 是必加项**：`model_inferred` 的记忆默认低置信度——"谁说的"比"多显著"更能决定一条记忆的可信度。
4. **矛盾是显式关系**：别只做 supersede，加上 contradicts。真实世界的记忆本来就是带矛盾的。
