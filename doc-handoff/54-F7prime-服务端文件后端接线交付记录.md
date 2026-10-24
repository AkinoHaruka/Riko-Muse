# 54 · F7′ 服务端文件后端接线交付记录

> 对应任务卡：任务 F7′：完成 memory-server 文件后端接线（Antigravity 执行）  
> 实施日期：2026-10-24  
> 分支：`Riko-Muse`  
> 接手基线：`d4ccdf1`（工作区保留前期半成品未提交状态）  
> 交付状态：`memory-server` 成功通过 alias + shim 转接至纯文件后端 `memory-store-file`；`crates/memory-server/Cargo.toml` 完全移除 `memory-store-sqlite` 依赖；服务端代码内无对 SQLite Store 的直接引用；全量工作区测试 **330 passed / 0 failed**；临时目录 memoryd 独立端口真实 4 项 HTTP 冒烟验证通过。

---

## 1. 任务背景与方案实施

依据 `doc8/07 §2` 与 `hidden_files/briefs/f7prime-seam-brief.md` 定案：
- **目标**：在不重构上万行 HTTP handler、不抽象重型 trait、不引入双后端配置枚举的前提下，将 `memory-server`（`memoryd`）后端由 `memory-store-sqlite` 切换至纯文件存储引擎 `memory-store-file`。
- **选型路线**：采用“模块别名 + compat shim 包装器”路线：
  - 在 `crates/memory-server/Cargo.toml` 中将依赖切换为 `memory-store-file`（完全移除 `memory-store-sqlite`）。
  - 在 `src/store_compat.rs` 中提供兼容包装结构 `pub struct Store { inner: memory_store_file::Store, ... }` 及对应类型与常量别名。
  - 在 `main.rs`、`worker.rs`、`dream_worker.rs` 顶部声明 `use store_compat as memory_store_sqlite;`，零改动所有 HTTP handler 的业务逻辑与契约。
- **停止条件核查**：实施过程证明别名 + shim 路线平稳且完全行得通，未遇到必须大改 handler 的阻碍，**未触发停止条件**。

---

## 2. Shim 最终形态与实现要点

`crates/memory-server/src/store_compat.rs` 共 3693 行，核心转接要点如下：

1. **核心 Store 包装与生命周期**：
   - 包装 `inner: memory_store_file::Store`。
   - `Store::open(db_path, _migrations)` 与 `open_warned`：直接调用 `memory_store_file::Store::open(db_path)`，文件后端无需 SQLite migrations 目录。
   - 实现 `principal_create`、`principal_authenticate`、`principal_list`：直接转调 `inner` 的鉴权与租户接口。

2. **证据写入与记忆提炼（Write Path）**：
   - `record_evidence`：转调 `inner.ingest_evidence`，维持幂等哈希与证据块追加。
   - `remember_memory`：转调 `inner.remember`，由文件引擎完成原子断言与索引构建。
   - `save_candidate_with_reason`：针对后台 worker 提炼历史窗口内非最新事件作为候选的场景，当 `remember` 校验 `StaleUserEvidence` 时，降级通过直接写入 `scope_dir/memory/claims/{id}.md` 并触发 `rebuild_index`，兼顾了候选安全与文件规范。

3. **读取与检索（Read Path）**：
   - `get_memory` / `get_memory_visible`：转调 `inner.get_claim_visible`。
   - `search_memories`：转调 `inner.search_memories`，过滤保留 `h.kind == "claim"` 命中，维护记忆搜索的历史行为契约。
   - `forget_memory` / `retire_memory` / `restore_memory`：转调 `inner.forget`、`inner.retire`、`inner.restore`，并处理二次忘却的幂等读回。

4. **Context Bundle 与衍生段（Segments）**：
   - `context_bundle`：转调 `inner.context_bundle`，生成完备的 `resident`、`retrieved`、`budget` 以及 V2-H1 `segments`（compact / alignment / relationships / retrieved）与 `source_versions`。

