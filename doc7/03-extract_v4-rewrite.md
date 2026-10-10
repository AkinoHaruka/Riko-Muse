# doc7/03 · extract_v4 rewrite 步骤施工规范

> 依据：`Muse文档/13-附录D-改写步骤规范.md`（璃，2026-10-06）——针对两轮真实模型验收
> 钉住的"0 active"病根（v3 prompt 不得补写主语 × admit_v2 要求显式主语的结构矛盾）。
> 逐字采纳 D.1—D.3、D.6、D.7；与现有冻结纪律冲突处按下述决定执行。

## 1. 版本接线（纪律：不回改历史版本）

| 项 | 决定 |
|---|---|
| prompt | 新建 `extract_v4`：阶段 1 复用 v3 提取行为（逐字 quote、一候选一命题），阶段 2 新增 rewrite 调用。`EXTRACT_PROMPT_VERSION` 同提交切换为 v4；v1/v2/v3 冻结 |
| 准入 | 新建 `admit_v4`（`ADMISSION_VERSION_V4="admit_v4"`，主线新作业同提交切换）。**v3 常量已被 Dream 占用**（doc6/09），故跳过该号。admit_v2 冻结 |
| 作业 | rewrite 是 extract job 内的第二步（同 job、同租约/重试/审计），不新增 job 类型（D.7-1） |

## 2. admit_v4 语义（v2 门"不动"，换受检文本）

规则 1–3（来源/逐字 span/长度）仍检 **quote**；规则 4–11 的受检文本换成
`claim`（存在且非空）否则回落 quote——回落路径与 v2 **逐字节同判**（保 v2 行为不静默漂移）：

- 规则 5 `multi_claim`：对改写文本改用"段首再次出现用户主语（用户/用户的）"判定
  多命题（`，`谓语并列不算）；quote 路径沿用原启发式（我/还/也/平时主要写）。
- 规则 11 `explicit_shape`：改写文本（第三人称"用户…"句式）不套用第一人称形状表；
  改判 = 文本以 `用户`/`用户的` 开头即视为显式（改写质量由 prompt 规则 + 审计约束）。
  quote 路径沿用原形状表。

其余门（CONTEXT_UNCERTAIN / UNCLEAR_SUBJECT / SECRET / SENSITIVE / THIRD_PARTY /
TEMPORAL）原样作用于改写文本——**内容政策门不因 rewrite 放水**（THIRD_PARTY/
SENSITIVE/TEMPORAL 该 held 继续 held，验收预期：v4 只解决主语/显式性矛盾）。

## 3. rewrite 调用契约（D.3 逐字 + 一处裁决）

- 每作业**一次**批量调用（D.7-4）：输入 = `{"context": <窗口事件 JSON 同提取输入>,
  "items": [{candidate_index, source_event_id, quote, kind, speaker, occurred_at}]}`；
  输出 = `{"results": [{candidate_index, claims: ["…"], confidence, rewrite_notes} |
  {candidate_index, claims: [], reason}]}`。
- **D.4-2（一条候选拆两条）与 D.3（单 claim 输出）冲突**：按 D.3 契约实现
  `claims` 数组（0=等效 null+reason，1=常规，**至多 2**），落库取首条进候选，
  第 2 条记入审计（候选表 dup 键为应用层 (job, evidence, kind, quote_sha)，不改 schema，
  重试幂等不破坏）。此偏离与理由记入交付记录。
- 改写规则（D.4）：主语只来自 speaker/同窗上文/用户自述，否则 claims=[] + reason；
  保留限定词；不引入新事实；推断 confidence ≤0.8；kind 不变；口语转书面；
  **允许说"不知道"**（D.5 例 6 是硬要求）。
- parse 严格校验：deny_unknown_fields、candidate_index 覆盖恰一次（缺失/重复/越界
  → MODEL_BAD_RESPONSE 走既有重试）；null → 候选保持 held（D.6），原 reason 不变。

## 4. 落库与审计

- `save_candidate`：claim 列 = `fold_whitespace(claim 或 quote)`；memories 的
  `claim_sha256` 同源（改写句成为记忆正文）；quote 列保持逐字原文（审计锚不丢）。
- 审计（D.6）：每 v4 作业一次 `audit_events` 行，`actor_kind='system'`、
  `actor_id='extract-worker'`、`action='extraction_rewrite'`、detail_json 记录全部
  改写条目（quote/claims/confidence/rewrite_notes/reason）。
- worker 时序：阶段 1 调用 → 代际核验 → （有 held 时）释放锁做 rewrite 调用 → 再核
  代际 → 保存。两次核验之间失权则丢弃结果交租约恢复（doc4/02 §5 语义不变）。

## 5. 验收指标（D.7-3）

同一语料、同一模型（Gemini 3.5 Flash Lite）：v3 基线 0/14 active；v4 的 active 数
即验收指标。同时核对：THIRD_PARTY/SENSITIVE/TEMPORAL 候选不得因 v4 变 active；
rewrite null 的候选保持 held；审计行存在且含 rewrite_notes。
