//! memoryd 内核进程：CLI、配置、HTTP 生命周期（doc/09）。
//!
//! 启动顺序：验证 loopback 绑定 → 读取配置 → 打开数据库与 WAL → 校验并执行迁移
//! → 检查索引状态 → 开放 HTTP。模型端点暂时不可达可启动服务（doc/09）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Json, Path as AxumPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Extension;
use axum::Router;
use clap::{Parser, Subcommand};
use memory_contract::{
    ErrorCode, ErrorResponse, HealthResponse, VersionResponse, COMPOSE_MAX_CHARS_DEFAULT,
    COMPOSE_MAX_ITEMS_DEFAULT, EVIDENCE_CONTENT_MAX_BYTES, HOST_ID_MAX_CHARS, PROTOCOL_VERSION,
    SCHEMA_VERSION, SEARCH_QUERY_MAX_CHARS,
};
use memory_domain::{MemoryKind, Origin, ScopeKey};
use memory_store_sqlite::{
    ComposeResult, FlushOutcome, IngestOutcome, JobRow, RememberOutcome, SearchHit, Store,
    StoreError,
};
use serde::Deserialize;
use uuid::Uuid;

mod worker;

/// 构建标识：优先取编译期注入的 commit，否则 "dev"。
const BUILD: &str = match option_env!("AGENT_MEMORY_BUILD") {
    Some(v) => v,
    None => "dev",
};

/// 每个请求的服务端 request ID（doc/12 §1）。
#[derive(Debug, Clone)]
struct RequestId(String);

#[derive(Parser)]
#[command(name = "memoryd", about = "Agent Memory 内核（v1 冻结契约 doc/10-15）")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动内核服务（只监听 loopback）
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// 创建用户令牌（32 字节随机，base64url 写入新文件，数据库只存 SHA-256）
    Principal {
        #[command(subcommand)]
        action: PrincipalAction,
    },
    /// 只读诊断：迁移、principals、索引状态（不修复数据）
    Doctor {
        #[arg(long)]
        config: PathBuf,
    },
    /// 从 active 规范表重建 FTS/grams（不复活 forgotten）
    RebuildIndex {
        #[arg(long)]
        config: PathBuf,
    },
    /// SQLite 在线一致性备份（VACUUM INTO）
    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum PrincipalAction {
    Add {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        token_out: PathBuf,
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        migrations: PathBuf,
    },
    RotateToken {
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        token_out: PathBuf,
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value = "migrations")]
        migrations: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    listen_addr: String,
    db_path: PathBuf,
    migrations_dir: PathBuf,
    /// OpenAI 兼容提取端点（http://host:port/path）。未配置则不启动提取 worker。
    model_endpoint: Option<String>,
    model_name: Option<String>,
    /// 模型密钥文件路径（密钥不进配置、不进日志）。
    model_key_file: Option<PathBuf>,
    /// 是否允许空密钥（doc2/05 §2：默认 false，不能把空字符串当真实授权）。
    model_allow_empty_key: Option<bool>,
}

