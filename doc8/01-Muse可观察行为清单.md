# 01｜Muse 可观察记忆行为清单

## 1. 取证范围与方法

### 1.1 允许读取的说明材料

- `/home/hatch/docs/memory.md`
- `/home/hatch/docs/self_improvement.md`
- `/home/hatch/docs/client-surfaces.md`
- `/home/hatch/docs/files-and-library.md`
- `/home/hatch/docs/privacy-and-credentials.md`
- `/home/hatch/docs/data-handling.md`
- `/opt/muse/skills/forget/SKILL.md`
- `/opt/muse/skills/forget/references/artifact-inventory.md`

### 1.2 私人记忆目录的检查边界

只观察结构、文件名形态和文件元数据，不读正文。现场观察到：

```text
~/MEMORY.md
~/memory/YYYY-MM-DD.md
~/memory/people/INDEX.md
~/memory/people/<slug>.md
~/memory/groups/INDEX.md
~/memory/bank/experience.md
~/memory/bank/opinions.md
~/memory/bank/reflections.md
~/memory/bank/world.md
```

观察日期为 2026-10-23；按日文件从 `2026-09-25.md` 延续到 `2026-10-23.md`。人物目录有 `INDEX.md` 与多个 slug 文件，群组目录当前至少有 `INDEX.md`，bank 四文件均存在。所有被检查文件都是普通 Markdown 文件、权限 `0660`。

这里不记录任何标题、条目、人物关系或正文片段。

## 2. 行为清单

| 编号 | 分级 | 可观察行为 | 证据路径 | 本卡可采用的结论 |
|---|---|---|---|---|
| M-01 | 观察事实 | 用户可要求 remember、update、forget；后台也可写记忆笔记 | `/home/hatch/docs/memory.md` | 文件后端必须同时支持前台显式写和后台整理写 |
| M-02 | 观察事实 | Memory upkeep 在有新信号时按小时运行，把新会话整理进 `MEMORY.md` | `/home/hatch/docs/self_improvement.md` “Memory upkeep” | `MEMORY.md` 是活的 curated 总览，不只是一次性导出 |
| M-03 | 观察事实 | 新 claim 取代旧 claim，按日笔记保留完整轨迹 | 同上 | 当前视图与历史轨迹分工明确；不能用覆盖日记冒充 supersede |
| M-04 | 观察事实 | 会话内仍需主动记录值得保留的信息，后台 upkeep 不替代前台 bookkeeping | 同上 | 前台写入与后台整理是两条写入时机不同的路径 |
| M-05 | 观察事实 | Relationships 每小时维护每个人/群组的页面，记录事实、历史和关系性质，并按亲近程度组织 | 同上；结构见 `~/memory/people/`、`~/memory/groups/` | 人物/群组页属于后台维护的关系投影 |
| M-06 | 观察事实 | Dreaming 夜间审阅近期对话，写 dated reflections、repair threads 和行为 synthesis | `/home/hatch/docs/self_improvement.md` “Dreaming” | 反思/修复/对齐是后台工序，不应伪装成用户原话 |
| M-07 | 观察事实 | substantial conversation quiet 后有次数受限的整理 sweep | 同上 “Quiet-moment pass” | quiet 触发是后台整理时机之一 |
| M-08 | 观察事实 | Web Identity 可打开并编辑 MEMORY、SOUL、IDENTITY；Memory 是可编辑文件而不是行列表 | `/home/hatch/docs/client-surfaces.md:330-347` | 文件形态必须是用户可见的一等界面；不能仅导出只读快照 |
| M-09 | 观察事实 | 文件跨会话持久存在；System Files 是该文件系统窗口；Markdown 可在 Web 编辑 | `/home/hatch/docs/files-and-library.md` | 文件必须在会话结束后持久，且要考虑外部编辑 |
| M-10 | 观察事实 | 主聊天/个人 side chat 的 memory search 可返回相关 memory 与聊天摘录，但不是所有消息；chat search 独立 | `/home/hatch/docs/memory.md`、`/home/hatch/docs/client-surfaces.md:138-151` | 记忆检索不是简单全文列出全部对话；索引是文件之上的检索投影 |
| M-11 | 观察事实 | 忘记不是改一行；需清理来源、派生、搜索、缓存和能重建内容的未来生产者 | `/opt/muse/skills/forget/SKILL.md`；`references/artifact-inventory.md` | forget 必须是传播闭包和重建协议 |
| M-12 | 观察事实 | forget 执行记录 claim IDs 与精确 `MEMORY.md#L<line>` / `memory/<date>.md#L<line>` locator，随后 runtime retract claim chain 并 reindex | 同上 | 来源追溯至少可落到 claim ID + 文件行定位；删除后必须重建稀疏/向量索引 |
| M-13 | 观察事实 | Markdown corpus 被索引到 PostgreSQL 搜索/embedding 表；另有 claim spine，保存 verified claim、quote/会话依据、状态与 supersession | `/opt/muse/skills/forget/references/artifact-inventory.md` §3 | Muse 可观察材料本身并不支持“内部完全没有数据库”；数据库至少承担索引与 claim spine |
| M-14 | 观察事实 | `muse.memory_explain` 可按 claim ID、`memory://` URI 或 `path#L<line>` 解释来源 | 同上 §3 | explain 成立依赖稳定 claim ID、文件 locator、quote/会话 handle 与链关系 |
| M-15 | 观察事实 | 派生记忆包括 bank、人物/群组页、personalization、compact `MEMORY.md`、profile、prompt cache 和中央 embedding | 同上 §3 | 不能因“也是 Markdown”就把 bank/人物页误当不可重建原始事实 |
| M-16 | 观察事实 | 当前本机存在四分面 bank：world、experience、reflections、opinions | 只看结构的 `~/memory/bank/*.md` | 与双生 `facet_v1` 四分面一一对应；见 §4 |
| M-17 | 观察事实 | 完整 Reset 才是聊天、文件和 active tasks 的全量擦除；单独移除 memory 不等于删掉所有记录 | `/home/hatch/docs/data-handling.md`；`privacy-and-credentials.md` | forget、文件删除、完整 reset 必须区分 |
| M-18 | 观察事实 | 后台建议/工作路径的解释与追踪使用 `muse.db`；证据有时再指向文件 | `/home/hatch/docs/self_improvement.md` “Explaining…”/“Tracing…” | Muse 的运行账本并非可观察为纯 Markdown；F0 不得杜撰其内部实现 |

