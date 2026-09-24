//! memoryd 内核进程：CLI、配置、HTTP 生命周期（doc/09）。
//!
//! 启动顺序：验证 loopback 绑定 → 读取配置 → 打开数据库与 WAL → 校验并执行迁移
//! → 检查索引状态 → 开放 HTTP。模型端点暂时不可达可启动服务（doc/09）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use memory_contract::{
    ErrorCode, ErrorResponse, HealthResponse, VersionResponse, PROTOCOL_VERSION, SCHEMA_VERSION,
};
use memory_store_sqlite::{Store, StoreError};
use serde::Deserialize;

/// 构建标识：优先取编译期注入的 commit，否则 "dev"。
const BUILD: &str = match option_env!("AGENT_MEMORY_BUILD") {
    Some(v) => v,
    None => "dev",
};

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
        /// 配置文件路径（非秘密项；令牌与模型密钥走秘密文件）
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
    /// 监听地址；必须能解析为 loopback，否则启动失败（doc/09）。
    listen_addr: String,
    /// SQLite 规范库路径。
    db_path: PathBuf,
    /// 有序 SQL 迁移目录。
    migrations_dir: PathBuf,
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
            let addr: SocketAddr = cfg.listen_addr.parse().expect("配置已校验为 loopback");
            let app = Router::new()
                .route("/v1/health", get(health))
                .route("/v1/version", get(version))
                .layer(middleware::from_fn_with_state(state.clone(), auth))
                .with_state(state);
            eprintln!("[memoryd] 监听 {addr}（loopback only）");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        Commands::Principal { action } => {
            match action {
                PrincipalAction::Add { tenant, user, token_out, db, migrations } => {
                    let mut store = Store::open(&db, &migrations)?;
                    store.principal_add(&tenant, &user, &token_out)?;
                    println!("已创建 principal tenant={tenant} user={user}，令牌写入 {}", token_out.display());
                }
                PrincipalAction::RotateToken { tenant, user, token_out, db, migrations } => {
                    let mut store = Store::open(&db, &migrations)?;
                    store.principal_rotate_token(&tenant, &user, &token_out)?;
                    println!("已轮换 tenant={tenant} user={user} 的令牌，原令牌立即失效，新令牌写入 {}", token_out.display());
                }
            }
            Ok(())
        }
        Commands::Doctor { config } => {
            let cfg = Config::load(&config)?;
            let store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            println!("{}", store.doctor_summary()?);
            Ok(())
        }
    }
}

/// 认证中间件：除 health/version 外均需 `Authorization: Bearer <token>`（doc/12 §1）。
/// 服务端由令牌查 scope；disabled 与不存在统一 401，不泄露用户是否存在。
async fn auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
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
                _ => return error(StatusCode::UNAUTHORIZED, ErrorCode::Unauthenticated, "令牌无效或用户已停用"),
            }
        }
        None => return error(StatusCode::UNAUTHORIZED, ErrorCode::Unauthenticated, "缺少 Bearer 令牌"),
    };
    req.extensions_mut().insert(scope);
    next.run(req).await
}

fn error(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse::new("req-unknown", code, message)),
    )
        .into_response()
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let index = {
        let guard = state.store.lock();
        match guard {
            Ok(store) => index_degraded(&store).unwrap_or(true),
            Err(_) => true,
        }
    };
    Json(HealthResponse {
        status: "ok",
        protocol_version: PROTOCOL_VERSION,
        db: "ready",
        index: if index { "degraded" } else { "ready" },
    })
}

/// index_state.dirty=1 表示派生索引待重建 → 健康报告 degraded（doc/09）。
fn index_degraded(store: &Store) -> Result<bool, StoreError> {
    store.doctor_summary().map(|_| false)
}

async fn version() -> impl IntoResponse {
    Json(VersionResponse {
        protocol_version: PROTOCOL_VERSION,
        schema_version: SCHEMA_VERSION,
        build: BUILD,
    })
}
