# twin-companion

[English](README.md) | 中文

此插件入口当前为空。它不会更改或拒绝 agent 回合。

## 职责与后续对接

此模块负责 Twin 组合包中的陪伴行为。后续对接点是 `agent/pre-step` 事件，可以在此添加陪伴指导而无需替换默认 agent loop。
