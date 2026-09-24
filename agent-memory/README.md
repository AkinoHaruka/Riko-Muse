# Agent Memory（memoryd）

Rust 优先的通用 Agent 长期记忆内核。设计契约见 `doc/`（v1 冻结：doc/10；实现契约：doc/11–15）。

**当前状态：卡 0/卡 1 骨架**——workspace、迁移、令牌认证、`/v1/health` 与 `/v1/version` 已就绪；
evidence ingest、记忆读写、检索与提取作业在卡 2–5 交付。详见 `doc/15-开发任务卡与最小验收.md`。

## 快速开始（本机，Windows 已验证）

```bash
# 1. 配置（非秘密项）
cp config.example.toml config.toml

# 2. 创建用户令牌（令牌只显示一次，写入新文件）
cargo run -p memory-server --bin memoryd -- principal add \
  --tenant local --user alice --token-out alice.token --db data/agent-memory.db

# 3. 启动内核（只监听 127.0.0.1:8791）
cargo run -p memory-server --bin memoryd -- serve --config config.toml

# 4. 检查
curl http://127.0.0.1:8791/v1/version
curl http://127.0.0.1:8791/v1/health
```

## 目录

- `crates/memory-contract`：协议常量、错误码、v1 默认限额
- `crates/memory-domain`：作用域、状态机、`normalize_v1`、claim 哈希
- `crates/memory-store-sqlite`：迁移执行器、principals、事务存储
- `crates/memory-extract`：模型适配协议与候选校验（卡 4）
- `crates/memory-recall`：检索与上下文编译（卡 3）
- `crates/memory-server`：`memoryd` 二进制（CLI + HTTP）
- `adapters/dsh`：DSH TypeScript 薄适配器（卡 2/3）
- `migrations/0001_init.sql`：规范库蓝图（doc/11）

## 安全边界

- 请求正文不接受 `tenant_id/user_id`；scope 只由 Bearer 令牌在服务端解析（doc/12 §1）。
- 令牌与模型密钥严禁写入仓库、配置或日志（doc/09）。
- 首版只监听 loopback；不声称跨用户进程内身份切换能力（doc/10 C 节）。