impl Config {
    fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置文件失败 {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("解析配置文件失败 {}: {e}", path.display()))?;
        cfg.validate()
    }

    fn validate(&self) -> Result<Self, String> {
        let addr: SocketAddr = self
            .listen_addr
            .parse()
            .map_err(|e| format!("listen_addr 不是合法地址 {}: {e}", self.listen_addr))?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "listen_addr={} 不是 loopback；首版只监听 127.0.0.1",
                self.listen_addr
            ));
        }
        Ok(self.clone())
    }
}

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => {
            let cfg = Config::load(&config)?;
            let store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            eprintln!(
                "[memoryd] 迁移完成：{}",
                store.doctor_summary().unwrap_or_else(|e| format!("诊断失败: {e}"))
            );
            let state = AppState { store: Arc::new(Mutex::new(store)) };
            // 提取 worker：模型配置齐全才启动；端点不可达时作业可见失败，不影响手工记忆（doc/09）。
            let model_cfg = match (&cfg.model_endpoint, &cfg.model_name, &cfg.model_key_file) {
                (Some(endpoint), Some(name), Some(key_file)) => {
                    let api_key = std::fs::read_to_string(key_file)
                        .map_err(|e| format!("读取模型密钥文件失败 {}: {e}", key_file.display()))?
                        .trim()
                        .to_string();
                    // doc2/05 §2：空 key 是否允许由配置显式决定，不能把空字符串当真实授权。
                    if api_key.is_empty() && !cfg.model_allow_empty_key.unwrap_or(false) {
                        return Err("模型密钥文件为空；如确需无密钥端点请显式设置 model_allow_empty_key=true".into());
                    }
                    Some(worker::ModelConfig {
                        endpoint: endpoint.clone(),
                        model: name.clone(),
                        api_key,
                        timeout: std::time::Duration::from_secs(
                            memory_contract::MODEL_CALL_TIMEOUT_SECS,
                        ),
                    })
                }
                _ => None,
            };
            worker::spawn_worker(state.clone(), model_cfg);
            let addr: SocketAddr = cfg.listen_addr.parse().expect("配置已校验为 loopback");
            let app = Router::new()
                .route("/v1/health", get(health))
                .route("/v1/version", get(version))
                .route("/v1/evidence/events", post(ingest_events))
                .route("/v1/extraction/flush", post(flush_window))
                .route("/v1/jobs/{job_id}", get(get_job))
                .route("/v1/jobs/{job_id}/retry", post(retry_job))
                .route("/v1/memories/remember", post(remember_memory))
                .route("/v1/memories/search", post(search_memories))
                .route("/v1/memories/{memory_id}", get(get_memory))
                .route("/v1/memories/{memory_id}/correct", post(correct_memory))
                .route("/v1/memories/{memory_id}/forget", post(forget_memory))
                .route("/v1/context/compose", post(compose_context))
                .layer(middleware::from_fn_with_state(state.clone(), request_pipeline))
                .with_state(state);
            eprintln!("[memoryd] 监听 {addr}（loopback only）");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        Commands::Principal { action } => match action {
            PrincipalAction::Add { tenant, user, token_out, db, migrations } => {
                let mut store = Store::open(&db, &migrations)?;
                store.principal_add(&tenant, &user, &token_out)?;
                println!("已创建 principal tenant={tenant} user={user}，令牌写入 {}", token_out.display());
                Ok(())
            }
            PrincipalAction::RotateToken { tenant, user, token_out, db, migrations } => {
                let mut store = Store::open(&db, &migrations)?;
                store.principal_rotate_token(&tenant, &user, &token_out)?;
                println!("已轮换 tenant={tenant} user={user} 的令牌，原令牌立即失效，新令牌写入 {}", token_out.display());
                Ok(())
            }
        },
        Commands::Doctor { config } => {
            let cfg = Config::load(&config)?;
            let store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            println!("{}", store.doctor_summary()?);
            Ok(())
        }
        Commands::RebuildIndex { config } => {
            let cfg = Config::load(&config)?;
            let mut store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            let (fts, grams) = store.rebuild_index()?;
            println!("rebuild-index 完成：fts_rows={fts} grams_rows_deleted={grams}");
            Ok(())
        }
        Commands::Backup { config, out } => {
            let cfg = Config::load(&config)?;
            let mut store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            store.backup_to(&out)?;
            println!("备份完成：{}", out.display());
            Ok(())
        }
    }
}

fn err(req_id: &str, status: StatusCode, code: ErrorCode, msg: &str) -> Response {
    (status, Json(ErrorResponse::new(req_id, code, msg))).into_response()
}

/// 请求管线：生成/校验 request ID；除 health/version 外执行 Bearer 认证（doc/12 §1）。
async fn request_pipeline(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| Uuid::parse_str(v).is_ok())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    req.extensions_mut().insert(RequestId(request_id));

    let path = req.uri().path().to_string();
    if path == "/v1/health" || path == "/v1/version" {
        return next.run(req).await;
    }

    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());

    let scope = match token {
        Some(t) => {
            let result = state
                .store
                .lock()
                .map_err(|_| ())
                .and_then(|store| store.verify_token(t).map_err(|_| ()));
            match result {
                Ok(Some(scope)) => scope,
                _ => {
                    let rid = req
                        .extensions()
                        .get::<RequestId>()
                        .map(|r| r.0.clone())
                        .unwrap_or_default();
                    return err(&rid, StatusCode::UNAUTHORIZED, ErrorCode::Unauthenticated, "令牌无效或用户已停用");
                }
            }
        }
        None => {
            let rid = req
                .extensions()
                .get::<RequestId>()
                .map(|r| r.0.clone())
                .unwrap_or_default();
            return err(&rid, StatusCode::UNAUTHORIZED, ErrorCode::Unauthenticated, "缺少 Bearer 令牌");
        }
    };
    req.extensions_mut().insert(scope);
    next.run(req).await
}

