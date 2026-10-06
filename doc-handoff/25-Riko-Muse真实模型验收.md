# 25 · Riko-Muse 真实模型验收（第一轮观察）

> 日期：2026-10-06 · 分支 `Riko-Muse`（HEAD a282e76）· 执行：ZCode（Aki 在场）
> 方法依 doc7/02 §下一步顺序第 1 条：真实模型验收。密钥来源 `模型.txt`（SiliconFlow
> Qwen3.5-4B）；密钥只写仓库外临时 key 文件，验收结束已删除，本文档无任何凭据。
> 语料：3 段自然中文对话（25 轮，user 17 / assistant 8），**先写对话、后标注**，
> 不围绕 cue 字面量设计；语料是合成的，质量数字是观察样本，不是质量验收结论。

## 1. 环境

- 临时库（`%TEMP%/muse-real/muse-real.db`，schema 14，验收后已删除）；memoryd serve 端口 8796。
- 提取 worker：`https://api.siliconflow.cn/v1/chat/completions` + `Qwen/Qwen3.5-4B`，
  `model_extra_json = { enable_thinking = false }`，`model_max_tokens = 1024`。
- 首轮 `model_timeout_secs` 用默认 30s → s2/s3 三次尝试全部 `MODEL_TIMEOUT` 转 dead
  （L0 保留，符合设计）；提至 90s 后经 `/v1/jobs/{id}/retry` 重试成功。

## 2. 写入管线（真实模型，9 次调用）

| 作业 | 尝试 | 结果 |
|---|---|---|
| s1 | 1（30s 超时档） | succeeded |
| s2 | 3 次 timeout → dead → 90s 档重试 1 次 | succeeded（共 4 次调用） |
| s3 | 2 次 timeout → 90s 档重试 2 次 | succeeded（共 4 次调用） |

**结果：22 条候选，全部 `held`，0 rejected，0 active。** held 原因分布：
`UNCLEAR_SUBJECT ×8`、`NOT_EXPLICIT ×8`、`THIRD_PARTY ×4`、`MULTI_CLAIM ×1`、`TEMPORAL ×1`。
kind 分布：fact 11 / preference 5 / instruction 3 / episode 3。

具体例子（节选，均 held）：

- `[fact|MULTI_CLAIM]` "我上周五说的是周三发工资，不对，我说错了，是这周三"——模型照抄整句
  触发单命题门。
- `[fact|THIRD_PARTY]` "老王不能吃海鲜，他对虾过敏"——第三人信息被准入拦下（本语料中
  唯一"应该被拦"的条目，判定正确）。
- `[preference|NOT_EXPLICIT]` "我一般左侧分批，跌 10% 买一点，越跌越买"——真实偏好但模型
  claim 未改写为显式陈述，被 `NOT_EXPLICIT` 持有。

**结论（如实）**：与 doc-handoff/19 的历史观察一致——Qwen3.5-4B 在 extract_v3 下的 claim
写法偏"照抄原话"，未经显式主语/显式陈述改写，导致本语料 0 条进入 active。保护面
（默认不信任、第三人拦截、时效拦截）按预期工作；**写入的"质量半区"（模型改写能力）
是当前真空**，与 Muse 增量无关，属既有内核验收缺口。观察期继续收集样本，不预设方案。

## 3. rupture 规则（同语料，先标注后扫描）

17 条 user 轮次，人工标注 4 条真纠正（对 Agent 的纠正/边界）：s1#7"别这样跟我推抄底话术"、
s2#4"你又来劝我早点睡了"、s2#7"上次你说汇报在周四，害我白请了半天假"、s3#4"不是这样的，
我妈能吃微辣"。

| | 标注为纠正（4） | 标注为非纠正（13） |
|---|---|---|
| 规则命中（4） | **TP 3**（s1#7、s2#4、s3#4） | **FP 1**：s1#4"不对，我说错了"——**自我纠正**，非对 Agent，规则无法区分 |
| 规则未命中（13） | **FN 1**：s2#7 抱怨 Agent 过去错误，无 cue 字面量 | 12 正确沉默 |

precision 3/4，recall 3/4。两个错误方向各有一个具体例子，方向与 Muse 文档 8.3-4 预判一致：
误报由人工关线兜底，漏报需 V2（扩充 cue 或语境规则）。