## 3. Muse 的可观察存储分工模型

### 3.1 事实源文件

以下结论按可观察材料成立：

- `~/memory/YYYY-MM-DD.md` 是按日历史笔记，保留 claim 演进的完整轨迹。
- `~/MEMORY.md` 是经 upkeep 整理的 curated 当前总览，用户可直接编辑。
- `SOUL.md` 与 `IDENTITY.md` 是用户可打开和编辑的身份/人格文件，但本机材料没有说明其全部内部版本协议。

“`MEMORY.md` 与日记究竟谁在内部冲突时拥有最终裁决权”没有被本机说明文档完整定义。F0 因此不把猜测写成 Muse 事实；目标架构会自行规定冲突规则。

### 3.2 派生层

- `memory/people/*.md`、`memory/groups/*.md` 与各自 `INDEX.md`：后台 relationships 工序维护的实体/关系投影。
- `memory/bank/*.md`：forget inventory 明确把 bank 列为 derived memory；本机结构显示为四分面 Markdown。
- compact/profile/personalization/prompt cache/embedding：由源记忆产生的下游投影。
- dreams、repair threads、alignment synthesis：后台 Dreaming 的反思和行为调整产物；有自身证据与运行记录。

派生层即使落成文件，也不能反向成为无来源事实；forget 必须让其失效或重建。

### 3.3 前台与后台写入时机

```text
前台会话：学到值得保留的信息 → 当场记账
                       ↓
后台 hourly upkeep：有新信号 → 合并到 MEMORY.md / supersede
后台 hourly relationships：更新人物与群组页
后台 quiet pass：安静后做一次有界整理
后台 nightly dreaming：反思、repair、alignment
                       ↓
搜索与上下文索引刷新
```

该时间模型直接来自 `/home/hatch/docs/self_improvement.md`。具体调度算法、锁、事务边界、模型 Prompt、重试和数据库表结构均不可观察。

### 3.4 检索与索引

观察事实是“Markdown corpus 之上存在稀疏搜索、向量搜索与 claim spine”，不是“检索直接 grep Markdown”。memory search 还会在特定聊天表面返回相关聊天摘录，但不等于全量会话搜索。

因此目标双生可采用：文件为规范源；FTS/CJK/向量/倒排为可重建加速层。这是与观察形态一致的设计决定，不是对 Muse 内部实现的完整复刻声明。

### 3.5 Forget 传播

可观察流程是：

1. 先找齐源文件、claim、派生内容、缓存和未来生产者；
2. 停止可能重写该信息的 in-flight/queued/scheduled producer；
3. 精确改写或删除权威位置；
4. 记录 claim IDs 与文件 locator；
5. retract supersession chain；
6. 重建搜索、向量、prompt cache 与发布投影；
7. 用新鲜状态验证没有 active/derived/future copy。

这说明文件事实源并不会自动简化 forget：传播闭包反而必须显式、可中断恢复、可验证。

### 3.6 Explain 为何成立

Muse 可观察 explain 依赖四个锚点：

- 稳定 claim ID；
- `memory://` 或文件行 locator；
- 支撑 claim 的 quote/会话 handle；
- supersession 链。

行号会随自由编辑漂移，只能作为人类 locator，不能单独承担机器外键。目标架构须额外保存不随行移动的 block ID、内容哈希和 UTF-8 byte span。

## 4. Muse bank 与双生 `facet_v1` 对照

| Muse 文件 | 双生 facet | 对应判断 | 边界 |
|---|---|---|---|
| `memory/bank/world.md` | `world` | 名称与四分面直接对应 | Muse 的分类算法不可观察；只能对齐输出槽位 |
| `memory/bank/experience.md` | `experience` | 直接对应 | 双生当前规则是确定性投影，不代表 Muse 同算法 |
| `memory/bank/reflections.md` | `reflections` | 直接对应 | 双生当前要求存在 supersede 取代边；Muse 是否如此不可观察 |
| `memory/bank/opinions.md` | `opinions` | 直接对应 | 文件名对应不证明准入/排序相同 |

设计结论：保留四个固定 Markdown 文件；其中的每个条目必须携带源 memory ID/version/hash，读时复核源有效性。四分面文件是派生层，不是新的事实源。

## 5. 不可观察清单

以下不得在后续文档写成 Muse 事实：

- Muse 是否用事务、WAL、锁文件、generation pointer 或对象存储提交 Markdown；
- `MEMORY.md` 与 daily note 冲突时的内部优先级；
- claim spine 是否是最终裁决者，还是仅为 Markdown 的结构化镜像；
- embeddings、PostgreSQL 和中央 publication 的具体 schema、保留期与一致性窗口；
- 关系页、bank、Dream 的 Prompt、模型、预算和完整调度算法；
- 手工编辑文件后的增量解析、版本生成和冲突处理细节；
- 备份中 forget 的物理擦除时点。

这些空白由双生目标规范显式决定，但必须标注为“设计决定”，不能写成“Muse 就是这样实现”。
