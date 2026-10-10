# doc7/05 · V2-P1 精读、explain 与统一可见性施工规范

> 立项：2026-10-06 · 分支：`Riko-Muse` · 前置：V2-S1 已交付（提交 `611ef7f`，schema 15）。
> 依据：`Muse-V2迭代开发文档/04-字段补齐与检索对齐.md` §1—§3、§5（产品决定）与 06 §3 的
> V03/V10/V11 行；`07-证据矩阵与推断纠偏.md` G9/D1/D7/D9。
> 与 V2 文档冲突时以本文件为准并回写修订记录。状态用语按 AGENTS.md 分档。

## 0. 定位与最大风险点

V2-P1 补的是**读取侧**：把已经存下来的逐字证据、版本链与保存原因按权限读出来，
并提供稳定的对外引用。它**不新增迁移**——全部读模型由既有表（`memories`、
`memory_evidence`、`evidence_events`、`memory_revisions`、`memory_retirements`、
`memory_candidates`、`memory_audit`）派生。0016 及以后留给 V2-R1/D1/Q1 按实际动工顺序分配。

本卡同时修一个**已核实的正确性缺口**（V2 文档 04 §2 指出、本轮源码复核确认）：

- `memories.rs` `is_active_clause()` 只判 `status`，不看 `valid_until`；
- `get_memory()` 只判 `status='active'` 与 `memory_retirements`，同样不看 `valid_until`。

也就是说，M1 的到期语义**只靠 15 分钟调度器把行改成 expired**才生效；在调度器跑之前，
一条 `valid_until` 已过期的记忆仍会被 get/search/compose 读出来。V2-P1 把可见性判定
收成**一个**谓词，所有读接口共用，不再依赖调度器时机。

## 1. 统一可见性判定（`VISIBLE_MEMORY`）

一条记忆对某次读请求可见，当且仅当四条同时成立：

| # | 条件 | 依据 |
|---|---|---|
| 1 | `status = 'active'`（`include_history=true` 时放宽为 `active/superseded/expired`，`forgotten` 恒不可见） | doc/12 §5；doc/11 §1 |
| 2 | `valid_until IS NULL OR valid_until > now` | 本卡新增；M1 语义 |
| 3 | 不存在 `memory_retirements` 行（retired 覆盖） | doc6/09 |
| 4 | 至少一条「有效来源」：`memory_evidence` 中该记忆的某条 evidence 未被 `suppressed_sources` 抑制（且抑制行属同一域） | doc/13 §3；doc6/02 §7 |

再加上 V2-S1 已有的 **域条件**：`domain_id IN (SELECT value FROM json_each(:read_json))`。
四条 + 域条件构成 `VISIBLE_MEMORY`，是本卡唯一的可见性真相。

**墓碑为什么不进读谓词（本轮核实修订）**：`purge_tombstones` 以 `content_sha256` 为键，
它的职责是「阻止被 purge 的正文经 spool 重放复活」（`record_evidence` 的重放闸），
而 purge 本身已物理删除被清证据行——「这条记忆已经没有来源了」由条件 4 的 EXISTS 自然表达。
最初把墓碑也写进读谓词后，d69 的共享来源用例实测到**同域内同文的另一条存活证据被误伤**
（一条证据被清、其内容哈希墓碑把同文证据一起判死）。因此墓碑只留在重放闸，
读路径不重复判一次内容哈希。

**实现形态（本轮冻结）**：谓词写成一处 Rust 常量 SQL 片段，绑定参数一律用**无名 `?`**，
绑定时按 SQL 文本出现顺序传入（V2-S1 已因混用 `?N`/无名 `?` 出过编号错位缺陷，
本卡不再重复）。检索路径不把谓词摊进四条候选 SQL，而是：

1. 各候选道照旧取候选 ID；
2. 合并后用一次 `filter_visible(scope, ids, dom, include_history)` 批量过滤；
3. 只用通过过滤的 ID 组装 hit。

这样谓词只有一个副本，代价是每次检索多一条按主键的批量查询。