线程归组：4 条全落在同一个 7 天窗口线程（设计如此：v1 归组是时间桶不是话题桶），
**线程 title 取首个 rupture 事件——恰好是那条误报**，作为已知局限记录（doc7/01 §1.3）。

## 4. synthesis / compose / 关线（端到端）

- synthesis v1（调度器自动生成）：纠正 4 / 用户 17，无纠正率 0.76，待修复线程 1——
  指标在真实语料上计算正确；刷新幂等（无新触发条件不出版本）。
- `POST /v1/context/compose` + `include_alignment: true`：响应含 `alignment` 对象，
  text 前置 `<alignment_synthesis version="1">` 块；memory items 为空（无 active 记忆，
  search 0 命中，`index_degraded=true`）——两路注入互不干扰。
- `POST /v1/repair/threads/{id}/close`（reason=验收演练）：`changed=true`，synthesis 因
  open 线程数变化生成 v2（open=0）。关线动作落 `audit_events`（`repair_thread_close`）。

## 5. 未决事项：回环瞬时 404（第三轮出现，根因未定）

验收收尾时 `POST /v1/alignment/synthesis` 一次 404（空响应体；同进程内其它 POST 均正常）。
至此共三轮偶发（两次冒烟、一次本验收），均发生在 POST；受控复测 20/20（含同连接连发与
新建连接连发）全部 200，**无法按需复现**。`netstat` 显示 8796 LISTENING 归属 PID 0，
本机沙箱网络层嫌疑最大（见 doc-handoff/24 §4 首次记录）。处置：不判为应用缺陷；
生产部署若复现，先抓响应头 `date`/`server` 与端口归属对比再定位。

## 6. 模型调用与清理

- 真实模型调用：9 次 attempts（SiliconFlow Qwen3.5-4B；含 5 次 timeout 失败调用）。
  Gemini / OpenRouter 本轮未调用。
- 验收后已停止 serve；临时 key 文件与令牌文件已删除；临时库与语料文件已删除。
- 本文档不含凭据；语料引文为合成对话片段。

## 7. 给下一轮的输入

1. 写入质量真空在"模型改写"半区：观察期收集 0-active 现象的更多样本后再议
   （改 extract prompt 需新建版本，冻结纪律见 AGENTS.md §5）。
2. rupture V2 的两个候选方向已有具体例子锚定：自我纠正排除（FP 例）、
   "上次你说…"式无字面量抱怨（FN 例）。仍按纪律等更多样本再动清单。
3. DSH adapter 接线（下一步第 2 条）开工前先做 seam 现场核对。

## 8. 第二轮：换模型验收（2026-10-06，用户改了 `模型.txt` 优先级后执行）

用户将优先级调整为：1 OpenRouter `inclusionai/ling-3.1-flash` → 2 Gemini →
3 SiliconFlow。同协议、**同一份语料**重跑（可比性）。

### 8.1 优先级 1：OpenRouter——不可用（凭据过期）

3 个作业 × 3 次尝试共 **9 次调用全部 HTTP 401**，OpenRouter 返回
`"API key expired"`（直连探测确认）。凭据问题，非代码问题；401 不产生 token 消耗。
作业按设计转 dead，L0 保留。

### 8.2 优先级 2：Gemini 3.5 Flash Lite——通过

- 探测 1 次 + 提取 3 次（每作业 1 次即成功，90s 超时档）= **4 次成功调用**。
  对比第一轮：Qwen3.5-4B 同超时档每作业要 4 次尝试，Gemini 延迟显著更好。
- **14 条候选，全部 held，0 active**（与 Qwen 定性一致：模型都偏照抄原话），
  但候选数 22 → 14。held 分布：`UNCLEAR_SUBJECT ×4`、`NOT_EXPLICIT ×4`、
  `MULTI_CLAIM ×3`、`THIRD_PARTY ×1`、`TEMPORAL ×1`。
- 闸门一致性：`我爸不吃香菜，我妈不碰辣` 被 `MULTI_CLAIM` 正确拦下（两命题）；
  家庭成员被 `THIRD_PARTY` 持有；一次性语境 `TEMPORAL`。

### 8.3 跨模型对照（本轮核心结论）

