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
use axum::routing::{delete, get, post};
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
    CandidateDetail, ComposeResult, FlushOutcome, IngestOutcome, JobDoctorStats, RememberOutcome,
    SearchHit, Store, StoreError,
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
    /// 作业管理（本地管理员；doc4/03 §4）
    Job {
        #[command(subcommand)]
        action: JobAction,
    },
    /// 候选只读诊断（doc4/04 §2；本阶段不提供 promote）
    Candidates {
        #[command(subcommand)]
        action: CandidatesAction,
    },
    /// Soul（用户编辑人格）管理（doc6/03 §1、doc6/06 §2；本机可信 CLI）
    Soul {
        #[command(subcommand)]
        action: SoulAction,
    },
    /// Resident 固定记忆管理（doc6/03 §5、doc6/06 §2；本机可信 CLI）
    Resident {
        #[command(subcommand)]
        action: ResidentAction,
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
enum JobAction {
    /// 显式跳过一个 dead/WINDOW_TOO_LARGE 作业的自动提取（不删 L0 原文；doc4/03 §4）
    Skip {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        job_id: String,
        /// 运维原因 1—256 字符；不得填用户正文或密钥（写入审计）
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum CandidatesAction {
    /// 列出候选（默认 held；只显示 ID/kind/reason/created_at/来源事件/quote 长度）
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "held")]
        status: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 翻页边界：当前 scope 中真实存在的候选 ID，列出其 (created_at,id) 之前的更早项
        #[arg(long)]
        before: Option<String>,
    },
    /// 显示单个候选的 quote 与证据定位（本机交互终端使用，不写日志）
    Show {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand)]
enum SoulAction {
    /// 显示当前 Soul（版本/时间/正文）
    Show {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] agent: String,
    },
    /// 导入 soul.md（CAS；expected-version 必填，初次创建用 0；doc6/03 §1）
    Import {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] agent: String,
        #[arg(long)] file: PathBuf,
        #[arg(long)] expected_version: i64,
    },
    /// 导出当前正文到文件（临时文件 + 原子替换；版本写 <out>.version sidecar）
    Export {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] agent: String,
        #[arg(long)] out: PathBuf,
    },
    /// 列出历史版本元数据（正文用 show/HTTP 单独读取）
    History {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] agent: String,
    },
}

