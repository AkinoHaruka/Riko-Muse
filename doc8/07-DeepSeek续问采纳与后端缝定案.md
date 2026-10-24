# 07 DeepSeek 续问采纳与后端缝定案（2026-10-24）

来源：consult-deepseek-v41flash-20261024.md 续问部分（经主人登录的网页端问得，D 尾+E+后端缝三题全回）。按指挥链，它说什么就是什么。

## D 尾与 E 的新增定案（F5′ 之后的新增量，后续以 F6′ 落地）

1. precedence 必须与 slot/topic 成对出现（无 slot 不谈冲突）；枚举 session/project/global，默认 global；session 不落库，project 需显式绑定，global 需用户确认。
2. 冲突裁决链：本轮显式指令 > 用户显式更正 > session > project > global > 无记忆；同 slot 同层取 updated_at 新者；仍冲突则不注入冲突项并标注。
3. 时间分工：updated_at（记录时间）与 valid_from/valid_to（事件有效窗）分开；事件类记忆注入必带有效窗。
4. 召回排序：相关性/当前任务 > 显式 pin/会话指令 > precedence > 新近度；superseded 默认不注入正文，只留更正链摘要。
5. 覆盖信号三层：按 id、按 slot、整块；"忽略记忆"默认只对当前轮/会话生效。块尾 override_signal 必备，语义含默认 none 与系统指令优先。
6. 注入块骨架定死：版本头（version/generated_at/timezone）→ priority（在 memories 之前，写明裁决链与"不覆盖系统/开发者指令、本轮用户指令优先"）→ memories（每条单行：id, status, precedence, slot, source, updated_at, [valid_from, valid_to], [superseded_by], confidence, | 内容）→ conflict_notes（有冲突必有）→ override_signal（块尾）。外壳与分隔符、slot 命名、confidence、时间格式可调，硬字段不省、内容放最后。

## 后端缝定案（后续以 F7′ 落地）

- 选垫片方案再简化：memory-server 内 `pub type Store = memory_store_file::Store;` 先行；编译不过再建 `memory-server/src/store_compat.rs` shim，按编译错误逐个补转调方法。shim 只住服务端。
- 不做 trait、不做 feature 双后端、不做运行时配置选后端；不先改 handler；不加配置枚举。
- 迁移完成即从 memory-server 移除 memory-store-sqlite 依赖（crate 本体保留作旧库冻结）；不留"可切换后端"的假缝。回滚靠版本与部署。
