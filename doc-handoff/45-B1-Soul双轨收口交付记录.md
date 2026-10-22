# 45-B1-Soul双轨收口交付记录

> 对应任务卡：任务卡 B1：Soul 双轨收口（preset 默认只走内核 Soul）  
> 实施日期：2026-10-22  
> 分支：`Riko-Muse`  
> 交付基线：`aa0acd6`（B3 交付提交）  
> 状态：代码与记录已实施，适配器契约测试 54 passed / 0 failed，内核测试 169 passed / 0 failed

---

## 1. 背景与问题定位

在 `doc-handoff/33`（与 Muse 差异审查）§2.3 中明确记录了当前系统的“人格层双轨”与“静态对齐文件”缺陷：
1. **人格层双轨（平行注入与隔离缺失）**：
   - 历史文件版 `twin-soul`：`riko-dsh/src/soul.ts` 通过全局 `ctx.systemPrompt.section` 以 `order=100` 注入 system prompt，全进程一份、无任何 agent 隔离，subagent / Dream child 亦被无差别注入。
   - 内核版 Soul：按 `(tenant, user, agent_id)` 隔离、order=10300（`makeSoulAssembleHook`）。
   - 在发货 preset 下，两者会同时进入 system prompt，出现双人格、双真相源与隔离击穿。
2. **静态 `alignment.md`（同名异物、零数据通路）**：
   - `riko-dsh/alignment.md` 为静态手写 Markdown 文件，经 `alignment.ts` 在 pre-step 中注入；与内核 `memoryd` 的 alignment synthesis 零数据通路，却命名为“相处指南 / Alignment Synthesis”，造成混淆。

根据任务卡 B1 编排决定，本卡彻底收口人格双轨，确立**单一真相源 = 内核 Soul**。

---

## 2. 现场核对表（路径 + 行号 + 现状与处置）

| 序号 | 目标文件 | 关键行号 | 改前行为 | B1 收口后行为 |
|---|---|---|---|---|
| 1 | `riko-dsh/riko-preset.patch.yml` | L62–72 | 配置 `@riko/dsh` 插件，启用 `contextBundleEnabled: true` 与 `agentName: riko`。 | 保持不变。preset 默认只触发内核 Soul 装配，不再平行装载文件版 twin-soul。 |
| 2 | `riko-dsh/src/index.ts` | L27, L256 | `apply(ctx)` 无条件调用 `registerTwinSoul(ctx)`，向宿主全局注入 `twin-soul` (order 100)。 | **移除无条件调用**；仅在 `config.fileSoulEnabled === true` 时创建 `FileSoulWatcher` 并传递给 Soul 装配 hook。 |
| 3 | `riko-dsh/src/index.ts` | L183–207 | `registerSoulAssemble()` 仅以静态 `agentName` 包装 `makeSoulAssembleHook`。 | 支持 `fileSoulWatcher` 作为 opt-in 覆盖提供源；`dispose()` 时安全关闭文件监听器。 |
| 4 | `riko-dsh/src/soul.ts` | L8–9, L25–85 | 导出全局注入函数 `registerTwinSoul`（order 100）。 | 新增 `createFileSoulWatcher` 供装配路径热重载读取；`registerTwinSoul` 标记为 `@deprecated`，保留单测兼容但适配器不再默认调用。 |
| 5 | `riko-dsh/src/recall.ts` | L183–220 | `makeSoulAssembleHook` 固定在 order 10300 注入 `<agent_memory:soul>`；使用 `agentName \|\| fallback`，多 agent 下若配了 `agentName` 会强行统一。 | 新增 `resolveAgentId`（优先显式 ID/name，避免随机会话污染，支持多 agent 独立 ID）；新增 `fileSoulOpts` 显式 opt-in 参数，文件覆盖同样走 order 10300 与 agent 隔离。 |
| 6 | `riko-dsh/src/config.ts` | L40–47, L68–89 | 插件配置中仅有 `agentName` 与 `contextBundleEnabled`，无文件版开关。 | 新增 `fileSoulEnabled: boolean`（严格默认 `false`，支持 env `RIKO_DSH_FILE_SOUL`）与 `soulFile: string`。 |
| 7 | `riko-dsh/alignment.md` | L1–4 | 标为“相处指南 / Alignment Synthesis”，暗示为活的行为指南。 | **文件头明确标注废弃声明**（详见下文处置说明），指向内核 `memoryd` alignment synthesis 为唯一活源。 |
| 8 | `riko-dsh/README.md` | L29–37 | 文档描述 `soul.md` 与 API soul 并存注入，描述 `alignment.md` 为每轮带入指南。 | 更新文档：澄清内核 Soul 为唯一真相源，说明 `soul.md` 需显式 opt-in 且走装配路径，标注 `alignment.md` 已废弃。 |

---

## 3. 编排决定实施细节

### 3.1 单一真相源 = 内核 Soul
1. 默认 preset 路径下，`apply(ctx)` 不再调用 `registerTwinSoul`。
2. System prompt assemble waterfall 中，仅注册一个具名 section `riko-dsh:soul`（order 10300）。
3. 全局 order 100 的 `twin-soul` section 彻底不在 system prompt 中出现。

