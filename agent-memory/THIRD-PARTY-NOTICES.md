# 第三方声明（THIRD-PARTY-NOTICES）

本项目为独立实现，未复制三个上游仓库（EverOS / Hindsight / TencentDB Agent Memory）的源文件或 Prompt。
设计借鉴关系见 `doc/02-上游源码对照.md` 与 `doc/10-开发冻结规范.md` 的 G 节；
将来若复制实质代码，必须在此保留相应版权与许可通知（EverOS: Apache-2.0；Hindsight: MIT；TencentDB Agent Memory: 以仓库内 LICENSE 为准）。

## 直接依赖（Rust crates，构建时解析于 Cargo.lock）

- rusqlite（bundled SQLite）：MIT，https://github.com/rusqlite/rusqlite
- SQLite（bundled 编译）：public domain，https://www.sqlite.org/
- tokio / axum：MIT
- serde / serde_json：MIT / Apache-2.0
- clap：MIT / Apache-2.0
- uuid、chrono、sha2、hex、base64、rand、unicode-normalization、toml、thiserror：以 Cargo.lock 实际解析版本为准（MIT / Apache-2.0 / BSD 等）

## 依赖（DSH 适配器）

- 仅 TypeScript 编译器与 DSH 宿主 API；无运行时第三方库。

完整版本清单以 `Cargo.lock` 与 `adapters/dsh/package.json` 为权威来源。