// ---- POST /v1/evidence/events（doc/12 §3）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestRequest {
    origin: OriginDto,
    event_seq: i64,
    role: String,
    source_kind: String,
    occurred_at: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginDto {
    host_id: String,
    agent_id: String,
    session_id: String,
}

#[derive(Debug, serde::Serialize)]
struct IngestResponse {
    request_id: String,
    status: &'static str,
    evidence_id: String,
}

fn validate_origin(dto: OriginDto) -> Result<Origin, (&'static str, &'static str)> {
    for (_name, v) in [
        ("host_id", &dto.host_id),
        ("agent_id", &dto.agent_id),
        ("session_id", &dto.session_id),
    ] {
        if v.is_empty() {
            return Err(("INVALID_FIELD", "origin ID 不能为空"));
        }
        if v.chars().count() > HOST_ID_MAX_CHARS {
            return Err(("INVALID_FIELD", "origin ID 超过 256 字符"));
        }
    }
    Ok(Origin {
        host_id: dto.host_id,
        agent_id: dto.agent_id,
        session_id: dto.session_id,
    })
}

async fn ingest_events(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<IngestRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        // JSON 语法错误 → INVALID_JSON；反序列化失败（含未知字段/类型错）→ INVALID_FIELD（doc/12 §1/§8）。
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_code, msg)) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg),
    };
    if body.event_seq < 0 {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "event_seq 必须非负");
    }
    if !matches!(body.role.as_str(), "user" | "assistant" | "tool" | "system") {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "role 必须是 user/assistant/tool/system");
    }
    if !matches!(body.source_kind.as_str(), "user" | "assistant" | "tool" | "plugin" | "system") {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "source_kind 必须是 user/assistant/tool/plugin/system");
    }
    let occurred_at = match chrono::DateTime::parse_from_rfc3339(&body.occurred_at) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "occurred_at 必须是带时区的 RFC3339 时间")
        }
    };
    if body.content.is_empty() || body.content.len() > EVIDENCE_CONTENT_MAX_BYTES {
        return err(&req_id.0, StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge, "content 必须 1～64 KiB");
    }

    let outcome = {
        let mut guard = state.store.lock().unwrap();
        guard.record_evidence(
            &scope,
            &origin,
            body.event_seq,
            &body.role,
            &body.source_kind,
            &occurred_at,
            &body.content,
        )
    };
    match outcome {
        Ok(IngestOutcome::Recorded(id)) => (
            StatusCode::CREATED,
            Json(IngestResponse { request_id: req_id.0, status: "recorded", evidence_id: id }),
        )
            .into_response(),
        Ok(IngestOutcome::AlreadyRecorded(id)) => (
            StatusCode::OK,
            Json(IngestResponse { request_id: req_id.0, status: "already_recorded", evidence_id: id }),
        )
            .into_response(),
        Err(StoreError::EventConflict) => {
            err(&req_id.0, StatusCode::CONFLICT, ErrorCode::EventConflict, "事件键已存在且内容哈希不同")
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- POST /v1/memories/remember（doc/12 §5）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberRequest {
    origin: OriginDto,
    user_evidence_id: String,
    quote: String,
    kind: String,
}

async fn remember_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<RememberRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_, msg)) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg),
    };
    let kind = match body.kind.as_str() {
        "fact" => MemoryKind::Fact,
        "preference" => MemoryKind::Preference,
        "instruction" => MemoryKind::Instruction,
        "episode" => MemoryKind::Episode,
        _ => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "kind 必须是 fact/preference/instruction/episode"),
    };
    let quote_chars = body.quote.chars().count();
    if quote_chars == 0 || quote_chars > 512 {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "quote 必须 1～512 个 Unicode 标量字符");
    }
    let outcome = {
        let mut guard = state.store.lock().unwrap();
        guard.remember(&scope, &origin, &body.user_evidence_id, &body.quote, kind)
    };
    match outcome {
        Ok(RememberOutcome::Created { memory_id, version }) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "request_id": req_id.0,
                "memory_id": memory_id,
                "version": version,
                "status": "active"
            })),
        )
            .into_response(),
        Ok(RememberOutcome::Dedup { memory_id, version }) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "request_id": req_id.0,
                "memory_id": memory_id,
                "version": version,
                "status": "active",
                "deduplicated": true
            })),
        )
            .into_response(),
        Err(StoreError::QuoteMismatch) => err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::QuoteMismatch, "quote 不是该用户消息的连续原文子串"),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => {
            err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StaleUserEvidence, "引用的用户证据不是该会话最新用户事件或角色不符")
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- GET /v1/memories/{id}（doc/12 §5）----