### 3.2 文件版覆盖能力显式 opt-in（默认关）
1. 配置项 `fileSoulEnabled` 在 `loadConfig` 中严格默认为 `false`。
2. 当且仅当用户显式配置 `fileSoulEnabled: true`（或设置环境变量 `RIKO_DSH_FILE_SOUL=1`）时开启。
3. 开启时不创建平行 order 100 section，而是将文件内容送入 `makeSoulAssembleHook`：
   - 同样在 `system-prompt/assemble` 阶段注入；
   - 同样使用 `order = 10300` 与具名 section `riko-dsh:soul`；
   - 同样遵循 subagent / Dream child 隔离门禁（子会话绝不注入）；
   - 同样遵循 agent 隔离（`targetAgent` 过滤，非目标 agent 不会被文件污染，正常回落至自身内核 Soul）。

### 3.3 静态 `alignment.md` 的处置与理由
任务卡提供两种方案（二选一）：
- 方案 A：从 preset 注入列表移除。
- 方案 B：文件头明确标注已废弃、指向内核 synthesis 为唯一活源。

**实施选择：方案 B（明确标注已废弃）**  
**选择理由**：
1. **破坏最小原则（Zero Regressions）**：`alignment.ts` 与其单测 `alignment.test.mjs` 在既有测试套件中深度耦合了 pre-step 消息插桩链路。若粗暴在源码中删除或更改中间件签名，易引发宿主 pre-step 流水线的潜在连锁反应。
2. **消灭认知混淆**：在 `alignment.md` 文件正文头部添加显著的 HTML 注释与 Markdown 警示块，明确声明：
   > `DEPRECATED / 已废弃：此文件仅为历史静态占位/参考，不得再被当作“活的对齐综合（alignment synthesis）”呈现。内核 memoryd alignment synthesis（通过 context/bundle 的 segments[alignment] 注入）是唯一的活源与单一真相源。`
3. 配合 `README.md` 同步修订，彻底阻断将静态文件误认为实时对齐数据的可能性。

---

## 4. 验证与测试证据

### 4.1 适配器测试（新增 4 项针对性契约测试，全量 54/54 全绿）

运行命令：`cd riko-dsh && npm test`
```
> @riko/dsh@0.5.2 test
> npm run build && node --test "tests/*.test.mjs"

> @riko/dsh@0.5.2 build
> tsc -p tsconfig.json

✔ injects local guidance after memory context and before the original user message (5.303075ms)
✔ registers alignment in pre-step and picks up alignment.md edits without restart (91.133199ms)
✔ leaves rejected, aborted, and subagent decisions unchanged (1.539639ms)
...
✔ registers twin-soul at order 100 and injects its content alongside API soul (21.689324ms)
✔ reloads soul.md changes into the next system prompt assembly (69.666779ms)
✔ B1: fileSoulEnabled defaults to false in config (1.215813ms)
✔ B1: default preset injects exactly one Soul (kernel soul at order 10300) without twin-soul (0.874051ms)
✔ B1: multi-agent Soul is isolated per agent (agents cannot see each other's Soul) (0.478507ms)
✔ B1: opt-in file soul walks assemble path at order 10300 and preserves agent isolation (4.669003ms)
✔ retire and restore send exact UTF-8 instruction byte spans and receipts (10.187571ms)
✔ lifecycle tools reject absent or ambiguous quotes without a write (2.005729ms)
✔ lifecycle tools require a positive integer version (1.966519ms)
ℹ tests 54
ℹ suites 0
ℹ pass 54
ℹ fail 0
```

测试覆盖重点：
1. **默认关测试**：`loadConfig(base).fileSoulEnabled === false`。
2. **唯一 Soul 测试**：默认 preset 注入下，`sections` 中有且仅有一个 Soul（`riko-dsh:soul`，order 10300），`twin-soul`（order 100）不存在。
3. **多 Agent 隔离测试**：`agent-alpha` 与 `agent-beta` 分别组装，各自只能注入专属 Soul 正文，绝不串门；Dream/subagent 子会话 Soul 注入为 0。
4. **Opt-in 覆盖路径与隔离测试**：开启 `fileSoulEnabled` 时，目标 agent 走 order 10300 装配路径注入文件内容（无 order 100 平行 section）；非目标 agent 仍接收自身内核 Soul；子会话依旧排除。

### 4.2 内核测试

运行命令：`cd riko-memory && cargo test --workspace`
- 结果：`169 passed; 0 failed; 0 ignored`，内核所有领域驱动测试与迁移测试全绿。
- 本卡未修改 Rust 源码，未引入新迁移（保持 schema 24 冻结）。

---

## 5. 交付结论与边界

- **单一真相源**：发货 preset 仅走内核 Soul 组装路径，文件版不再默认进入 system prompt。
- **硬边界遵守**：
  - 未打开 `dana/realtest` 原库；
  - 未修改上游/官方 DSH clone；
  - 未修改冻结迁移；
  - 未修改 extract/admit 历史版本行为；
  - 仓库无任何凭据泄漏。