#[derive(Subcommand)]
enum ResidentAction {
    /// 固定一条记忆（显式 position 即重排；doc6/02 §2）
    Pin {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        #[arg(long)] position: Option<i64>,
        #[arg(long)] expected_pin_version: Option<i64>,
    },
    /// 解除固定（行保留，enabled=0）
    Unpin {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        #[arg(long)] expected_pin_version: Option<i64>,
    },
    /// 重排到目标下标（越界钳制到末尾）
    Move {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        #[arg(long)] position: i64,
        #[arg(long)] expected_pin_version: Option<i64>,
    },
    /// 列出 enabled pin 与当前可见性/原因（doc6/03 §5）
    List {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
    },
    /// 导出 pinned 清单 Markdown（D6-2 视图：仅 pinned 区；预算/召回归 D6-3 bundle）
    Export {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] out: PathBuf,
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
    /// 单次生成上限（默认 1024）。推理型 provider 无上限输出会拖垮提取调用。
    model_max_tokens: Option<u32>,
    /// 单次模型调用超时秒数（默认 MODEL_CALL_TIMEOUT_SECS=30）。
    model_timeout_secs: Option<u64>,
    /// 额外请求体字段（provider 专有开关，如 enable_thinking=false），顶层合并。
    model_extra_json: Option<toml::Value>,
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
                            cfg.model_timeout_secs
                                .unwrap_or(memory_contract::MODEL_CALL_TIMEOUT_SECS),
                        ),
                        max_tokens: cfg.model_max_tokens.unwrap_or(1024),
                        extra_body: match &cfg.model_extra_json {
                            Some(v) => Some(serde_json::to_value(v).map_err(|e| {
                                format!("model_extra_json 无法转换为 JSON 请求字段: {e}")
                            })?),
                            None => None,
                        },
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
                .route("/v1/jobs", get(list_jobs))
                .route("/v1/jobs/{job_id}", get(get_job))
                .route("/v1/jobs/{job_id}/retry", post(retry_job))
                .route("/v1/memories/remember", post(remember_memory))
                .route("/v1/memories/search", post(search_memories))
                .route("/v1/memories/{memory_id}", get(get_memory))
                .route("/v1/memories/{memory_id}/correct", post(correct_memory))
                .route("/v1/memories/{memory_id}/forget", post(forget_memory))
                .route("/v1/context/compose", post(compose_context))
                .route("/v1/soul", get(get_soul).put(put_soul))
                .route("/v1/soul/revisions", get(list_soul_revisions))
                .route("/v1/resident/pins", post(post_resident_pin))
                .route("/v1/resident/pins/{memory_id}", delete(delete_resident_pin))
                .layer(middleware::from_fn_with_state(state.clone(), request_pipeline))
                .with_state(state);
            eprintln!("[memoryd] 监听 {addr}（loopback only）");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        Commands::Job { action } => match action {
            JobAction::Skip { config, tenant, user, job_id, reason } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    let reason_chars = reason.chars().count();
                    if reason_chars == 0 || reason_chars > 256 {
                        return Err("skip reason 必须 1—256 个字符，且不得填用户正文或密钥".into());
                    }
                    let mut store = Store::open(&cfg.db_path, &cfg.migrations_dir).map_err(|e| e.to_string())?;
                    let scope = ScopeKey { tenant_id: tenant, user_id: user };
                    // 先按 scope 精确查询，缺失/跨 scope 与状态不符给出可区分错误。
                    if store.get_job(&scope, &job_id).map_err(|e| e.to_string())?.is_none() {
                        return Err(format!("作业 {job_id} 不存在或不属于当前 scope，未变更任何行"));
                    }
                    match store.skip_dead_job(&scope, &job_id, &reason) {
                        Ok(true) => {
                            println!("已跳过作业 {job_id}（WINDOW_TOO_LARGE）；L0 原文保留，后窗将从该 through 之后推进");
                            Ok(())
                        }
                        Ok(false) => {
                            println!("作业 {job_id} 已存在跳过记录（幂等），未重复写入");
                            Ok(())
                        }
                        Err(StoreError::StateConflict) => Err(
                            "仅 dead 且 error_code=WINDOW_TOO_LARGE 的作业可 skip；其他 dead 请先修复原因后用完整 job ID retry".into(),
                        ),
                        Err(e) => Err(e.to_string()),
                    }
                };
                run()?;
                Ok(())
            }
        },
        Commands::Candidates { action } => match action {
            CandidatesAction::List { config, tenant, user, status, limit, before } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    if !(1..=100).contains(&limit) {
                        return Err("limit 必须 1～100".into());
                    }
                    if !matches!(status.as_str(), "held" | "candidate" | "rejected") {
                        return Err("status 必须是 held/candidate/rejected".into());
                    }
                    let store = Store::open(&cfg.db_path, &cfg.migrations_dir).map_err(|e| e.to_string())?;
                    let scope = ScopeKey { tenant_id: tenant, user_id: user };
                    let rows = store
                        .list_candidates(&scope, &status, limit, before.as_deref())
                        .map_err(|e| match e {
                            StoreError::JobNotFound => "before 候选不存在或不属于当前 scope".to_string(),
                            other => other.to_string(),
                        })?;
                    println!("{:<40} {:<12} {:<24} {:<30} {:<24} {}", "ID", "kind", "reason", "created_at", "evidence_id", "quote_len");
                    for c in rows {
                        println!(
                            "{:<40} {:<12} {:<24} {:<30} {:<24} {}",
                            c.id,
                            c.kind,
                            c.reason_code.clone().unwrap_or_else(|| "-".into()),
                            c.created_at,
                            c.primary_evidence_id,
                            c.quote_len
                        );
                    }
                    Ok(())
                };
                run()?;
                Ok(())
            }
            CandidatesAction::Show { config, tenant, user, id } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    let store = Store::open(&cfg.db_path, &cfg.migrations_dir).map_err(|e| e.to_string())?;
                    let scope = ScopeKey { tenant_id: tenant, user_id: user };
                    let detail: Option<CandidateDetail> = store
                        .get_candidate(&scope, &id)
                        .map_err(|e| e.to_string())?;
                    let Some(c) = detail else {
                        return Err(format!("候选 {id} 不存在或不属于当前 scope"));
                    };
                    println!("id: {}", c.id);
                    println!("kind: {} status: {} reason: {}", c.kind, c.status, c.reason_code.clone().unwrap_or_else(|| "-".into()));
                    println!("created_at: {}", c.created_at);
                    println!("primary_evidence_id: {}", c.primary_evidence_id);
                    println!(
                        "evidence_span: {}..{}",
                        c.evidence_start_byte.map(|v| v.to_string()).unwrap_or("-".into()),
                        c.evidence_end_byte.map(|v| v.to_string()).unwrap_or("-".into())
                    );
                    println!("quote_sha256: {}", c.quote_sha256);
                    println!("quote:");
                    println!("{}", c.quote);
                    Ok(())
                };
                run()?;
                Ok(())
            }
        },
        Commands::Soul { action } => {
            run_soul_action(action)?;
            Ok(())
        }
        Commands::Resident { action } => {
            run_resident_action(action)?;
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
            // 作业侧聚合计数（doc4/04 §3）：全部 scope 聚合只显示总数，不打印用户列表；
            // db=ready 不代表作业健康，卡住的作业在此可见。
            let stats: JobDoctorStats = store.job_doctor_stats()?;
            println!("{}", stats.summary());
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

// ---- D6-2：soul/resident 本机 CLI（doc6/03 §1/§5）----

/// CLI 打开数据库的统一入口：先警告 Store::open 会自动应用未执行迁移（doc6/03 §5）。
fn open_store_warned(db_path: &Path, migrations_dir: &Path) -> Result<Store, String> {
    eprintln!(
        "[memoryd] 注意：打开 {} 会自动应用未执行的迁移（当前二进制 schema {}）；旧版本二进制会被 checksum 拒绝",
        db_path.display(),
        SCHEMA_VERSION
    );
    Store::open(db_path, migrations_dir).map_err(|e| e.to_string())
}

/// 导出文件原子替换（doc6/02 §8.6）：同目录临时文件 + write_all + sync_all + rename。
fn atomic_write_file(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;
        f.write_all(content.as_bytes()).map_err(|e| format!("写入临时文件失败: {e}"))?;
        f.sync_all().map_err(|e| format!("刷盘失败: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("原子替换失败: {e}"))
}

/// Markdown 单行化：折叠换行，避免用户正文破坏导出文件结构（doc6/03 §2）。
fn md_single_line(text: &str) -> String {
    text.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

fn run_soul_action(action: SoulAction) -> Result<(), String> {
    match action {
        SoulAction::Show { config, tenant, user, agent } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            match store.get_soul(&scope, &agent).map_err(|e| e.to_string())? {
                None => Err(format!("agent {agent} 在当前 scope 无 Soul（version 0）")),
                Some(p) => {
                    println!("agent_id: {}", p.agent_id);
                    println!("version: {}", p.version);
                    println!("updated_at: {}", p.updated_at);
                    println!("body_sha256: {}", p.body_sha256);
                    println!("---");
                    println!("{}", p.body_md);
                    Ok(())
                }
            }
        }
        SoulAction::Import { config, tenant, user, agent, file, expected_version } => {
            let cfg = Config::load(&config)?;
            if expected_version < 0 {
                return Err("expected-version 不能为负；初次创建用 0".into());
            }
            let body = std::fs::read_to_string(&file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let report = store
                .upsert_soul(&scope, &agent, &body, expected_version, "user_cli", None, None)
                .map_err(|e| match e {
                    StoreError::VersionConflict => {
                        "版本冲突（VERSION_CONFLICT）：expected-version 与当前不符；用 soul show 查看当前版本".into()
                    }
                    StoreError::SoulBodyTooLong => "正文超过 2000 个 Unicode 标量字符，拒绝导入".into(),
                    StoreError::InvalidAgentId => "agent ID 须为 1—256 字符".into(),
                    other => other.to_string(),
                })?;
            let version = match report.outcome {
                memory_store_sqlite::soul::SoulUpsertOutcome::Created { version } => {
                    println!("已创建 Soul version={version}");
                    version
                }
                memory_store_sqlite::soul::SoulUpsertOutcome::Updated { version } => {
                    println!("已更新 Soul version={version}");
                    version
                }
                memory_store_sqlite::soul::SoulUpsertOutcome::Unchanged { version } => {
                    println!("内容与 version={version} 相同，幂等未变更");
                    version
                }
            };
            if !report.audit_recorded {
                eprintln!("[memoryd] 告警：memory_audit 写入失败（业务修改已提交）；请检查数据库");
            }
            let profile = store.get_soul(&scope, &agent).map_err(|e| e.to_string())?;
            println!("body_sha256: {}", profile.map(|p| p.body_sha256).unwrap_or_default());
            let _ = version;
            Ok(())
        }
        SoulAction::Export { config, tenant, user, agent, out } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let profile = store
                .get_soul(&scope, &agent)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("agent {agent} 在当前 scope 无 Soul，无可导出正文"))?;
            atomic_write_file(&out, &profile.body_md)?;
            let sidecar = out.with_extension("version");
            atomic_write_file(&sidecar, &format!("{}\n", profile.version))?;
            println!(
                "已导出 Soul version={} 到 {}（版本 sidecar：{}）",
                profile.version,
                out.display(),
                sidecar.display()
            );
            Ok(())
        }
        SoulAction::History { config, tenant, user, agent } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let revisions = store.list_soul_revisions(&scope, &agent).map_err(|e| e.to_string())?;
            if revisions.is_empty() {
                println!("（无历史版本）");
                return Ok(());
            }
            println!("{:<10} {:<20} {:<12} {}", "version", "body_sha256", "actor", "changed_at");
            for r in revisions {
                println!("{:<10} {:<20} {:<12} {}", r.version, &r.body_sha256[..20.min(r.body_sha256.len())], r.actor_kind, r.changed_at);
            }
            Ok(())
        }
    }
}

fn run_resident_action(action: ResidentAction) -> Result<(), String> {
    match action {
        ResidentAction::Pin { config, tenant, user, memory_id, position, expected_pin_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            match store.resident_pin(&scope, &memory_id, position, expected_pin_version, None) {
                Ok(memory_store_sqlite::resident::PinOutcome::Pinned { version, position }) => {
                    println!("已固定 {memory_id}：pin_version={version} position={position}");
                    Ok(())
                }
                Ok(memory_store_sqlite::resident::PinOutcome::Unchanged { version, position }) => {
                    println!("{memory_id} 已固定（幂等）：pin_version={version} position={position}");
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => {
                    Err("记忆不存在或不属于当前 scope（404 语义），未变更".into())
                }
                Err(StoreError::VersionConflict) => {
                    Err("pin 版本冲突（409）：expected-pin-version 与当前不符；用 resident list 查看".into())
                }
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::Unpin { config, tenant, user, memory_id, expected_pin_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            match store.resident_unpin(&scope, &memory_id, expected_pin_version, None) {
                Ok(o) => {
                    if o.already_disabled {
                        println!("{memory_id} 已处于解除状态（幂等）：pin_version={}", o.version);
                    } else {
                        println!("已解除固定 {memory_id}：pin_version={}", o.version);
                    }
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => Err("记忆不存在或不属于当前 scope（404 语义），未变更".into()),
                Err(StoreError::VersionConflict) => Err("pin 版本冲突（409）：expected-pin-version 与当前不符".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::Move { config, tenant, user, memory_id, position, expected_pin_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            match store.resident_move(&scope, &memory_id, position, expected_pin_version) {
                Ok(version) => {
                    println!("已重排 {memory_id} 到 position={position}：pin_version={version}");
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => Err("pin 行/记忆不存在或不属于当前 scope（404 语义）".into()),
                Err(StoreError::VersionConflict) => Err("pin 版本冲突（409）".into()),
                Err(StoreError::StateConflict) => Err("disabled 行不可重排；先重新 pin".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::List { config, tenant, user } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let rows = store.resident_pins_with_status(&scope, &now).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无 enabled pin）");
                return Ok(());
            }
            println!("{:<40} {:<6} {:<8} {:<14} {:<20} {}", "memory_id", "pos", "pin_ver", "status", "visible", "reason");
            for r in rows {
                println!(
                    "{:<40} {:<6} {:<8} {:<14} {:<20} {}",
                    r.memory_id,
                    r.position,
                    r.version,
                    r.memory_status,
                    r.visible,
                    r.reason
                );
            }
            Ok(())
        }
        ResidentAction::Export { config, tenant, user, out } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let rows = store.resident_pins_with_status(&scope, &now).map_err(|e| e.to_string())?;
            let mut md = String::from("# 长期记忆（resident pinned 视图）\n\n");
            md.push_str(&format!(
                "生成时间: {now}；本文件为只读快照，编辑入口是 pin/unpin 与 remember/correct/forget（doc6/03 §2）。\n\n"
            ));
            if rows.is_empty() {
                md.push_str("（无固定项）\n");
            }
            let rows_len = rows.len();
            for r in rows {
                let claim = store
                    .get_memory_claim(&scope, &r.memory_id)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| "（正文不可读）".into());
                let visibility = if r.visible { "可见".into() } else { format!("不可见（{}）", r.reason) };
                md.push_str(&format!(
                    "- [memory: {}] {}\n  状态: {}；选择: pinned；位置: {}；{}\n",
                    r.memory_id,
                    md_single_line(&claim),
                    r.memory_status,
                    r.position,
                    visibility
                ));
            }
            atomic_write_file(&out, &md)?;
            println!("已导出 {} 条 pinned 项到 {}", rows_len, out.display());
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
        // 直写内容护栏已按用户产品决定（2026-09-25 深夜）全部解除；直写路径不再有
        // 内容类别类 409。quote/证据类错误（QUOTE_MISMATCH、STALE_USER_EVIDENCE）
        // 沿既有映射。
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
        Ok(FlushOutcome::NothingToExtract { job_id }) => Json(serde_json::json!({
            "request_id": req_id.0, "status": "nothing_to_extract", "job_id": job_id
        }))
        .into_response(),
        Ok(FlushOutcome::Created { job_id, status }) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "request_id": req_id.0, "job_id": job_id, "status": status })),
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

fn job_json(req_id: &str, j: &memory_store_sqlite::JobDetail) -> serde_json::Value {
    let i = &j.item;
    serde_json::json!({
        "request_id": req_id,
        "job_id": i.id,
        "host_id": i.host_id,
        "session_id": i.session_id,
        "through_event_seq": i.through_event_seq,
        "status": i.status,
        "attempts": i.attempts,
        "run_after": i.run_after,
        "lease_until": i.lease_until,
        "error_code": i.error_code,
        "skipped": i.skipped,
        "window_key": j.window_key,
        "prompt_version": j.prompt_version,
        "admission_version": j.admission_version,
        "model_name": j.model_name,
        "input_tokens": j.input_tokens,
        "output_tokens": j.output_tokens,
        "created_at": i.created_at,
        "updated_at": i.updated_at
    })
}

// ---- D6-2：GET/PUT /v1/soul、GET /v1/soul/revisions、resident pins（doc6/06 §2）----

fn validate_agent_id(agent_id: &str) -> Result<(), &'static str> {
    if agent_id.is_empty() {
        return Err("agent_id 不能为空");
    }
    if agent_id.chars().count() > memory_store_sqlite::soul::SOUL_AGENT_ID_MAX_CHARS {
        return Err("agent_id 超过 256 字符");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoulQuery {
    agent_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoulImportRequest {
    agent_id: String,
    body_md: String,
    expected_version: i64,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoulRevisionsQuery {
    agent_id: String,
    limit: Option<usize>,
    /// 翻页：返回 version 大于该值的条目（升序）；cursor 即上一页最后的 version。
    after_version: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinRequest {
    memory_id: String,
    position: Option<i64>,
    expected_pin_version: Option<i64>,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnpinBody {
    expected_pin_version: Option<i64>,
}

/// 重放响应：以存储的最小结果 JSON 为基底，补回请求关联字段；重放一律 200。
fn replay_response(
    req_id: &str,
    key_field: (&str, &str),
    stored: &memory_store_sqlite::soul::MutationReceipt,
) -> Response {
    let mut body: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&stored.response_json).unwrap_or_default();
    body.insert("request_id".into(), serde_json::json!(req_id));
    body.insert(key_field.0.into(), serde_json::json!(key_field.1));
    Json(serde_json::Value::Object(body)).into_response()
}

async fn get_soul(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<SoulQuery>,
) -> Response {
    if let Err(msg) = validate_agent_id(&query.agent_id) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg);
    }
    let guard = state.store.lock().unwrap();
    match guard.get_soul(&scope, &query.agent_id) {
        Ok(Some(p)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "agent_id": p.agent_id,
            "version": p.version,
            "body_md": p.body_md,
            "sha256": p.body_sha256,
        }))
        .into_response(),
        // 不存在：version 0 / 空正文（doc6/06 §2），不报错。
        Ok(None) => Json(serde_json::json!({
            "request_id": req_id.0,
            "agent_id": query.agent_id,
            "version": 0,
            "body_md": "",
            "sha256": "",
        }))
        .into_response(),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn put_soul(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<SoulImportRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    if let Err(msg) = validate_agent_id(&req.agent_id) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg);
    }
    if req.expected_version < 0 {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "expected_version 不能为负");
    }
    // 规范化请求哈希（doc6/06 §2）：固定字段顺序；同键异请求 409 IDEMPOTENCY_CONFLICT。
    let hash = memory_store_sqlite::soul::receipt_hash(&[
        &req.agent_id,
        &req.body_md,
        &req.expected_version.to_string(),
    ]);
    {
        let guard = state.store.lock().unwrap();
        match guard.fetch_mutation_receipt(&scope, "soul_import", &req.idempotency_key) {
            Ok(Some(receipt)) => {
                if receipt.request_sha256 != hash {
                    return err(
                        &req_id.0,
                        StatusCode::CONFLICT,
                        ErrorCode::IdempotencyConflict,
                        "同一幂等键曾以不同请求体使用",
                    );
                }
                return replay_response(&req_id.0, ("agent_id", &req.agent_id), &receipt);
            }
            Ok(None) => {}
            Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
        }
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.upsert_soul(
            &scope,
            &req.agent_id,
            &req.body_md,
            req.expected_version,
            "user_api",
            Some(&req_id.0),
            Some((&req.idempotency_key, &hash)),
        )
    };
    match result {
        Ok(report) => {
            if !report.audit_recorded {
                // doc6/14 §4：审计失败非阻塞且显式告警（业务修改已提交）。
                eprintln!("[memoryd] 告警：memory_audit 写入失败（soul_import request={}）；业务修改已提交", req_id.0);
            }
            let (status_text, code) = match &report.outcome {
                memory_store_sqlite::soul::SoulUpsertOutcome::Created { .. } => ("created", StatusCode::CREATED),
                memory_store_sqlite::soul::SoulUpsertOutcome::Updated { .. } => ("updated", StatusCode::OK),
                memory_store_sqlite::soul::SoulUpsertOutcome::Unchanged { .. } => ("unchanged", StatusCode::OK),
            };
            let version = match &report.outcome {
                memory_store_sqlite::soul::SoulUpsertOutcome::Created { version }
                | memory_store_sqlite::soul::SoulUpsertOutcome::Updated { version }
                | memory_store_sqlite::soul::SoulUpsertOutcome::Unchanged { version } => *version,
            };
            let sha = {
                let guard = state.store.lock().unwrap();
                guard
                    .get_soul(&scope, &req.agent_id)
                    .ok()
                    .flatten()
                    .map(|p| p.body_sha256)
                    .unwrap_or_default()
            };
            (
                code,
                Json(serde_json::json!({
                    "request_id": req_id.0,
                    "agent_id": req.agent_id,
                    "version": version,
                    "status": status_text,
                    "sha256": sha,
                    "audit_recorded": report.audit_recorded,
                })),
            )
                .into_response()
        }
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "expected_version 与当前 Soul 版本不符",
        ),
        Err(StoreError::SoulBodyTooLong) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "body_md 超过 2000 个 Unicode 标量字符，拒绝导入",
        ),
        Err(StoreError::InvalidAgentId) => {
            err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "agent_id 须为 1—256 字符")
        }
        Err(StoreError::InvalidIdempotencyKey) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "idempotency_key 须为 1—128 个 ASCII [A-Za-z0-9._-]",
        ),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn list_soul_revisions(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<SoulRevisionsQuery>,
) -> Response {
    if let Err(msg) = validate_agent_id(&query.agent_id) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg);
    }
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～200");
    }
    let guard = state.store.lock().unwrap();
    match guard.list_soul_revisions(&scope, &query.agent_id) {
        Ok(all) => {
            let page: Vec<_> = all
                .into_iter()
                .filter(|r| query.after_version.map(|after| r.version > after).unwrap_or(true))
                .take(limit)
                .collect();
            let next_cursor =
                if page.len() == limit { page.last().map(|r| r.version.to_string()) } else { None };
            let items: Vec<serde_json::Value> = page
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "version": r.version,
                        "body_sha256": r.body_sha256,
                        "actor_kind": r.actor_kind,
                        "changed_at": r.changed_at,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "request_id": req_id.0,
                "agent_id": query.agent_id,
                "revisions": items,
                "next_cursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn post_resident_pin(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<PinRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    if req.memory_id.is_empty() {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "memory_id 不能为空");
    }
    // 规范化哈希：None 用空串占位，字段集固定（doc6/06 §2）。
    let hash = memory_store_sqlite::soul::receipt_hash(&[
        &req.memory_id,
        &req.position.map(|p| p.to_string()).unwrap_or_default(),
        &req.expected_pin_version.map(|v| v.to_string()).unwrap_or_default(),
    ]);
    {
        let guard = state.store.lock().unwrap();
        match guard.fetch_mutation_receipt(&scope, "resident_pin", &req.idempotency_key) {
            Ok(Some(receipt)) => {
                if receipt.request_sha256 != hash {
                    return err(
                        &req_id.0,
                        StatusCode::CONFLICT,
                        ErrorCode::IdempotencyConflict,
                        "同一幂等键曾以不同请求体使用",
                    );
                }
                return replay_response(&req_id.0, ("memory_id", &req.memory_id), &receipt);
            }
            Ok(None) => {}
            Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
        }
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.resident_pin(
            &scope,
            &req.memory_id,
            req.position,
            req.expected_pin_version,
            Some((&req.idempotency_key, &hash)),
        )
    };
    match result {
        Ok(outcome) => {
            let (code, status_text, pin_version, position) = match &outcome {
                memory_store_sqlite::resident::PinOutcome::Pinned { version, position } => {
                    // 新建（v1）201；重激活/重排 200。
                    let code = if *version == 1 { StatusCode::CREATED } else { StatusCode::OK };
                    (code, "pinned", *version, *position)
                }
                memory_store_sqlite::resident::PinOutcome::Unchanged { version, position } => {
                    (StatusCode::OK, "unchanged", *version, *position)
                }
            };
            (
                code,
                Json(serde_json::json!({
                    "request_id": req_id.0,
                    "memory_id": req.memory_id,
                    "status": status_text,
                    "pin_version": pin_version,
                    "position": position,
                })),
            )
                .into_response()
        }
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或不属于当前 scope",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "expected_pin_version 与当前不符",
        ),
        Err(StoreError::StateConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "position 非法",
        ),
        Err(StoreError::InvalidIdempotencyKey) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "idempotency_key 须为 1—128 个 ASCII [A-Za-z0-9._-]",
        ),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn delete_resident_pin(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(memory_id): AxumPath<String>,
    headers: axum::http::HeaderMap,
    bytes: axum::body::Bytes,
) -> Response {
    // Idempotency-Key 请求头必填（doc6/06 §2）；规范化请求哈希含路径 ID 与可选正文。
    let idem_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(idem_key) = idem_key else {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "缺少 Idempotency-Key 请求头",
        );
    };
    let expected_pin_version: Option<i64> = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<UnpinBody>(&bytes) {
            Ok(b) => b.expected_pin_version,
            Err(_) => {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "请求正文不是合法的 {expected_pin_version}",
                )
            }
        }
    };
    let hash = memory_store_sqlite::soul::receipt_hash(&[
        &memory_id,
        &expected_pin_version.map(|v| v.to_string()).unwrap_or_default(),
    ]);
    {
        let guard = state.store.lock().unwrap();
        match guard.fetch_mutation_receipt(&scope, "resident_unpin", idem_key) {
            Ok(Some(receipt)) => {
                if receipt.request_sha256 != hash {
                    return err(
                        &req_id.0,
                        StatusCode::CONFLICT,
                        ErrorCode::IdempotencyConflict,
                        "同一幂等键曾以不同请求体使用",
                    );
                }
                return replay_response(&req_id.0, ("memory_id", &memory_id), &receipt);
            }
            Ok(None) => {}
            Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
        }
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.resident_unpin(&scope, &memory_id, expected_pin_version, Some((idem_key, &hash)))
    };
    match result {
        Ok(outcome) => {
            let (status_text, pin_version) = if outcome.already_disabled {
                ("already_disabled", outcome.version)
            } else {
                ("unpinned", outcome.version)
            };
            Json(serde_json::json!({
                "request_id": req_id.0,
                "memory_id": memory_id,
                "status": status_text,
                "pin_version": pin_version,
            }))
            .into_response()
        }
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或不属于当前 scope",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "expected_pin_version 与当前不符",
        ),
        Err(StoreError::InvalidIdempotencyKey) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "Idempotency-Key 须为 1—128 个 ASCII [A-Za-z0-9._-]",
        ),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

