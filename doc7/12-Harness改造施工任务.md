# DSH Harness 改造施工任务（dsh-adapter/ 新结构）

> 前置：F1-F4 完成 → review 通过 → `agent-memory/` 重命名为 `riko-memory/` → `adapters/dsh/` 移到顶层 `dsh-adapter/`
> 语言：纯 TypeScript，只调 riko-memory 的 HTTP API，不碰 Rust
> 模型：Antigravity `gemini-3.8-flash-medium`，effort `medium`
> 每个任务独立 commit，不 push，`npm test` 通过才提交

---

## H4：twin-soul 人格注入

**目标**：对标 Muse 的 SOUL.md 机制，twin 通过 system prompt 拥有稳定人格。

**实现**：
- 在 `dsh-adapter/src/soul.ts` 新建模块
- 使用 `ctx.systemPrompt.section({name: 'twin-soul', order: 100, text: ...})` 注册人格 section
- 人格内容从配置文件读取（`soul.md`，用户可编辑），支持热重载
- 人格包含：名字、说话风格、与用户的关系定位、行为准则

**验收**：
- 启动后 system prompt 包含 twin-soul section
- 修改 soul.md 后无需重启生效（或重启后生效，文档注明）
- 测试：section 正确注册、内容正确注入、order 不与其他 section 冲突

**参考**：逆向文档 04-上下文管理 §4.1

---

## H5：相处指南注入（alignment synthesis）

**目标**：对标 Muse 的 ALIGNMENT_SYNTHESIS.md，twin 记住与用户的相处模式。

**实现**：
- 在 `dsh-adapter/src/alignment.ts` 新建模块
- 使用 `agent/pre-step` hook，在每轮对话前注入相处指南
- 指南内容从配置文件读取（`alignment.md`），包含：用户偏好、沟通风格、边界、当前关系状态
- 注入位置：在记忆注入之后、人格注入之前（order 协调）

**验收**：
- 每轮 pre-step 都能看到 alignment 内容
- 测试：注入顺序正确、内容更新后生效

**参考**：逆向文档 04-上下文管理 §4.2、01-Agent主循环

---

## H8：schedule 定时任务对接

**目标**：对标 Muse 的 cron 机制，twin 支持定时任务。

**实现**：
- 在 `dsh-adapter/src/schedule.ts` 新建模块
- 封装 DSH 的 `ctx.schedule`（或对应 API），提供类 Muse 的接口：
  - `schedule_create`：支持 one-shot、daily、weekly、cron 表达式
  - `schedule_list` / `schedule_update` / `schedule_delete`
- 到期任务以普通消息形式回到原对话（对标 Muse 行为）
- 定时任务触发时，自动注入记忆上下文（调 riko-memory `/v1/context/compose`）

**验收**：
- 创建、列出、更新、删除定时任务都能工作
- 到期任务正确触发并回到对话
- 测试：各种调度类型的触发、任务持久化（重启不丢）

**参考**：逆向文档 06-定时任务

---

## H9-micro：compaction 微改（摘要质量 + 注入格式）

**目标**：DSH 原生有 compaction，不重写，只调两处对标 Muse。

**实现**：
- **摘要质量**：定制摘要 prompt，确保保留关键决策、待办、用户偏好（参考 Muse 摘要的保留项）
- **注入格式**：压缩后注入的不只是摘要，还有记忆快照 + 目标状态（对标 Muse 的 standing context 注入顺序）
- 在 `dsh-adapter/src/compaction.ts` 新建模块，hook DSH 的 compaction 事件

**验收**：
- 实际跑一次完整压缩，检查摘要是否保留关键信息
- 注入格式与 Muse 一致（摘要 + 记忆 + 目标）
- 测试：摘要不丢关键决策、注入顺序正确

**注意**：先实测 DSH 默认效果，有问题再改，没问题就不碰。

**参考**：逆向文档 07-Session与事件 §7.1、04-上下文管理

---

## H11：tools/pre-execute 敏感工具拦截

**目标**：对标 Muse 的 approval 机制，twin 对敏感工具有人格化审批。

**实现**：
- 在 `dsh-adapter/src/approval.ts` 新建模块
- 监听 `tools/pre-execute` waterfall 事件
- 对敏感工具（如文件删除、网络请求、支付）进行拦截
- 拦截时向用户展示人格化的确认提示（不是干巴巴的 "Allow?"）
- 用户确认后放行，拒绝后 deny

**验收**：
- 敏感工具触发时正确拦截
- 非敏感工具不受影响
- 测试：拦截逻辑、用户确认流程、deny 后行为

**参考**：逆向文档 02-工具系统 §2.2

---

## H12：agent/request-error 定制重试

**目标**：模型调用失败时，twin 有智能重试策略（不是无脑重试）。

**实现**：
- 在 `dsh-adapter/src/retry.ts` 新建模块
- 监听 `agent/request-error` waterfall 事件
- 重试策略：
  - 429/503：指数退避，最多 3 次
  - 401/403：不重试，直接报错（认证问题）
  - 超时：重试 1 次，延长超时时间
  - 其他：重试 1 次
- 重试时记录日志，方便排查

**验收**：
- 各种错误类型按策略正确处理
- 测试：模拟各种错误，验证重试行为

**参考**：逆向文档 01-Agent主循环 §1.2

---

## 施工顺序

1. H4（人格）→ H5（相处）→ H11（审批）→ H8（定时）→ H12（重试）→ H9-micro（压缩）
2. H4/H5 先做，因为后面的任务都依赖人格上下文
3. H9-micro 最后，因为要实测

## 通用要求

- 每个任务：实现 → `npm run build` → `npm test` → 独立 commit
- commit message 格式：`H4: twin-soul personality injection`（以此类推）
- 不 push，等 Riko review
- 遇到 DSH API 与逆向文档不符，先查源码，查不到就问 Riko，不要猜