## 2. 精读读模型 `MemoryExplain`

新存储方法 `Store::memory_explain(scope, ref_id, dom, include_history)`，返回：

| 字段 | 来源与要求 |
|---|---|
| `memory_id` / `kind` / `claim` / `status` / `version` | `memories` 当前行（不把 quote 当新版正文） |
| `valid_from` / `valid_until` / `occurred_at` / `created_at` / `updated_at` | `memories` 原样，未知即 null |
| `domain_id` | `memories.domain_id` |
| `evidence[]` | 每条：`evidence_id`、`start_byte`/`end_byte`（可为 null=整事件引用）、`quote`（按 UTF-8 字节 span 从 `evidence_events.content` **逐字切片**，不折叠空白、不拼接、不由模型重写；span 为 null 时返回整个 content 并标 `span_exact=false`）、`role`、`source_kind`、`occurred_at`、`host_id`/`session_id`、`suppressed`、`tombstoned` |
| `speaker` | 由 `role`+`source_kind` 映射：`user`→`"user"`、`assistant`→`"assistant"`、`tool`→`"tool"`；其它为 `null`。**不推断具体人物身份** |
| `subject` | 命题主体。当前内核没有可靠字段，恒为 `null` 并附 `subject_source: "unknown"`；**不得由 claim 或 quote 反推** |
| `reason` | `memory_candidates.reason_code`（同 job+quote_sha256 关联）；无则 `null`。理由文本与来源证据是两件事，缺就返回 null |
| `relations` | `superseded_by`（被哪条取代：`memory_revisions` 或 claim 前缀链）、`corrects`/`corrected_by`、`retired`（bool）、`retirement_reason_code` |
| `source_class` | `memories.source_class` |
| `stable_ref` | 见 §3 |

**历史与删除**：`include_history=false`（缺省）时只读 active 且必须通过 `VISIBLE_MEMORY`
（含 valid_until）；`include_history=true` 时允许 `superseded/expired` 的链上版本，
**但 `forgotten` 与已 purge 的正文永不返回**——explain 不能成为复活通道（doc 04 §2）。

## 3. stable_ref 格式与鉴权

```
riko://memory/<tenant_id>/<user_id>/<domain_id>/<memory_id>@<version>
```

- 由**服务端**生成，模型不得自造。`memory-domain::refs` 提供纯函数
  `memory_stable_ref(..)` 与 `parse_memory_ref(..)`。
- 解析出的 tenant/user **必须**与服务端由 Bearer 令牌解析出的 `ScopeKey` 完全一致，
  否则按不存在处理（不得跨 scope）。domain 必须 ∈ 该请求的读域集。
- 版本后缀是**引用时的版本**；explain 返回该引用对应的版本快照信息，
  但正文始终取自规范表当前行（不做第二真相源）。
- explain 的入口既接受裸 `memory_id`（必须已在本 scope + 读域集内），也接受
  `riko://` 引用；不接受任何形式的 `evidence_id` 由调用方指定。
- `claim_sha256` **不是**稳定引用（正文一改就变），继续只作内容指纹。

## 4. HTTP API

```
GET /v1/memories/{memory_id}/explain            # 缺省 active-only
GET /v1/memories/{memory_id}/explain?history=1  # 显式历史（仍受权限与 purge 限制）
```

- `{memory_id}` 接受裸 ID 或 URL 编码后的 `riko://` 引用。
- 可见性失败一律 404 `NOT_FOUND`（不泄露「存在但无权」）。
- 响应 envelope 含 `request_id`，与其它端点一致；`evidence` 数组保序（按 `occurred_at`、`evidence_id`）。
- 域：读域集由 V2-S1 的 `X-Riko-Memory-Domain` 与写域闭包决定（doc7/04 §2.3），本卡不新增域语义。

## 5. 存储层契约

新增/改动（`memory-store-sqlite`）：