/// GET /v1/jobs（doc4/04 §1）：scope 内分页作业列表，默认 dead，不返回正文。
async fn list_jobs(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<JobsQuery>,
) -> Response {
    let status = query.status.as_deref().unwrap_or("dead");
    if !matches!(
        status,
        "queued" | "running" | "retryable_failed" | "succeeded" | "dead" | "all"
    ) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "status 必须是 queued/running/retryable_failed/succeeded/dead/all",
        );
    }
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～100");
    }
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => match decode_job_cursor(raw) {
            Ok(c) => Some(c),
            Err(msg) => {
                return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, msg)
            }
        },
    };
    let result = {
        let guard = state.store.lock().unwrap();
        guard.list_jobs(&scope, status, limit, cursor.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))
    };
    match result {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "job_id": i.id,
                        "host_id": i.host_id,
                        "session_id": i.session_id,
                        "through_event_seq": i.through_event_seq,
                        "status": i.status,
                        "attempts": i.attempts,
                        "run_after": i.run_after,
                        "lease_until": i.lease_until,
                        "error_code": i.error_code,
                        "skipped": i.skipped,
                        "created_at": i.created_at,
                        "updated_at": i.updated_at,
                    })
                })
                .collect();
            // next_cursor：取满一页时以最后一行的排序键继续翻页；不足一页则到底。
            let next_cursor = if rows.len() == limit {
                let last = rows.last().unwrap();
                Some(encode_job_cursor(&last.created_at, &last.id))
            } else {
                None
            };
            Json(serde_json::json!({
                "request_id": req_id.0,
                "jobs": items,
                "next_cursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobsQuery {
    status: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// cursor = base64url(JSON {created_at,id})；解码 ≤512 字节，校验 RFC3339 与非空 ID。
/// 它只是翻页位置，不是授权凭据：scope 始终来自当前 token。
fn encode_job_cursor(created_at: &str, id: &str) -> String {
    use base64::Engine as _;
    let payload = serde_json::json!({ "created_at": created_at, "id": id }).to_string();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes())
}

fn decode_job_cursor(raw: &str) -> Result<(String, String), &'static str> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.as_bytes())
        .map_err(|_| "cursor 不是合法 base64url")?;
    if bytes.len() > 512 {
        return Err("cursor 解码超长（>512 字节）");
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "cursor 不是合法 JSON")?;
    let created_at = v
        .get("created_at")
        .and_then(|x| x.as_str())
        .ok_or("cursor 缺 created_at")?;
    if chrono::DateTime::parse_from_rfc3339(created_at).is_err() {
        return Err("cursor.created_at 不是 RFC3339 时间");
    }
    let id = v.get("id").and_then(|x| x.as_str()).ok_or("cursor 缺 id")?;
    if id.is_empty() {
        return Err("cursor.id 不能为空");
    }
    Ok((created_at.to_string(), id.to_string()))
}

async fn get_job(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    match state.store.lock().unwrap().get_job_detail(&scope, &job_id) {
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
    let mut store = state.store.lock().unwrap();
    // 先按 scope 精确查询：缺失/跨 scope 一律 404，不靠截断 ID 猜匹配。
    let detail = match store.get_job_detail(&scope, &job_id) {
        Ok(Some(d)) => d,
        Ok(None) => return err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "作业不存在"),
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    if detail.item.status != "dead" {
        return err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StateConflict, "仅 dead 状态作业可重试");
    }
    if detail.item.error_code.as_deref() == Some("WINDOW_TOO_LARGE") {
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "WINDOW_TOO_LARGE 不允许原样 retry；请用 memoryd job skip 显式跳过，或先修复输入",
        );
    }
    match store.retry_dead_job(&scope, &job_id) {
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

#[cfg(test)]
mod cursor_tests {
    //! doc4/04 §1：作业列表 cursor 的编解码约束——base64url(JSON {created_at,id})，
    //! 解码 ≤512 字节，RFC3339 时间与非空 ID；坏 cursor 全部拒绝。

    use super::{decode_job_cursor, encode_job_cursor};
    use base64::Engine as _;

    #[test]
    fn cursor_roundtrip() {
        let enc = encode_job_cursor("2026-09-25T08:00:00.123456Z", "job-1");
        let (created_at, id) = decode_job_cursor(&enc).unwrap();
        assert_eq!(created_at, "2026-09-25T08:00:00.123456Z");
        assert_eq!(id, "job-1");
    }

    #[test]
    fn cursor_rejects_bad_input() {
        assert!(decode_job_cursor("not-base64!!").is_err(), "非法 base64url 拒绝");
        // 合法 base64url 但内容不是 JSON。
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"plain text");
        assert!(decode_job_cursor(&b64).is_err());
        // JSON 但缺字段。
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"created_at":"2026-09-25T08:00:00Z"}"#);
        assert!(decode_job_cursor(&b64).is_err(), "缺 id 拒绝");
        // 非法时间。
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"created_at":"yesterday","id":"j"}"#);
        assert!(decode_job_cursor(&b64).is_err(), "非 RFC3339 拒绝");
        // 空 ID。
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"created_at":"2026-09-25T08:00:00Z","id":""}"#);
        assert!(decode_job_cursor(&b64).is_err(), "空 id 拒绝");
        // 超长解码（>512 字节）。
        let big = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(vec![b'x'; 600]);
        assert!(decode_job_cursor(&big).is_err(), "解码超长拒绝");
    }
}
