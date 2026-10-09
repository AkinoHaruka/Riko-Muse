# twin-memory

[English](README.md) | 中文

此插件入口当前为空。它不会读取 Session 事件、注册工具或发起网络请求。

## 职责与后续对接

此模块负责 Twin 组合包中的记忆行为。后续对接点是 Riko-Memory 的 `memoryd` HTTP API，包括 `POST /v1/context/compose`；此处尚未实现传输、认证和错误处理。
