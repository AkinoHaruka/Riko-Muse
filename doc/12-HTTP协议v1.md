# 12 本机 HTTP 协议 v1

本文件是 Rust 服务与 DSH 适配器的唯一端点契约。所有路径以前缀 `/v1` 开始；JSON UTF-8；请求／响应字段用 `snake_case`；未知字段返回 400 `INVALID_FIELD`，不静默忽略。除 `health/version` 外均需要 `Authorization: Bearer <32-byte-random-token>`；服务端从令牌查出 scope，**业务请求不含 `tenant_id/user_id`**。令牌无效、用户 disabled 统一返回 401，不泄露用户是否存在。响应头 `Cache-Control: no-store`。`X-Request-Id` 若有则使用合法 UUID；否则服务端生成。

## 1. 通用 envelope

成功响应含 `request_id` 与业务字段。失败响应：

```json
{"request_id":"018...","error":{"code":"EVENT_CONFLICT","message":"event key already exists with different content"}}
```

`message` 只供诊断，不包含用户文本、令牌和其他账户信息。客户端判断依赖 `code`，不解析英文句子。HTTP 状态：400 输入无效；401 身份错误；403 当前用户无权操作；404 本 scope 不存在；409 幂等／版本／状态冲突；413 正文超限；429 并发／预算限制；503 内核或模型暂不可用；500 未知错误。404 和 403 的具体区分不能让调用方枚举其他用户 ID；跨用户记忆 ID 一律 404。

## 2. 健康与版本

`GET /v1/health` 无需认证，200 `{status:"ok", protocol_version:1, db:"ready", index:"ready|degraded"}`。数据库未完成迁移时 503。`GET /v1/version` 无需认证，200 `{protocol_version:1, schema_version:1, build:"<commit>"}`。不输出数据库路径或用户列表。

## 3. 写入 L0 事件

`POST /v1/evidence/events`

```json
{
  "origin":{"host_id":"dsh","agent_id":"agent-a","session_id":"session-x"},
  "event_seq":42,
  "role":"user",
  "source_kind":"user",
  "occurred_at":"2026-09-24T12:00:00Z",
  "content":"以后回答我用中文"
}
```

字段均必填；`event_seq` 非负；三个 origin ID 非空且最长 256 字符；content 1～64 KiB UTF-8；timestamp 必须可解析为带时区时间并转 UTC。`source_kind` 取 `user/assistant/tool/plugin/system`，role 与 source_kind 组合由适配器提供；`source_kind=plugin` 不参与记忆提取。服务器计算 `content_sha256`。唯一键 `(scope,host_id,session_id,event_seq)`；首次响应 201 `{status:"recorded",evidence_id:"..."}`；相同键、相同 hash 响应 200 `{status:"already_recorded",evidence_id:"..."}`；不同 hash 响应 409 `EVENT_CONFLICT`。

## 4. 关闭窗口和查看任务

`POST /v1/extraction/flush`

```json
{"host_id":"dsh","session_id":"session-x","through_event_seq":42}
```

请求 schema 不变。**服务端分窗为权威**（doc4/03）：服务器读取认证 scope 中该 host/session 自上一已排窗口之后到 `through_event_seq` 的事件，按实际模型输入序列化字节贪心切分为多个有序窗口（每窗 ≤100 事件且 ≤32 KiB），同一事务内全部创建。响应的 `job_id` 指请求 through 对应的**最后一个**作业；`status` 如实返回该作业状态（通常 `queued`）。单事件自身超过 32 KiB 时生成 `dead/WINDOW_TOO_LARGE` 作业且不调用模型，后续窗口照常创建。新范围内没有用户事件时创建内部 `succeeded` 零模型调用 checkpoint 以推进下界，返回 200 `{status:"nothing_to_extract","job_id":"..."}`（`job_id` 供诊断）。同 `through_event_seq` 重试（在乱序检查之前）返回同一 job ID 和当前状态。`through_event_seq` 不得大于该会话已收到的最大事件序号，也不得小于已排最大窗口（否则 409）。v1 不接受 `wait=true`，避免适配器阻塞模型回合。

`GET /v1/jobs/{job_id}` 返回本 scope 的 `{job_id,status,attempts,created_at,updated_at,error_code?,input_tokens?,output_tokens?}`。不同用户的 ID 一律 404。`POST /v1/jobs/{job_id}/retry` 只允许 `dead` 状态、同 scope 的显式管理调用；`WINDOW_TOO_LARGE` 的作业不接受原样 retry，须用本地 CLI `memoryd job skip` 显式跳过（写审计，L0 原文保留）。DSH Agent 工具不注册此端点。

`GET /v1/jobs?status=<值>&limit=<1..100>&cursor=<opaque>`（doc4/04 §1）返回本 scope 的作业列表：`status` 取 `queued/running/retryable_failed/succeeded/dead/all`，默认 `dead`；默认 limit 20。按 `(created_at DESC,id DESC)` 稳定排序；`cursor` 是 base64url 编码的 `(created_at,id)` 翻页位置（解码上限 512 字节，服务端严格校验；scope 始终来自当前令牌，cursor 不能指定 scope），坏 cursor 返回 400 `INVALID_FIELD`。响应 `{jobs:[{job_id,host_id,session_id,through_event_seq,status,attempts,run_after,lease_until,error_code,skipped,created_at,updated_at}],next_cursor}`，`skipped` 表示该 dead 作业已有显式跳过记录；列表不返回事件正文、Prompt 或密钥。`GET /v1/jobs/{job_id}` 同时补充 `window_key/prompt_version/model_name` 等诊断字段。