| | Qwen3.5-4B（第一轮） | Gemini 3.5 Flash Lite（第二轮） |
|---|---|---|
| 每作业调用次数 | 1 / 4 / 4 | 1 / 1 / 1 |
| 候选数 | 22 | 14 |
| active | **0** | **0** |
| held 占比 | 100% | 100% |
| rupture 命中集合 | 4 条（3 TP / 1 FP） | **逐条相同** |
| synthesis 指标 | 4/17, 0.76 | 4/17, 0.76 |

两个结论：(a) **换模型不改变"0 active"现象**——写入质量瓶颈在 extract_v3 的模型改写
半区，与具体小模型关系不大，两个供应商一致；(b) **模型更换完全不扰动 rupture/synthesis
层**（确定性规则与指标逐条复现）——关注点分离按设计工作。

### 8.4 附带观察

- `extraction_jobs.error_code` 在作业 succeeded 后保留最后一次失败的错误码
  （显示 `MODEL_HTTP_ERROR` 但实为成功）——既有内核的展示层行为，非本批引入，仅记录。
- 本轮 rupture/synthesis/compose/关线端到端全部复现第一轮结果；synthesis 版本链
  （空窗口基线 v1 → 扫描后 v2 → 关线后 v3）按触发策略推进，行为符合 doc7/01 §1.4。

### 8.5 调用与清理（第二轮）

- 模型调用：OpenRouter 9 次（全部 401，未产生 token）+ Gemini 4 次（全 200）。
- 临时 key/令牌/库文件验收后删除；本文档无凭据。

## 9. 第三轮：extract_v4 rewrite 验收（2026-10-06，验收指标达成）

依据 `Muse文档/13-附录D-改写步骤规范.md` 实施 extract_v4（规范 [doc7/03](../doc7/03-extract_v4-rewrite.md)），
同语料、同模型（Gemini 3.5 Flash Lite）对比 v3 基线（0/14 active）。

### 9.1 验收指标（D.7-3）：active 0 → **7**

6 次模型调用（3 extract + 3 rewrite），全部首次成功。7 条 active 记忆（v4 改写句为正文）：

- 用户喜欢喝百事，办公室囤了一箱百事
- 用户投资时一般采用左侧分批策略，跌 10% 买一点，越跌越买
- **用户不喜欢被推销"抄底"话术**（instruction，NOT_EXPLICIT→改写后显式）
- 用户持仓里除了百事还有腾讯和小麦期货
- **用户在2026年10月前后在玩《星穹铁道》**（TEMPORAL→按 occurred_at 归位）
- 用户玩《星穹铁道》的时间是每天晚上8点到11点
- 用户在《星穹铁道》中只清模拟宇宙周常，别的不碰

7 条 held 全部正确：第三人（我妈/老王）不被改写、准入门拦截；"最近"未归位的候选保持
TEMPORAL；临时请求（季度汇报提醒）不硬改。**内容政策门零放水。**

### 9.2 端到端

- 检索"想买百事可乐" → 2 条改写记忆命中（FTS/grams 索引改用 claim 正文，见 §9.3）。
- compose `include_alignment` → alignment 块 + 改写记忆同轮注入（既有形态不变）。
- 审计：每作业 1 行 `extraction_rewrite`（actor=system/extract-worker），含
  quote/claims/confidence/rewrite_notes（D.6 达成）。

### 9.3 实施中发现并修复的两个缺陷（都在本轮验收暴露）

1. **promote 路径 claim 未跟随改写**：memories INSERT 仍写 quote（候选表正确、
   记忆表错误）。修复：claim/normalized_claim/revision 统一用改写后的 `claim_text`。
2. **索引文本未跟随**：promote 的 FTS/grams 索引用 quote。修复：改用 `claim_text`
   （quote 已存 evidence 锚）。两缺陷均为 v4 才能触达的路径（v3 claim==quote 掩盖）。

### 9.4 冻结纪律核对

- extract_v1/v2/v3、admit_v1/v2/v3（Dream）行为未变；新作业默认版本同一提交切换
  `extract_v4/admit_v4`；`admit_v3` 号段被 Dream 占用故主线跳至 `admit_v4`（doc7/03 §1）。
- 测试：workspace 156 项全过（新增 v4 dispatch/parse/admit 测试 + 更新版本钉值断言）。
- 遗留观察：save_candidate 对 held 候选也 mark_index_dirty（最后一条 held 会使 dirty=1
  残留至 rebuild-index）——既有行为，与本批无关，仅记录。