5. **后台任务与 Dream 调度模拟**：
   - 保留内存任务队列 `jobs: Arc<Mutex<Vec<JobRow>>>` 与 `dream_jobs: Arc<Mutex<HashMap<String, DreamJobRow>>>`，平滑承接 worker 和 dream_worker 的轮询、认领、失败重试与超时判定。

---

## 3. 收尾核对（Cleanliness Check）

1. **依赖核对**：
   - `riko-memory/crates/memory-server/Cargo.toml` 中仅保留 `memory-store-file.workspace = true`，**已彻底移除 `memory-store-sqlite`**。
2. **源码引用核对**：
   - 服务端代码中无任何直接使用 `memory_store_sqlite::Store` 的残留（所有 `memory_store_sqlite` 命名均经由 `store_compat as memory_store_sqlite` 局部模块别名转接）。
   - `store_compat.rs` 内部 **0 引用** `memory_store_sqlite`，无任何 `rusqlite` 残留。
   - `crates/memory-store-sqlite` 仅作为工作区独立 crate 存在，不被服务端链接。

---

## 4. 自动化测试验证结果

全量工作区测试（`cd riko-memory && cargo test --workspace`）：
```text
test result: ok. 21 passed; 0 failed; finished in 0.02s (memory-contract)
test result: ok. 26 passed; 0 failed; finished in 0.01s (memory-domain)
test result: ok. 2 passed; 0 failed; finished in 0.00s (memory-extract)
test result: ok. 30 passed; 0 failed; finished in 0.03s (memory-file-format)
test result: ok. 9 passed; 0 failed; finished in 0.01s (memory-recall)
test result: ok. 34 passed; 0 failed; finished in 0.75s (memory-server)
test result: ok. 39 passed; 0 failed; finished in 3.70s (memory-store-file)
test result: ok. 169 passed; 0 failed; finished in 62.00s (memory-store-sqlite)
Doc-tests: 0 failed

Total: 330 passed; 0 failed; 0 ignored
```

其中 `crates/memory-server` 自身测试 **34 passed / 0 failed**。

---

## 5. 真实 memoryd 运行与 HTTP 冒烟证据

在 scratch 端口 `38792`、临时文件目录 `/tmp/riko-smoke-8Ka8sW` 启动真实 `memoryd serve`，使用测试 Token 顺序执行四项 HTTP 冒烟验证：

### 1) 服务健康检查 `GET /v1/health`
**Request**:
```bash
curl -s -i "http://127.0.0.1:38792/v1/health"
```
**Response**:
```http
HTTP/1.1 200 OK
content-type: application/json
content-length: 65
date: Sat, 24 Oct 2026 04:35:18 GMT

{"status":"ok","protocol_version":1,"db":"ready","index":"ready"}
```

### 2) 证据写入 `POST /v1/evidence/events`
**Request**:
```bash
curl -s -i \
  -X POST "http://127.0.0.1:38792/v1/evidence/events" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "origin": {
      "host_id": "smoke-host",
      "agent_id": "smoke-agent",
      "session_id": "smoke-session"
    },
    "event_seq": 1,
    "role": "user",
    "source_kind": "user",
    "occurred_at": "2026-10-24T12:00:00Z",
    "content": "烟测事件：今天天气很好"
  }'
```
**Response**:
```http
HTTP/1.1 201 Created
content-type: application/json
content-length: 126
date: Sat, 24 Oct 2026 04:35:18 GMT

{"request_id":"01a16c31-1732-70ce-8c63-8680bde67b44","status":"recorded","evidence_id":"01a16c31-1734-7207-b494-b7a4a02e090f"}
```