| 项 | 说明 |
|---|---|
| `VISIBLE_MEMORY_SQL`（常量） | §1 谓词唯一副本，无名 `?` 绑定，含 `now`、`read_json` 两个参数位 |
| `filter_visible(scope, ids, dom, include_history, now)` | 批量返回可见 ID（保持输入顺序去重后由调用方排序） |
| `get_memory` | 追加 §1 条件 2/4（3 已有） |
| `search_memories` | 合并候选后经 `filter_visible` 过滤再计分输出 |
| `memory_explain(..)` | §2 读模型 |
| `memory_evidence_detail(..)` | 内部：带逐字切片的证据行（`tombstoned` 只作诊断展示，不参与可见性） |
| `resident.rs` `select_resident` | 加入条件 2（valid_until），保持与检索一致 |
| `semantic_index`/页面来源复核 | 不动（页面已有自己的来源版本复核） |

```rust
pub struct MemoryExplain {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub version: i64,
    pub domain_id: String,
    pub source_class: String,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub evidence: Vec<EvidenceDetail>,
    pub speaker: Option<String>,
    pub subject: Option<String>,
    pub subject_source: &'static str,   // 恒 "unknown"
    pub reason_code: Option<String>,
    pub relations: ExplainRelations,
    pub stable_ref: String,
    pub visible: bool,
}

pub struct EvidenceDetail {
    pub evidence_id: String,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
    pub quote: String,
    pub span_exact: bool,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub host_id: String,
    pub session_id: String,
    pub suppressed: bool,
    pub tombstoned: bool,
}

pub struct ExplainRelations {
    pub superseded_by: Option<String>,
    pub supersedes: Option<String>,
    pub retired: bool,
    pub retirement_reason_code: Option<String>,
}
```

## 6. 验收（`v2_p1_tests`）

1. **到期即时不可见**：`valid_until` 设为过去、**不跑** expire 调度器 → get/search/compose/
   explain/`filter_visible` 全部不可见；未到期的仍可见。
2. **来源全失效即不可见**：唯一来源被 `suppress`（forget）或被 purge 墓碑命中 → 不可见；
   有多条来源时只失效其中一条仍然可见。
3. **retired 与 forgotten**：retire 后读不到；forget 后 get/search/explain 都不返回正文。
4. **逐字证据**：explain 的 `quote` 与 `evidence_events.content` 的 span 切片逐字节相等；
   span 为 null 时整条返回且 `span_exact=false`；多来源全部返回、不合并。
5. **speaker/subject**：`role=user` → `speaker="user"`；`subject` 恒 `null` 且
   `subject_source="unknown"`（不得推断）。
6. **stable_ref**：格式正确；解析往返一致；换 tenant/user/domain 解析后必须不可读（404 语义）。
7. **历史与复活**：`include_history=true` 能读到 superseded 链；forgotten/purge 过的正文
   在两种模式下都不返回。
8. **域**：主域读不到 side 记忆的 explain；授权后（V2-S1 机制）可读。

档位：`cargo build --workspace && cargo test --workspace`（构建/测试通过）→ 临时库 HTTP
冒烟（explain 端点 + `history=1` + 越权引用 404）。真实 DSH 闭环与真实模型：**本卡不涉及，
未验证**。适配器侧 `memory_explain` 工具随本卡交付，但 TS 构建与真实宿主调用单独记档。

## 7. 明确不做 / 移交

- **entry / chunk 上下文对象**（`context_entries`、`entry_sources`、entry 召回对照）→ V2-Q1，
  届时才用 0016 迁移。
- `entry_attributes` 任意 KV → 不实现；需要时按域/类型/来源校验扩展。
- salience 字段与排序权重 → 不伪造，按 V2 文档 04 §5 保留未知。
- `privacy_class` 取值映射 → 不硬映射；沿用 V2-S1 的来源域与公开范围。
- `reason_text`（动机文本）生成 → 本卡只暴露既有 `reason_code`；生成动机须另立规范。
- 人物/群组（V2-R1）、四分面蒸馏与可读投影（V2-D1）、后台闭环（V2-B1/A1）、
  宿主使用纪律（V2-H1）→ 各自施工卡。