#[derive(Debug, serde::Serialize)]
struct MemoryDetailResponse {
    request_id: String,
    memory_id: String,
    kind: String,
    claim: String,
    status: String,
    version: i64,
    occurred_at: Option<String>,
    valid_until: Option<String>,
    origin_agent_id: String,
    evidence_refs: Vec<serde_json::Value>,
}

async fn get_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(memory_id): AxumPath<String>,
) -> Response {
    let row = state.store.lock().unwrap().get_memory(&scope, &memory_id);
    match row {
        Ok(Some(m)) => {
            let refs: Vec<serde_json::Value> = m
                .evidence_refs
                .into_iter()
                .map(|(evidence_id, start, end)| {
                    serde_json::json!({"evidence_id": evidence_id, "start_byte": start, "end_byte": end})
                })
                .collect();
            Json(MemoryDetailResponse {
                request_id: req_id.0,
                memory_id: m.memory_id,
                kind: m.kind,
                claim: m.claim,
                status: m.status,
                version: m.version,
                occurred_at: m.occurred_at,
                valid_until: m.valid_until,
                origin_agent_id: m.origin_agent_id,
                evidence_refs: refs,
            })
            .into_response()
        }
        Ok(None) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "记忆不存在或不可见"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- POST /v1/memories/search（doc/12 §7）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    query: String,
    limit: Option<usize>,
    include_history: Option<bool>,
}

async fn search_memories(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<SearchRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let q_chars = body.query.chars().count();
    if q_chars == 0 || q_chars > SEARCH_QUERY_MAX_CHARS {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "query 必须 1～2048 个 Unicode 标量字符");
    }
    let limit = body.limit.unwrap_or(5);
    if !(1..=20).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～20");
    }
    let include_history = body.include_history.unwrap_or(false);
    if include_history && !memory_store_sqlite::has_history_cue(&body.query) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "include_history=true 要求 query 含明确历史词（以前/过去/曾经/当时 等）");
    }
    let result = state.store.lock().unwrap().search_memories(&scope, &body.query, limit, include_history);
    match result {
        Ok((hits, degraded)) => {
            let items: Vec<serde_json::Value> = hits
                .into_iter()
                .map(|h: SearchHit| {
                    serde_json::json!({
                        "memory_id": h.memory_id,
                        "kind": h.kind,
                        "claim": h.claim,
                        "status": h.status,
                        "score": h.score,
                        "match_reason": h.match_reason,
                        "evidence_refs": h.evidence_refs,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "request_id": req_id.0,
                "items": items,
                "index_degraded": degraded
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- POST /v1/context/compose（doc/12 §7）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeRequest {
    agent_id: String,
    query: String,
    max_items: Option<usize>,
    max_chars: Option<usize>,
}

async fn compose_context(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<ComposeRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let max_items = body.max_items.unwrap_or(COMPOSE_MAX_ITEMS_DEFAULT);
    if !(1..=5).contains(&max_items) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "max_items 必须 1～5");
    }
    let max_chars = body.max_chars.unwrap_or(COMPOSE_MAX_CHARS_DEFAULT);
    if !(100..=2000).contains(&max_chars) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "max_chars 必须 100～2000");
    }
    let result = state
        .store
        .lock()
        .unwrap()
        .compose_context(&scope, &body.agent_id, &body.query, max_items, max_chars);
    match result {
        Ok(ComposeResult { text, items, truncated, index_degraded }) => {
            let item_objs: Vec<serde_json::Value> = items
                .into_iter()
                .map(|(memory_id, evidence_ids)| serde_json::json!({"memory_id": memory_id, "evidence_ids": evidence_ids}))
                .collect();
            Json(serde_json::json!({
                "request_id": req_id.0,
                "text": text,
                "items": item_objs,
                "truncated": truncated,
                "index_degraded": index_degraded
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- POST /v1/extraction/flush + /v1/jobs（doc/12 §4）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlushRequest {
    host_id: String,
    session_id: String,
    through_event_seq: i64,
}

async fn flush_window(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<FlushRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    if body.host_id.is_empty() || body.session_id.is_empty() || body.through_event_seq < 0 {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "host_id/session_id 非空且 through_event_seq 非负");
    }
    let outcome = {
        let mut guard = state.store.lock().unwrap();
        guard.flush_window(&scope, &body.host_id, &body.session_id, body.through_event_seq)
    };
    match outcome {
        Ok(FlushOutcome::NothingToExtract) => Json(serde_json::json!({
            "request_id": req_id.0, "status": "nothing_to_extract"
        }))
        .into_response(),
        Ok(FlushOutcome::Created { job_id }) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "request_id": req_id.0, "job_id": job_id, "status": "queued" })),
        )
            .into_response(),
        Ok(FlushOutcome::Existing { job_id, status }) => Json(serde_json::json!({
            "request_id": req_id.0, "job_id": job_id, "status": status
        }))
        .into_response(),
        Err(StoreError::StateConflict) => {
            err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StateConflict, "through_event_seq 越界或乱序 flush")
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

fn job_json(req_id: &str, j: &JobRow) -> serde_json::Value {
    serde_json::json!({
        "request_id": req_id,
        "job_id": j.id,
        "status": j.status,
        "attempts": j.attempts,
        "created_at": j.created_at,
        "updated_at": j.updated_at
    })
}

async fn get_job(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    match state.store.lock().unwrap().get_job(&scope, &job_id) {
        Ok(Some(j)) => Json(job_json(&req_id.0, &j)).into_response(),
        Ok(None) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "作业不存在"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn retry_job(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    match state.store.lock().unwrap().retry_dead_job(&scope, &job_id) {
        Ok(true) => Json(serde_json::json!({ "request_id": req_id.0, "job_id": job_id, "status": "queued" })).into_response(),
        Ok(false) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StateConflict, "仅 dead 状态作业可重试"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- POST /v1/memories/{id}/correct 与 /forget（doc/12 §6）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorrectRequestBody {
    expected_version: i64,
    origin: OriginDto,
    user_evidence_id: String,
    old_quote: String,
    replacement_quote: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetRequestBody {
    expected_version: i64,
    origin: OriginDto,
    user_evidence_id: String,
    target_quote: String,
}

async fn correct_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<CorrectRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, m),
    };
    let req = memory_store_sqlite::CorrectRequest {
        expected_version: body.expected_version,
        origin,
        user_evidence_id: body.user_evidence_id,
        old_quote: body.old_quote,
        replacement_quote: body.replacement_quote,
    };
    match state.store.lock().unwrap().correct_memory(&scope, &memory_id, &req) {
        Ok(out) => Json(serde_json::json!({
            "request_id": req_id.0,
            "old_memory_id": out.old_memory_id,
            "new_memory_id": out.new_memory_id,
            "old_version": out.old_version,
            "new_version": out.new_version
        }))
        .into_response(),
        Err(StoreError::MemoryNotFound) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "记忆不存在或非 active"),
        Err(StoreError::VersionConflict) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::VersionConflict, "版本冲突，请重读当前版本"),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => {
            err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StaleUserEvidence, "引用的用户证据不是该会话最新用户事件")
        }
        Err(StoreError::QuoteMismatch) => err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::QuoteMismatch, "最新用户消息缺少替代原文"),
        Err(StoreError::AmbiguousTarget) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::AmbiguousTarget, "目标含糊，请用户更明确表达"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn forget_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<ForgetRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, m),
    };
    let req = memory_store_sqlite::ForgetRequest {
        expected_version: body.expected_version,
        origin,
        user_evidence_id: body.user_evidence_id,
        target_quote: body.target_quote,
    };
    match state.store.lock().unwrap().forget_memory(&scope, &memory_id, &req) {
        Ok(out) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": out.memory_id,
            "status": "forgotten",
            "version": out.version,
            "raw_evidence_retained": true
        }))
        .into_response(),
        Err(StoreError::MemoryNotFound) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "记忆不存在"),
        Err(StoreError::VersionConflict) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::VersionConflict, "版本冲突，请重读当前版本"),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => {
            err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StaleUserEvidence, "引用的用户证据不是该会话最新用户事件")
        }
        Err(StoreError::AmbiguousTarget) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::AmbiguousTarget, "含糊请求：需明确遗忘动词与目标原话"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let degraded = {
        let guard = state.store.lock();
        match guard {
            Ok(store) => store.doctor_summary().map(|_| false).unwrap_or(true),
            Err(_) => true,
        }
    };
    Json(HealthResponse {
        status: "ok",
        protocol_version: PROTOCOL_VERSION,
        db: "ready",
        index: if degraded { "degraded" } else { "ready" },
    })
}

async fn version() -> impl IntoResponse {
    Json(VersionResponse {
        protocol_version: PROTOCOL_VERSION,
        schema_version: SCHEMA_VERSION,
        build: BUILD,
    })
}