## 5. 用户显式记忆

`POST /v1/memories/remember`

```json
{
  "origin":{"host_id":"dsh","agent_id":"agent-a","session_id":"session-x"},
  "user_evidence_id":"...",
  "quote":"以后回答我用中文",
  "kind":"instruction"
}
```

服务器检查证据属当前 scope、origin 的 host/session、role=user、source_kind=user，且是该 session 的最新用户事件；quote 是其正文的连续原文子串。`claim` 固定为 quote（仅折叠连续空白）；不接受调用方另填解释性 claim。`kind` 属四类。成功 201 `{memory_id,version:1,status:"active"}`；同 scope 同 kind 同规范化 claim 已 active 时，关联新证据并返回 200 `{memory_id,version,status:"active",deduplicated:true}`。不满足最新用户证据返回 409 `STALE_USER_EVIDENCE`，quote 不匹配返回 400 `QUOTE_MISMATCH`。

`GET /v1/memories/{memory_id}` 只查认证 scope；返回 `{memory_id,kind,claim,status,version,occurred_at?,valid_until?,origin_agent_id,evidence_refs:[...]}`。普通 Agent 工具只允许读取 active；管理 CLI 可通过另一条管理接口读取历史，本协议不提供全库任意浏览。

## 6. 纠错与遗忘

`POST /v1/memories/{memory_id}/correct`

```json
{
  "expected_version":1,
  "origin":{"host_id":"dsh","agent_id":"agent-a","session_id":"session-x"},
  "user_evidence_id":"...",
  "old_quote":"我住在上海",
  "replacement_quote":"我现在住苏州"
}
```

服务器要求最新用户事件同时包含 `old_quote` 和 `replacement_quote` 的连续片段；`old_quote` 在旧 memory claim 中也必须出现。事务中旧 memory `superseded`、version+1，新 active memory 保存 replacement_quote 与证据，建立 `supersedes` relation。若旧片段不匹配，返回 `AMBIGUOUS_TARGET`，适配器请用户更明确表达；不让模型自行猜指代。成功响应 `{old_memory_id,new_memory_id,old_version,new_version:1}`。

`POST /v1/memories/{memory_id}/forget`

```json
{
  "expected_version":2,
  "origin":{"host_id":"dsh","agent_id":"agent-a","session_id":"session-x"},
  "user_evidence_id":"...",
  "target_quote":"我住在上海"
}
```

服务器要求最新用户事件含明确遗忘动词（首版中文 `忘记/删除记忆/不要再记得`、英文 `forget/delete this memory`）及与该 memory claim 连续匹配的 `target_quote`；不能只凭 ID 删除。含糊“忘记那个”返回 409 `AMBIGUOUS_TARGET`，需要用户重述目标。成功 200 `{memory_id,status:"forgotten",version,raw_evidence_retained:true}`；再次相同请求且当前已 forgotten 返回 200 同状态，不能让旧版本冲突阻断幂等确认。原始 L0 保留，除非以后使用独立的原始数据删除流程。

## 7. 搜索与上下文

`POST /v1/memories/search`

```json
{"query":"我的语言偏好","limit":5,"include_history":false}
```

`query` 1～2048 Unicode 标量字符，`limit` 1～20，默认 5。普通搜索只返回 active 且未过期记录。`include_history=true` 仅当 query 含明确历史词（中文 `以前/过去/曾经/当时`，英文 `previously/used to/before`）时允许，否则 400 `INVALID_FIELD`；历史记录标明 `status` 与时间，绝不当当前事实注入。响应 `{items:[{memory_id,kind,claim,status,score,match_reason,evidence_refs:[...]}],index_degraded:false}`。`score` 是内部排序分数，仅同次查询可比较，不宣称概率。

`POST /v1/context/compose`

```json
{"agent_id":"agent-b","query":"按我的习惯回答","max_items":5,"max_chars":2000}
```

仅返回 active；`max_items` 1～5、`max_chars` 100～2000，服务端强制上限。响应 `{text:"...",items:[{memory_id,evidence_ids:[...]}],truncated:false,index_degraded:false}`；无结果 `text=""`。适配器不自行拼接未经服务端筛选的全文。查询或内核超时由适配器跳过注入并记录本地错误，不发送其他用户的缓存结果。

## 8. 错误码全集（v1）

`INVALID_JSON`, `INVALID_FIELD`, `UNAUTHENTICATED`, `FORBIDDEN`, `NOT_FOUND`, `BODY_TOO_LARGE`, `EVENT_CONFLICT`, `QUOTE_MISMATCH`, `STALE_USER_EVIDENCE`, `VERSION_CONFLICT`, `STATE_CONFLICT`, `AMBIGUOUS_TARGET`, `MODEL_UNAVAILABLE`, `INDEX_DEGRADED`, `RATE_LIMITED`, `INTERNAL`。每个端点只返回适用码；`INDEX_DEGRADED` 作为响应字段或在无法安全检索时作为 503 错误，不能偷偷变成空结果。

## 9. 协议升级

`protocol_version=1` 写入编译常量。新增可选字段可以小版本演进；删除字段、改变 scope 来源、改变 forget 语义或状态含义必须升 `/v2`。适配器启动先请求 `/v1/version`，版本不符不注册记忆 Hook 和工具，并给用户可见的配置错误。