### 3) 记忆持久化与读回
#### 3a. `POST /v1/memories/remember`
**Request**:
```bash
curl -s -i \
  -X POST "http://127.0.0.1:38792/v1/memories/remember" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "origin": {
      "host_id": "smoke-host",
      "agent_id": "smoke-agent",
      "session_id": "smoke-session"
    },
    "user_evidence_id": "01a16c31-1734-7207-b494-b7a4a02e090f",
    "quote": "烟测事件：今天天气很好",
    "kind": "fact"
  }'
```
**Response**:
```http
HTTP/1.1 201 Created
content-type: application/json
content-length: 134
date: Sat, 24 Oct 2026 04:35:19 GMT

{"memory_id":"01a16c31-175b-7633-9276-c854e74bc4da","request_id":"01a16c31-1758-7789-a41e-eeece58cb535","status":"active","version":1}
```

#### 3b. 读回验证 `GET /v1/memories/01a16c31-175b-7633-9276-c854e74bc4da`
**Request**:
```bash
curl -s -i \
  -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:38792/v1/memories/01a16c31-175b-7633-9276-c854e74bc4da"
```
**Response**:
```http
HTTP/1.1 200 OK
content-type: application/json
content-length: 389
date: Sat, 24 Oct 2026 04:35:19 GMT

{"request_id":"01a16c31-177f-706e-b20b-8a29dafb7d09","memory_id":"01a16c31-175b-7633-9276-c854e74bc4da","kind":"fact","claim":"烟测事件：今天天气很好","status":"active","version":1,"occurred_at":"2026-10-24T12:00:00.000000Z","valid_until":null,"origin_agent_id":"smoke-agent","evidence_refs":[{"end_byte":33,"evidence_id":"01a16c31-1734-7207-b494-b7a4a02e090f","start_byte":0}]}
```

### 4) 上下文打包 `POST /v1/context/bundle`
**Request**:
```bash
curl -s -i \
  -X POST "http://127.0.0.1:38792/v1/context/bundle" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "agent_id": "smoke-agent",
    "query": "天气"
  }'
```
**Response**:
```http
HTTP/1.1 200 OK
content-type: application/json
content-length: 1320
date: Sat, 24 Oct 2026 04:35:19 GMT

{"agent_id":"smoke-agent","budget":{"total_chars":8000,"used_chars":11},"lexical_status":"ok","page_index_status":"ok","request_id":"01a16c31-1791-762e-8999-49422993d11b","rerank_status":"disabled","resident":{"conflict_ids":[],"items":[{"claim":"烟测事件：今天天气很好","evidence_ids":["01a16c31-1734-7207-b494-b7a4a02e090f"],"kind":"memory","memory_id":"01a16c31-175b-7633-9276-c854e74bc4da","memory_kind":"fact","reason":"pinned","version":1}],"omitted":[],"stale_pages":[],"text":"","truncated":false},"retrieved":{"items":[],"omitted":[],"text":"","truncated":false},"segments":[{"char_count":11,"complete":true,"domain_id":"user_main","items":[{"body":"烟测事件：今天天气很好","observed_or_inferred":"observed","stable_refs":["riko://memory/smoke_tenant/smoke_user/user_main/01a16c31-175b-7633-9276-c854e74bc4da@1"]}],"omitted":0,"segment":"compact","version":1},{"char_count":0,"complete":true,"domain_id":"user_main","segment":"alignment","version":0},{"batch_version":1,"char_count":0,"complete":true,"domain_id":"user_main","items":[],"omitted":0,"segment":"relationships","total":0},{"char_count":0,"complete":true,"deduped":1,"domain_id":"user_main","omitted":0,"segment":"retrieved","text":""}],"semantic_status":"disabled","source_versions":{"01a16c31-175b-7633-9276-c854e74bc4da":1}}
```

---

## 6. 验证档位与安全边界

- **验证档位**：
  - 静态编译与代码检查：`cargo check -p memory-server` 通过（0 errors）。
  - 单元与集成测试：`cargo test --workspace` **330 passed / 0 failed**。
  - 本地 HTTP 冒烟验证：通过（4 项核心端点请求与响应完全正常，见上节证据）。
  - 真实模型连通 / 生产服务器部署：**未执行 / 未触碰**（无真实模型依赖，未触碰生产库与服务器）。
