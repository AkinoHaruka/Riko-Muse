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
use memory_domain::{find_quote_span, MemoryKind, Origin, ScopeKey};
use memory_store_sqlite::{
    CandidateDetail, ComposeResult, FlushOutcome, IngestOutcome, JobDoctorStats, RememberOutcome,
    SearchHit, Store, StoreError,
};
use serde::Deserialize;
use uuid::Uuid;

mod dream_worker;
mod embedding;
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
    /// 问题目录管理（doc6/05 §2、doc6/06 §2；可信 CLI，模型不可改）
    Questions {
        #[command(subcommand)]
        action: QuestionsAction,
    },
    /// 派生知识文档审阅与归档（doc6/05 §5）
    Pages {
        #[command(subcommand)]
        action: PagesAction,
    },
    /// 整理作业管理（doc6/05 §5；管理员显式 enqueue 属自定义触发）
    Consolidate {
        #[command(subcommand)]
        action: ConsolidateAction,
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
    /// D6-8：为全部 active 记忆与 published 页面补建语义索引队列（embedding
    /// 已配置时才有意义；按对象当前版本冻结，幂等）
    ReindexSemantic {
        #[arg(long)]
        config: PathBuf,
    },
    /// D6-9：退休一条记忆（可逆；须最新用户事件 quote 双向定位）
    Retire {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        #[arg(long)] expected_version: i64,
        #[arg(long)] evidence_id: String,
        #[arg(long)] quote: String,
    },
    /// D6-9：恢复一条退休记忆（须最新用户事件 quote 定位）
    Restore {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        #[arg(long)] evidence_id: String,
        #[arg(long)] quote: String,
    },
    /// D6-9：purge 第一阶段 preview（只读业务记忆；写确认元数据）
    PurgePreview {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] memory_id: String,
        /// 幂等键（confirm 须带同键；重复 confirm 只取回无正文结果）
        #[arg(long)] idempotency_key: String,
    },
    /// D6-9：purge 第二阶段 confirm（消费 token 并执行闭包）
    PurgeConfirm {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] token: String,
        #[arg(long)] idempotency_key: String,
    },
    /// D6-9：设置 retention 策略（可信 CLI；默认 0=关闭）
    RetentionPolicy {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long, default_value_t = 0)] raw_days: i64,
        #[arg(long, default_value_t = 0)] expired_days: i64,
        #[arg(long, default_value_t = false)] enabled: bool,
    },
    /// D6-9：执行一轮 retention 清理（无 LLM；复用 purge 闭包）
    RetentionRun {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
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
enum QuestionsAction {
    /// 列出问题（含 archived；默认全部）
    List {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] status: Option<String>,
    },
    /// 登记问题（key 1—64 个 [a-z0-9_]；正文 1—200 标量；key 须不存在）
    Add {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] key: String,
        #[arg(long)] text: String,
    },
    /// 修改问题正文（CAS；旧画像同事务立即 stale）
    Update {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] key: String,
        #[arg(long)] text: String,
        #[arg(long)] expected_version: i64,
    },
    /// 归档问题（用户"删除"首版行为；旧画像同事务 stale）
    Archive {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] key: String,
        #[arg(long)] expected_version: i64,
    },
    /// 重新启用（按新版本重新生成）
    Reactivate {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] key: String,
        #[arg(long)] expected_version: i64,
    },
}

#[derive(Subcommand)]
enum PagesAction {
    /// 列出页面（默认 published；可看 stale/archived）
    List {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 显示单页（读时复核来源；失效页不显示正文）
    Show {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] page_id: String,
    },
    /// 归档页面（CAS；保留 revision，不再搜索/注入）
    Archive {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] page_id: String,
        #[arg(long)] expected_version: i64,
    },
}

#[derive(Subcommand)]
enum ConsolidateAction {
    /// 显式入队一次整理（自定义触发；doc6/05 §2）。mental_model 按已登记问题
    /// 文本词法选输入；topic_page 按 key 词法选输入（≥2 条）。
    Enqueue {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] kind: String,
        #[arg(long)] key: String,
        /// 覆盖检索词；缺省 mental_model 用问题正文、topic_page 用 key
        #[arg(long)] query: Option<String>,
    },
    /// 列出整理作业（诊断）
    Status {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 重试一个 dead/终态作业（显式完整 job ID）
    Retry {
        #[arg(long)] config: PathBuf,
        #[arg(long)] tenant: String,
        #[arg(long)] user: String,
        #[arg(long)] job_id: String,
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

    // ---- D6-8 语义支路（doc6/04 §2：全部显式配置；未配置即 disabled）----
    /// OpenAI 兼容 embeddings endpoint 完整 URL（不猜路径）。
    embedding_endpoint: Option<String>,
    embedding_model: Option<String>,
    embedding_key_file: Option<PathBuf>,
    /// 预期向量维度（配置给出；响应维度不符即失败）。
    embedding_dimensions: Option<usize>,
    /// embeddings 请求超时秒数（默认 30；实时 query embedding 另受 800ms 总预算约束）。
    embedding_timeout_secs: Option<u64>,
    /// 专用 rerank endpoint（Jina/Cohere 兼容形状）；不配 reranker 时保留 RRF 顺序。
    rerank_endpoint: Option<String>,
    rerank_model: Option<String>,
    rerank_key_file: Option<PathBuf>,
    /// episode Retrieved 排序的 recency 模式：linear|exponential|none（默认 linear）。
    recency_mode: Option<String>,
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
    /// D6-8：embedding/reranker 客户端（未配置为 None，语义支路降级）。
    embedding: Option<Arc<embedding::EmbeddingClient>>,
    rerank: Option<Arc<embedding::RerankClient>>,
    /// recency 模式（doc6/04 §3.1：linear|exponential|none，默认 linear）。
    recency_mode: &'static str,
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
            // D6-8：embedding/reranker 客户端（全部显式配置；未配置即 disabled）。
            let embedding_client = match (&cfg.embedding_endpoint, &cfg.embedding_model, &cfg.embedding_key_file, cfg.embedding_dimensions) {
                (Some(endpoint), Some(name), Some(key_file), Some(dims)) => {
                    let api_key = std::fs::read_to_string(key_file)
                        .map_err(|e| format!("读取 embedding 密钥文件失败 {}: {e}", key_file.display()))?
                        .trim()
                        .to_string();
                    if api_key.is_empty() && !cfg.model_allow_empty_key.unwrap_or(false) {
                        return Err("embedding 密钥文件为空；如确需无密钥端点请显式设置 model_allow_empty_key=true".into());
                    }
                    let client = embedding::EmbeddingClient::new(embedding::EmbeddingConfig {
                        endpoint: endpoint.clone(),
                        model: name.clone(),
                        api_key,
                        timeout: std::time::Duration::from_secs(cfg.embedding_timeout_secs.unwrap_or(30)),
                        dimensions: dims,
                    })
                    .map_err(|e| format!("embedding 客户端初始化失败: {e}"))?;
                    eprintln!("[memoryd] 语义支路启用：embedding model={name} dims={dims}");
                    Some(Arc::new(client))
                }
                _ => {
                    eprintln!("[memoryd] 语义支路未配置：semantic_status=disabled，只做词法");
                    None
                }
            };
            let rerank_client = match (&cfg.rerank_endpoint, &cfg.rerank_model, &cfg.rerank_key_file) {
                (Some(endpoint), Some(name), Some(key_file)) => {
                    let api_key = std::fs::read_to_string(key_file)
                        .map_err(|e| format!("读取 rerank 密钥文件失败 {}: {e}", key_file.display()))?
                        .trim()
                        .to_string();
                    if api_key.is_empty() && !cfg.model_allow_empty_key.unwrap_or(false) {
                        return Err("rerank 密钥文件为空；如确需无密钥端点请显式设置 model_allow_empty_key=true".into());
                    }
                    let client = embedding::RerankClient::new(embedding::RerankConfig {
                        endpoint: endpoint.clone(),
                        model: name.clone(),
                        api_key,
                        timeout: std::time::Duration::from_secs(cfg.embedding_timeout_secs.unwrap_or(30)),
                    })
                    .map_err(|e| format!("reranker 客户端初始化失败: {e}"))?;
                    eprintln!("[memoryd] 精排启用：reranker model={name}");
                    Some(Arc::new(client))
                }
                _ => None,
            };
            let state = AppState {
                store: Arc::new(Mutex::new(store)),
                embedding: embedding_client.clone(),
                rerank: rerank_client.clone(),
                recency_mode: match cfg.recency_mode.as_deref() {
                    Some("none") => "none",
                    Some("exponential") => "exponential",
                    _ => memory_contract::RECENCY_MODE_DEFAULT,
                },
            };
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
            // Dream 管线的 chat 配置独立 clone（worker::spawn_worker 内部会再构建客户端）。
            let model_cfg_dream = model_cfg.clone();
            worker::spawn_worker(state.clone(), model_cfg);
            // D6-8：Dream 管线 + 语义索引 worker（doc6/10 §8 受控 runner）。
            // chat 模型复用提取端点配置；embedding 由语义支路配置决定。
            dream_worker::spawn_dream_pipeline(
                state.clone(),
                model_cfg_dream,
                embedding_client,
            );
            // D6-7：Auto Dream scheduler（doc6/10 §4.2，默认启用；memoryd 内置受控
            // runner，doc6/10 §8 路径——由持久 trigger/jobs 驱动）。周期 15 分钟
            // tick；每 scope 24 小时一次 + 空闲 15 分钟 + ≥1 条新 user event 才入队。
            // 无模型配置时作业停在 queued，doctor 报 dream_status=missing_model。
            {
                let sched_state = state.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
                    loop {
                        tick.tick().await;
                        let Ok(mut store) = sched_state.store.lock() else { continue };
                        let Ok(now) = memory_store_sqlite::now_rfc3339_pub() else { continue };
                        let Ok(due) = store.dream_auto_due(&now, 24, 15) else { continue };
                        for (tenant, user, _) in due {
                            let scope = ScopeKey { tenant_id: tenant, user_id: user };
                            let key = format!("auto-{}", &now[..10.min(now.len())]);
                            let _ = store.dream_trigger(&scope, "scheduled", &key, None, None, None);
                        }
                    }
                });
            }
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
                .route("/v1/memories/{memory_id}/retire", post(retire_memory))
                .route("/v1/memories/{memory_id}/restore", post(restore_memory))
                .route("/v1/context/compose", post(compose_context))
                .route("/v1/soul", get(get_soul).put(put_soul))
                .route("/v1/soul/revisions", get(list_soul_revisions))
                .route("/v1/resident/pins", post(post_resident_pin))
                .route("/v1/resident/pins/{memory_id}", delete(delete_resident_pin))
                .route("/v1/resident", get(get_resident))
                .route("/v1/resident/suggestions", get(get_resident_suggestions))
                .route("/v1/context/bundle", post(post_context_bundle))
                .route("/v1/pages", get(list_pages))
                .route("/v1/pages/{page_id}", get(get_page))
                .route("/v1/mental-model/questions", get(list_questions))
                .route("/v1/dream/triggers", post(post_dream_trigger))
                .route("/v1/dream/jobs", get(list_dream_jobs))
                .route("/v1/dream/jobs/{job_id}", get(get_dream_job))
                .route("/v1/resident/page-pins", post(post_page_pin))
                .route("/v1/resident/page-pins/{page_id}", delete(delete_page_pin))
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
        Commands::Questions { action } => {
            run_questions_action(action)?;
            Ok(())
        }
        Commands::Pages { action } => {
            run_pages_action(action)?;
            Ok(())
        }
        Commands::Consolidate { action } => {
            run_consolidate_action(action)?;
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
            let (fts, grams, pages) = store.rebuild_index()?;
            println!(
                "rebuild-index 完成：fts_rows={fts} grams_rows_deleted={grams} page_index_rows={pages}"
            );
            Ok(())
        }
        Commands::Backup { config, out } => {
            let cfg = Config::load(&config)?;
            let mut store = Store::open(&cfg.db_path, &cfg.migrations_dir)?;
            store.backup_to(&out)?;
            println!("备份完成：{}", out.display());
            Ok(())
        }
        Commands::ReindexSemantic { config } => {
            let cfg = Config::load(&config)?;
            if cfg.embedding_model.is_none() {
                return Err("embedding 未配置：reindex-semantic 无意义（语义支路 disabled）".into());
            }
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let n = store.semantic_reindex_all(cfg.embedding_model.as_deref().unwrap())?;
            println!("reindex-semantic 完成：入队 {n} 个对象（worker 将按当前版本生成向量）");
            Ok(())
        }
        Commands::Retire { config, tenant, user, memory_id, expected_version, evidence_id, quote } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let origin = Origin { host_id: "cli".into(), agent_id: "admin".into(), session_id: "cli".into() };
            // Rust 核逐字 span（最新用户事件）+ 目标 claim 双向定位（G-13 同法）。
            store.verify_user_quote_span(&scope, &origin, &evidence_id, &quote)
                .map_err(|e| format!("quote 核验失败: {e}"))?;
            let claim_ok = store.get_memory(&scope, &memory_id)
                .map_err(|e| e.to_string())?
                .map(|m| find_quote_span(&m.claim, &quote).is_some())
                .unwrap_or(false);
            if !claim_ok {
                return Err("目标含糊：quote 未定位到该记忆 claim".into());
            }
            let req = memory_store_sqlite::lifecycle::RetireRequest {
                expected_version,
                actor_kind: "user",
                reason_code: Some("user_request".to_string()),
                user_evidence_id: Some(evidence_id),
            };
            let retired = store.retire_memory(&scope, &memory_id, &req).map_err(|e| e.to_string())?;
            println!("retire 完成：memory_id={memory_id} retired={retired}");
            Ok(())
        }
        Commands::Restore { config, tenant, user, memory_id, evidence_id, quote } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let origin = Origin { host_id: "cli".into(), agent_id: "admin".into(), session_id: "cli".into() };
            store.verify_user_quote_span(&scope, &origin, &evidence_id, &quote)
                .map_err(|e| format!("quote 核验失败: {e}"))?;
            let restored = store.restore_memory(&scope, &memory_id, "user").map_err(|e| e.to_string())?;
            println!("restore 完成：memory_id={memory_id} restored={restored}");
            Ok(())
        }
        Commands::PurgePreview { config, tenant, user, memory_id, idempotency_key } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let (token, preview) = store
                .purge_preview(&scope, &memory_id, &idempotency_key)
                .map_err(|e| e.to_string())?;
            // 明文 token 只输出一次（确认后即弃；库中仅存哈希）。
            println!("preview token（一次性，15 分钟内有效）：{token}");
            println!("{}", serde_json::to_string_pretty(&preview).map_err(|e| e.to_string())?);
            Ok(())
        }
        Commands::PurgeConfirm { config, tenant, user, token, idempotency_key } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let out = store.purge_confirm(&scope, &token, &idempotency_key).map_err(|e| e.to_string())?;
            println!("purge confirm 完成：job_id={} deleted={}", out.job_id, out.deleted);
            Ok(())
        }
        Commands::RetentionPolicy { config, tenant, user, raw_days, expired_days, enabled } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let v = store.retention_set_policy(&scope, raw_days, expired_days, enabled).map_err(|e| e.to_string())?;
            println!("retention 策略已设置：version={v} raw_days={raw_days} expired_days={expired_days} enabled={enabled}");
            Ok(())
        }
        Commands::RetentionRun { config, tenant, user } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            match store.retention_run(&scope).map_err(|e| e.to_string())? {
                Some(r) => println!("retention 完成：{}", serde_json::to_string(&r).map_err(|e| e.to_string())?),
                None => println!("retention 无操作（策略未配置/关闭或本批次已执行）"),
            }
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

// ---- D6-5：问题目录 / pages / consolidate 本机 CLI（doc6/05 §2/§5、doc6/06 §2）----

/// 整理入队的输入选择（doc6/05 §2）：词法检索有界 20 条 active L1，
/// fingerprint 由排序后的 (id,version) 序列哈希（输入漂移即新指纹）。
fn select_consolidation_inputs(
    store: &Store,
    scope: &ScopeKey,
    query: &str,
) -> Result<Vec<(String, i64, String)>, String> {
    let (hits, _) = store
        .search_memories(scope, query, 20, false)
        .map_err(|e| e.to_string())?;
    let mut inputs: Vec<(String, i64, String)> = Vec::new();
    for hit in hits {
        let (v, s) = store
            .memory_version_sha(scope, &hit.memory_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("记忆 {} 读取失败", hit.memory_id))?;
        inputs.push((hit.memory_id, v, s));
    }
    Ok(inputs)
}

fn run_questions_action(action: QuestionsAction) -> Result<(), String> {
    match action {
        QuestionsAction::List { config, tenant, user, status } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let rows = store.question_list(&scope, status.as_deref()).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（问题目录为空——doc6/05：首版默认空，Dream 不生成画像）");
                return Ok(());
            }
            println!("{:<28} {:<8} {:<10} {:<22} {}", "key", "version", "status", "updated_at", "text");
            for r in rows {
                println!("{:<28} {:<8} {:<10} {:<22} {}", r.question_key, r.version, r.status, r.updated_at, r.question_text);
            }
            Ok(())
        }
        QuestionsAction::Add { config, tenant, user, key, text } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let v = store.question_add(&scope, &key, &text, "user_cli").map_err(|e| match e {
                StoreError::InvalidQuestionKey => "问题键须为 1—64 个 ASCII [a-z0-9_]".into(),
                StoreError::InvalidQuestionText => "问题正文须 1—200 个 Unicode 标量字符".into(),
                StoreError::StateConflict => "该问题键已存在（add 要求 key 不存在）".into(),
                other => other.to_string(),
            })?;
            println!("已登记问题 {key} version={v}");
            Ok(())
        }
        QuestionsAction::Update { config, tenant, user, key, text, expected_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let v = store.question_update(&scope, &key, &text, expected_version, "user_cli").map_err(|e| match e {
                StoreError::QuestionNotFound => "问题不存在或不属于当前 scope".into(),
                other => other.to_string(),
            })?;
            println!("已更新问题 {key} version={v}（旧画像已立即 stale）");
            Ok(())
        }
        QuestionsAction::Archive { config, tenant, user, key, expected_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let v = store.question_archive(&scope, &key, expected_version, "user_cli").map_err(|e| e.to_string())?;
            println!("已归档问题 {key} version={v}（旧画像已立即 stale）");
            Ok(())
        }
        QuestionsAction::Reactivate { config, tenant, user, key, expected_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let v = store.question_reactivate(&scope, &key, expected_version, "user_cli").map_err(|e| e.to_string())?;
            println!("已重新启用问题 {key} version={v}（按新版本与当前有效 L1 重新生成）");
            Ok(())
        }
    }
}

fn run_pages_action(action: PagesAction) -> Result<(), String> {
    match action {
        PagesAction::List { config, tenant, user, status, limit } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            if !(1..=100).contains(&limit) {
                return Err("limit 必须 1～100".into());
            }
            let statuses: Vec<&str> = match status.as_deref() {
                None => vec![],
                Some(s) => s.split(',').map(str::trim).collect(),
            };
            let rows = store.page_list(&scope, &statuses, limit).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无页面）");
                return Ok(());
            }
            println!("{:<40} {:<14} {:<24} {:<10} {:<8} {:<22} {}", "page_id", "kind", "key", "status", "version", "updated_at", "title");
            for r in rows {
                println!("{:<40} {:<14} {:<24} {:<10} {:<8} {:<22} {}", r.page_id, r.document_kind, r.document_key, r.status, r.version, r.updated_at, r.title);
            }
            Ok(())
        }
        PagesAction::Show { config, tenant, user, page_id } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            match store.get_page(&scope, &page_id, &now).map_err(|e| e.to_string())? {
                None => Err(format!("页面 {page_id} 不存在、非 published 或来源已失效（读时复核）")),
                Some(p) => {
                    println!("page_id: {}", p.page_id);
                    println!("kind/key: {}/{}", p.document_kind, p.document_key);
                    println!("version: {} generator: {}", p.version, p.generator_version);
                    println!("sources: {:?}", p.sources);
                    println!("---");
                    println!("{}", p.body_md);
                    Ok(())
                }
            }
        }
        PagesAction::Archive { config, tenant, user, page_id, expected_version } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let ok = store.page_archive(&scope, &page_id, expected_version).map_err(|e| e.to_string())?;
            if ok {
                println!("已归档 {page_id}（revision 保留；不再搜索/注入）");
            } else {
                return Err("归档未生效：页面不存在、非 published 或版本不符（409 语义）".into());
            }
            Ok(())
        }
    }
}

fn run_consolidate_action(action: ConsolidateAction) -> Result<(), String> {
    match action {
        ConsolidateAction::Enqueue { config, tenant, user, kind, key, query } => {
            if !matches!(kind.as_str(), "mental_model" | "topic_page") {
                return Err("kind 必须是 mental_model/topic_page".into());
            }
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            // mental_model：问题必须已登记且 active（doc6/05 §2 空目录跳过画像）。
            let question_version = if kind == "mental_model" {
                let q = store
                    .question_list(&scope, Some("active"))
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|q| q.question_key == key)
                    .ok_or_else(|| format!("问题 {key} 未登记或非 active——先 mental-model questions add"))?;
                Some(q.version)
            } else {
                None
            };
            let search_query = query.unwrap_or_else(|| {
                if kind == "mental_model" {
                    store
                        .question_list(&scope, Some("active"))
                        .ok()
                        .and_then(|rows| rows.into_iter().find(|q| q.question_key == key))
                        .map(|q| q.question_text)
                        .unwrap_or_else(|| key.clone())
                } else {
                    key.clone()
                }
            });
            let inputs = select_consolidation_inputs(&store, &scope, &search_query)?;
            if kind == "topic_page" && inputs.len() < 2 {
                return Err(format!("主题页至少需要 2 条 active 来源（当前 {} 条）；doc6/05 §2 不满足不生成", inputs.len()));
            }
            if kind == "mental_model" && inputs.is_empty() {
                return Err("没有相关 active L1 来源；不生成画像".into());
            }
            let parts: Vec<String> = inputs.iter().map(|(id, v, _)| format!("{id}:{v}")).collect();
            let part_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            let fingerprint = memory_store_sqlite::soul::receipt_hash(&part_refs);
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let generator = if kind == "mental_model" {
                memory_store_sqlite::pages::GENERATE_MENTAL_MODEL_V1
            } else {
                memory_store_sqlite::pages::GENERATE_CONSOLIDATE_V1
            };
            let job = store
                .consolidation_enqueue(
                    &scope, &kind, &key, question_version, generator, &fingerprint, &inputs, &now,
                )
                .map_err(|e| e.to_string())?;
            println!(
                "已入队整理作业 {}（kind={kind} key={key} 输入 {} 条 status={}）",
                job.id,
                inputs.len(),
                job.status
            );
            println!("注意：模型调用由 D6-7 Dream runner 接通后启动；当前作业停在 queued 属预期");
            Ok(())
        }
        ConsolidateAction::Status { config, tenant, user, status, limit } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            if !(1..=100).contains(&limit) {
                return Err("limit 必须 1～100".into());
            }
            let rows = store.consolidation_list(&scope, status.as_deref(), limit).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无整理作业）");
                return Ok(());
            }
            println!("{:<40} {:<14} {:<24} {:<12} {:<6} {:<22} {}", "job_id", "kind", "key", "status", "gen", "updated_at", "error");
            for r in rows {
                println!(
                    "{:<40} {:<14} {:<24} {:<12} {:<6} {:<22} {}",
                    r.id,
                    r.document_kind,
                    r.document_key,
                    r.status,
                    r.claim_generation,
                    r.updated_at,
                    r.error_code.unwrap_or_else(|| "-".into())
                );
            }
            Ok(())
        }
        ConsolidateAction::Retry { config, tenant, user, job_id } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey { tenant_id: tenant, user_id: user };
            let job = store
                .consolidation_get(&scope, &job_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("作业 {job_id} 不存在或不属于当前 scope"))?;
            if job.status != "dead" && job.status != "stale_input" {
                return Err(format!("仅 dead/stale_input 作业可 retry；当前 {}", job.status));
            }
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let ok = store
                .consolidation_requeue(&scope, &job_id, &now)
                .map_err(|e| e.to_string())?;
            if ok {
                println!("作业 {job_id} 已重新排队（冻结输入不变）");
            } else {
                return Err("重试未生效".into());
            }
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
        Ok(RememberOutcome::Created { memory_id, version }) => {
            enqueue_semantic_index(&state, &scope, "memory", &memory_id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "request_id": req_id.0,
                    "memory_id": memory_id,
                    "version": version,
                    "status": "active"
                })),
            )
                .into_response()
        }
        Ok(RememberOutcome::Dedup { memory_id, version }) => {
            enqueue_semantic_index(&state, &scope, "memory", &memory_id);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "request_id": req_id.0,
                    "memory_id": memory_id,
                    "version": version,
                    "status": "active",
                    "deduplicated": true
                })),
            )
                .into_response()
        }
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

// ---- D6-3：GET /v1/resident、GET /v1/resident/suggestions、POST /v1/context/bundle ----

/// bundle 预算默认值/上限（doc6/01 §4）。
const RESIDENT_MAX_ITEMS_DEFAULT: usize = 24;
const RESIDENT_MAX_ITEMS_MAX: usize = 100;
const RESIDENT_MAX_CHARS_DEFAULT: usize = 3000;
const RESIDENT_MAX_CHARS_MAX: usize = 12000;
const RETRIEVED_MAX_ITEMS_DEFAULT: usize = 8;
const RETRIEVED_MAX_ITEMS_MAX: usize = 100;
const RETRIEVED_MAX_CHARS_DEFAULT: usize = 2400;
const RETRIEVED_MAX_CHARS_MAX: usize = 24000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleRequest {
    agent_id: String,
    /// 空查询（工具续步等 step）仍给 resident，retrieved 为空（doc6/04 §4）。
    #[serde(default)]
    query: String,
    resident_max_items: Option<usize>,
    resident_max_chars: Option<usize>,
    retrieved_max_items: Option<usize>,
    retrieved_max_chars: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidentQuery {
    /// 仅诊断回显；选择本身是 scope 级（doc6/06 §2）。
    agent_id: Option<String>,
    limit: Option<usize>,
}

fn resident_section_json(
    req_id: &str,
    sel: &memory_store_sqlite::resident::ResidentSelection,
) -> serde_json::Value {
    let items: Vec<serde_json::Value> = sel
        .items
        .iter()
        .map(|i| {
            serde_json::json!({
                "kind": "memory",
                "memory_id": i.memory_id,
                "memory_kind": i.kind,
                "claim": i.claim,
                "reason": i.reason,
                "version": i.version,
                "evidence_ids": i.evidence_ids,
            })
        })
        .collect();
    let omitted: Vec<serde_json::Value> = sel
        .omitted
        .iter()
        .map(|(id, r)| serde_json::json!({"memory_id": id, "reason": r}))
        .collect();
    serde_json::json!({
        "text": sel.text,
        "items": items,
        "omitted": omitted,
        "conflict_ids": sel.conflict_ids,
        "stale_pages": sel.stale_pages,
        "truncated": sel.truncated,
    })
}

fn budget_check(
    req_id: &str,
    name: &str,
    value: Option<usize>,
    default: usize,
    max: usize,
) -> Result<usize, Response> {
    let v = value.unwrap_or(default);
    if !(1..=max).contains(&v) {
        return Err(err(
            req_id,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            Box::leak(format!("{name} 必须 1～{max}").into_boxed_str()),
        ));
    }
    Ok(v)
}

/// episode recency 因子（doc6/04 §3.1，Hindsight 可追溯初值；仅 Retrieved 排序
/// 信号）：linear `clamp(1-age/365,0.1,1)`；exponential `0.5^(age/90)` 下限 0.4
/// （保证乘数区间 ⊂ [0.92,1.10]，从而 20% base 差距不被 recency 越级）；none 恒 1。
/// 无可用时间戳不加不减；未来时间按 0 处理（不额外奖励）。
fn recency_factor(mode: &str, occurred_at: Option<&str>, updated_at: &str, now: &str) -> f64 {
    if mode == "none" {
        return 1.0;
    }
    let ts = occurred_at.filter(|s| !s.is_empty()).unwrap_or(updated_at);
    let (Ok(t), Ok(n)) = (
        chrono::DateTime::parse_from_rfc3339(ts),
        chrono::DateTime::parse_from_rfc3339(now),
    ) else {
        return 1.0;
    };
    let age_days = ((n - t).num_minutes().max(0) as f64) / 1440.0;
    let freshness = if mode == "exponential" {
        0.5f64.powf(age_days / memory_contract::RECENCY_HALFLIFE_DAYS).clamp(0.4, 1.0)
    } else {
        (1.0 - age_days / 365.0).clamp(memory_contract::RECENCY_FRESHNESS_MIN, 1.0)
    };
    1.0 + memory_contract::RECENCY_SCALE * (freshness - 0.5)
}

#[cfg(test)]
mod recency_tests {
    use super::recency_factor;

    const NOW: &str = "2026-09-26T00:00:00Z";

    #[test]
    fn recency_bounds_and_modes() {
        // doc6/04 §3.1：乘数区间 [0.92,1.10]；新鲜 episode ≈1.1 上限；无时间戳不加不减。
        let fresh = recency_factor("linear", Some("2026-09-26T00:00:00Z"), NOW, NOW);
        let old = recency_factor("linear", Some("2020-01-01T00:00:00Z"), NOW, NOW);
        assert!((fresh - 1.10).abs() < 1e-9, "新鲜=1.10 上限，实际 {fresh}");
        assert!((old - 0.92).abs() < 1e-9, "极旧=0.92 下限，实际 {old}");
        // 无可用时间戳（occurred_at 缺失且 updated_at 不可解析）→ freshness=0.5 不加不减。
        assert!((recency_factor("linear", None, "bad-ts", NOW) - 1.0).abs() < 1e-9);
        // none 恒 1；未来时间按 0 处理（钳制，不额外奖励）。
        assert_eq!(recency_factor("none", Some("2026-09-26T00:00:00Z"), NOW, NOW), 1.0);
        let future = recency_factor("linear", Some("2030-01-01T00:00:00Z"), NOW, NOW);
        assert!(future <= 1.10, "未来时间不奖励");
    }

    #[test]
    fn recency_cannot_override_20pct_base_gap() {
        // 乘数区间 [0.92,1.10] ⇒ 最大越级比 1.10/0.92 < 1.20：
        // base 相差 ≥20% 的两条 episode，低分项不得仅靠 recency 越级（doc6/04 §3.1）。
        for age_days in [0i64, 30, 90, 200, 400] {
            let ts = (chrono::DateTime::parse_from_rfc3339(NOW).unwrap()
                - chrono::Duration::days(age_days))
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
            let hi = recency_factor("linear", Some(&ts), NOW, NOW);
            let lo = recency_factor("exponential", Some(&ts), NOW, NOW);
            let (f_min, f_max) = (hi.min(lo), hi.max(lo));
            assert!(
                f_max / f_min < memory_contract::RECENCY_OVERRIDE_RATIO,
                "age={age_days} 天时因子比 {} 超界",
                f_max / f_min
            );
        }
    }
}

/// D6-8：对象写入后入队异步向量索引（doc6/02 §4）。embedding 未配置时不入队
/// （索引队列为空 = 语义支路 disabled）。
fn enqueue_semantic_index(state: &AppState, scope: &ScopeKey, object_kind: &str, object_id: &str) {
    if let Some(emb) = &state.embedding {
        let mut g = state.store.lock().unwrap();
        if let Err(e) = g.semantic_enqueue(scope, object_kind, object_id, emb.model_id()) {
            eprintln!("[memoryd] 语义索引入队失败 {object_kind}/{object_id}: {e}");
        }
    }
}

async fn get_resident(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<ResidentQuery>,
) -> Response {
    let limit = match budget_check(&req_id.0, "limit", query.limit, RESIDENT_MAX_ITEMS_DEFAULT, RESIDENT_MAX_ITEMS_MAX) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    let guard = state.store.lock().unwrap();
    match guard.select_resident(&scope, &now, limit, RESIDENT_MAX_CHARS_MAX) {
        Ok(sel) => {
            let needs_review: Vec<&String> = sel.needs_review.iter().collect();
            Json(serde_json::json!({
                "request_id": req_id.0,
                "agent_id": query.agent_id,
                "resident": resident_section_json(&req_id.0, &sel),
                "needs_review": needs_review,
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn get_resident_suggestions(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<ResidentQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～100");
    }
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    let guard = state.store.lock().unwrap();
    match guard.resident_suggestions(&scope, &now, limit) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "kind": "memory",
                        "memory_id": r.memory_id,
                        "memory_kind": r.kind,
                        "claim": r.claim,
                        "updated_at": r.updated_at,
                        "evidence_ids": r.evidence_ids,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "request_id": req_id.0,
                "items": items,
            }))
            .into_response()
        }
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn post_context_bundle(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<BundleRequest>, axum::extract::rejection::JsonRejection>,
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
    let r_items = match budget_check(&req_id.0, "resident_max_items", req.resident_max_items, RESIDENT_MAX_ITEMS_DEFAULT, RESIDENT_MAX_ITEMS_MAX) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let r_chars = match budget_check(&req_id.0, "resident_max_chars", req.resident_max_chars, RESIDENT_MAX_CHARS_DEFAULT, RESIDENT_MAX_CHARS_MAX) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let q_items = match budget_check(&req_id.0, "retrieved_max_items", req.retrieved_max_items, RETRIEVED_MAX_ITEMS_DEFAULT, RETRIEVED_MAX_ITEMS_MAX) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let q_chars = match budget_check(&req_id.0, "retrieved_max_chars", req.retrieved_max_chars, RETRIEVED_MAX_CHARS_DEFAULT, RETRIEVED_MAX_CHARS_MAX) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    let query_trim = req.query.trim();
    // resident 段（查询无关，每轮重算；doc6/03 §3）。
    let selection = {
        let guard = state.store.lock().unwrap();
        guard.select_resident(&scope, &now, r_items, r_chars)
    };
    let selection = match selection {
        Ok(s) => s,
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    // retrieved 段（doc6/04 §2/§3，D6-8）：各通道独立取候选与排名 → 单次全局 RRF
    // → 可选 cross-encoder 精排 → 对象复核（active/published/有效期）→ 排除
    // resident → 页/原子覆盖去重 → episode recency → 预算装配。分数仅用于排序。
    let mut lexical_status = "empty_query";
    let mut semantic_status = if state.embedding.is_some() { "ok" } else { "disabled" };
    let mut rerank_status = if state.rerank.is_some() { "ok" } else { "disabled" };
    let mut page_index_status = "not_applicable";
    let mut retrieved_text = String::new();
    let mut retrieved_items: Vec<serde_json::Value> = Vec::new();
    let mut retrieved_omitted: Vec<serde_json::Value> = Vec::new();
    let mut retrieved_truncated = false;
    let mut source_versions = serde_json::Map::new();
    for i in &selection.items {
        source_versions.insert(i.memory_id.clone(), serde_json::json!(i.version));
    }
    if !query_trim.is_empty() {
        let resident_ids: std::collections::HashSet<String> = selection
            .items
            .iter()
            .map(|i| i.memory_id.clone())
            .chain(selection.conflict_ids.iter().cloned())
            .collect();
        let lane_k = q_items.max(memory_contract::ADJUDICATE_RECALL_TOP_K);
        // ---- 通道 1：词法记忆（FTS+grams 内部融合，单一词法通道排名）。----
        let search_res = {
            let guard = state.store.lock().unwrap();
            guard.search_memories(&scope, query_trim, lane_k, false)
        };
        let (mem_lex, index_degraded) = match search_res {
            Ok(v) => v,
            Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
        };
        lexical_status = if index_degraded { "index_degraded" } else { "ok" };
        // ---- 通道 2：词法页面（published；读时复核在装配段统一做）。----
        let page_lex: Vec<String> = {
            let guard = state.store.lock().unwrap();
            guard.page_fts_search(&scope, query_trim, lane_k).unwrap_or_default()
        };
        page_index_status = "ok";
        // ---- 通道 3/4：向量（embedding 已配置才启用；query embedding 800ms 预算，
        // doc6/04 §2）。ready 总数达上限 → limit_exceeded 只做词法。----
        let mut mem_vec: Vec<String> = Vec::new();
        let mut page_vec: Vec<String> = Vec::new();
        if let Some(emb) = &state.embedding {
            let qtexts = vec![query_trim.to_string()];
            let fut = emb.embed(&qtexts);
            match tokio::time::timeout(
                std::time::Duration::from_millis(memory_contract::QUERY_EMBEDDING_TIMEOUT_MS),
                fut,
            )
            .await
            {
                Ok(Ok(vs)) if !vs.is_empty() => {
                    let qv = &vs[0];
                    let guard = state.store.lock().unwrap();
                    let scan_m = guard.semantic_scan(&scope, "memory", emb.model_id(), qv, lane_k);
                    let scan_p = guard.semantic_scan(&scope, "page", emb.model_id(), qv, lane_k);
                    match (scan_m, scan_p) {
                        (Ok((mh, mc)), Ok((ph, pc))) => {
                            if mc >= memory_contract::SEMANTIC_SCAN_LIMIT
                                || pc >= memory_contract::SEMANTIC_SCAN_LIMIT
                            {
                                semantic_status = "limit_exceeded"; // 只做词法（doc6/04 §2）
                            } else {
                                mem_vec = mh.into_iter().map(|(id, _)| id).collect();
                                page_vec = ph.into_iter().map(|(id, _)| id).collect();
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            semantic_status = "unavailable";
                            eprintln!("[bundle] 向量扫描失败: {e}");
                        }
                    }
                }
                Ok(Ok(_)) => { semantic_status = "unavailable"; } // 空向量
                Ok(Err(e)) => {
                    semantic_status = "unavailable";
                    eprintln!("[bundle] query embedding 失败: {e}");
                }
                Err(_) => {
                    semantic_status = "unavailable"; // 800ms 超时，降级词法
                    eprintln!("[bundle] query embedding 超时（{}ms）", memory_contract::QUERY_EMBEDDING_TIMEOUT_MS);
                }
            }
        }
        // ---- 对象复核与信息补全（active/published/有效期；不信任索引缓存）。----
        let mut mem_ids: Vec<String> = mem_lex.iter().map(|h| h.memory_id.clone()).collect();
        mem_ids.extend(mem_vec.iter().cloned());
        mem_ids.dedup();
        let mut page_ids: Vec<String> = page_lex.clone();
        page_ids.extend(page_vec.iter().cloned());
        page_ids.dedup();
        let mut mem_info: std::collections::HashMap<String, memory_store_sqlite::memories::MemoryRow> =
            std::collections::HashMap::new();
        {
            let guard = state.store.lock().unwrap();
            for mid in &mem_ids {
                if let Ok(Some(m)) = guard.get_memory(&scope, mid) {
                    mem_info.insert(mid.clone(), m);
                }
            }        }
        let mut page_info: std::collections::HashMap<String, memory_store_sqlite::pages::PageRow> =
            std::collections::HashMap::new();
        {
            let guard = state.store.lock().unwrap();
            for pid in &page_ids {
                if let Ok(Some(p)) = guard.get_page(&scope, pid, &now) {
                    page_info.insert(pid.clone(), p);
                }
            }
        }
        // ---- 单次全局 RRF（doc6/04 §2：禁止向量支路内部再 RRF）。----
        let mut channels: Vec<(&str, Vec<String>)> = Vec::new();
        channels.push(("lexical", mem_lex.iter().filter(|h| mem_info.contains_key(&h.memory_id)).map(|h| h.memory_id.clone()).collect()));
        channels.push(("page", page_lex.iter().filter(|p| page_info.contains_key(*p)).cloned().collect()));
        if semantic_status == "ok" {
            channels.push(("semantic", mem_vec.iter().filter(|m| mem_info.contains_key(*m)).cloned().collect()));
            channels.push(("semantic", page_vec.iter().filter(|p| page_info.contains_key(*p)).cloned().collect()));
        }
        let mut rrf: std::collections::HashMap<String, (Vec<u32>, Vec<&str>)> =
            std::collections::HashMap::new();
        for (name, ch) in &channels {
            for (i, key) in ch.iter().enumerate() {
                let entry = rrf.entry(key.clone()).or_default();
                entry.0.push(i as u32 + 1);
                if !entry.1.contains(name) {
                    entry.1.push(name);
                }
            }
        }
        let mut scored: Vec<(String, f64)> = rrf
            .iter()
            .map(|(k, (ranks, _))| (k.clone(), memory_recall::rrf_score(ranks)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // ---- cross-encoder 后置精排（可选；有界 top-N；失败保留 RRF 顺序并报告）。
        let n_rerank = scored.len().min((q_items * 3).max(10));
        if n_rerank > 1 {
            if let Some(rk) = &state.rerank {
                let docs: Vec<String> = scored[..n_rerank]
                    .iter()
                    .map(|(k, _)| match k.split_once(':') {
                        Some(("m", id)) => mem_info.get(id).map(|m| m.claim.clone()).unwrap_or_default(),
                        Some(("p", id)) => page_info.get(id).map(|p| p.title.clone()).unwrap_or_default(),
                        _ => String::new(),
                    })
                    .collect();
                let fut = rk.rerank(query_trim, &docs, n_rerank);
                match tokio::time::timeout(std::time::Duration::from_secs(3), fut).await {
                    Ok(Ok(order)) if !order.is_empty() => {
                        rerank_status = "success";
                        let mut reranked: Vec<(String, f64)> = Vec::with_capacity(n_rerank);
                        for (idx, s) in order {
                            if let Some((k, _)) = scored.get(idx) {
                                reranked.push((k.clone(), s));
                            }
                        }
                        let tail: Vec<(String, f64)> = scored[n_rerank..].to_vec();
                        scored = reranked;
                        scored.extend(tail);
                    }
                    Ok(Err(e)) => {
                        rerank_status = "unavailable";
                        eprintln!("[bundle] 精排失败（保留 RRF 顺序）: {e}");
                    }
                    Ok(Ok(_)) => rerank_status = "unavailable",
                    Err(_) => {
                        rerank_status = "unavailable";
                        eprintln!("[bundle] 精排超时（保留 RRF 顺序）");
                    }
                }
            }
        }
        // ---- episode recency（doc6/04 §3.1：仅 episode 的 Retrieved 排序信号；
        // fact/preference/instruction/Soul/pinned Resident 因子恒 1）。----
        for (k, base) in scored.iter_mut() {
            if let Some(("m", id)) = k.split_once(':') {
                if let Some(m) = mem_info.get(id) {
                    if m.kind == "episode" {
                        let f = recency_factor(
                            state.recency_mode,
                            m.occurred_at.as_deref(),
                            &m.updated_at,
                            &now,
                        );
                        *base *= f;
                    }
                }
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // ---- 装配：排除 resident → 页/原子覆盖去重 → 预算整条组装。----
        let mut covered: std::collections::HashSet<String> = resident_ids;
        let mut used_chars_retrieved = 0usize;
        let channel_of = |k: &str, rrf: &std::collections::HashMap<String, (Vec<u32>, Vec<&str>)>| -> String {
            rrf.get(k).map(|(_, names)| names.join("+")).unwrap_or_else(|| "fused".into())
        };
        for (key, _score) in &scored {
            match key.split_once(':') {
                Some(("m", mid)) => {
                    if covered.contains(mid) {
                        continue; // resident 已含/冲突 ID 或已被页面覆盖。
                    }
                    let Some(m) = mem_info.get(mid) else { continue };
                    let refs: Vec<String> = m.evidence_refs.iter().map(|(e, _, _)| e.clone()).collect();
                    let entry = format!("- [memory: {}] {}", m.memory_id, m.claim);
                    let entry_chars = entry.chars().count();
                    if retrieved_items.len() >= q_items
                        || (used_chars_retrieved > 0 && used_chars_retrieved + entry_chars + 1 > q_chars)
                    {
                        retrieved_omitted.push(serde_json::json!({
                            "memory_id": mid,
                            "reason": if retrieved_items.len() >= q_items { "ITEM_LIMIT" } else { "CHAR_LIMIT" },
                        }));
                        retrieved_truncated = true;
                        continue;
                    }
                    used_chars_retrieved += entry_chars + if retrieved_items.is_empty() { 0 } else { 1 };
                    if retrieved_text.is_empty() {
                        retrieved_text = entry;
                    } else {
                        retrieved_text.push('\n');
                        retrieved_text.push_str(&entry);
                    }
                    retrieved_items.push(serde_json::json!({
                        "kind": "memory",
                        "memory_id": m.memory_id,
                        "memory_kind": m.kind,
                        "claim": m.claim,
                        "reason": channel_of(key, &rrf),
                        "evidence_ids": refs,
                    }));
                    source_versions.insert(m.memory_id.clone(), serde_json::json!(m.version));
                    covered.insert(mid.to_string());
                }
                Some(("p", pid)) => {
                    if covered.contains(pid) {
                        continue; // resident 已含该页。
                    }
                    let Some(p) = page_info.get(pid) else { continue }; // 来源失效 → 不可见
                    let source_ids: Vec<String> = p.sources.iter().map(|(id, _)| id.clone()).collect();
                    let uncovered = source_ids.iter().filter(|s| !covered.contains(*s)).count();
                    let min_needed = if p.document_kind == "mental_model" { 1 } else { 2 };
                    if uncovered < min_needed {
                        continue; // doc6/04 §3：未覆盖来源不足 → 跳过文档。
                    }
                    let entry = format!("- [page: {}] {}", p.page_id, p.title);
                    let entry_chars = entry.chars().count();
                    if retrieved_items.len() >= q_items
                        || (used_chars_retrieved > 0 && used_chars_retrieved + entry_chars + 1 > q_chars)
                    {
                        retrieved_omitted.push(serde_json::json!({"page_id": pid, "reason": "ITEM_LIMIT"}));
                        retrieved_truncated = true;
                        continue;
                    }
                    used_chars_retrieved += entry_chars + if retrieved_items.is_empty() { 0 } else { 1 };
                    if retrieved_text.is_empty() {
                        retrieved_text = entry;
                    } else {
                        retrieved_text.push('\n');
                        retrieved_text.push_str(&entry);
                    }
                    // 文档入选覆盖其全部来源（后续相同 L1 不重复出现）。
                    for s in &source_ids {
                        covered.insert(s.clone());
                    }
                    retrieved_items.push(serde_json::json!({
                        "kind": "page",
                        "page_id": p.page_id,
                        "document_kind": p.document_kind,
                        "title": p.title,
                        "derived": true,
                        "reason": channel_of(key, &rrf),
                        "version": p.version,
                        "source_memory_ids": p.sources,
                    }));
                    source_versions.insert(p.page_id.clone(), serde_json::json!(p.version));
                }
                _ => {}
            }
        }
    }
    Json(serde_json::json!({
        "request_id": req_id.0,
        "agent_id": req.agent_id,
        "resident": resident_section_json(&req_id.0, &selection),
        "retrieved": {
            "text": retrieved_text,
            "items": retrieved_items,
            "omitted": retrieved_omitted,
            "truncated": retrieved_truncated,
        },
        "lexical_status": lexical_status,
        "semantic_status": semantic_status,
        "rerank_status": rerank_status,
        "page_index_status": page_index_status,
        "source_versions": source_versions,
    }))
    .into_response()
}

// ---- D6-5：GET /v1/pages、GET /v1/pages/{id}、GET /v1/mental-model/questions（只读审阅）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PagesListQuery {
    status: Option<String>,
    limit: Option<usize>,
}

fn page_json(p: &memory_store_sqlite::pages::PageRow) -> serde_json::Value {
    serde_json::json!({
        "kind": "page",
        "page_id": p.page_id,
        "document_kind": p.document_kind,
        "document_key": p.document_key,
        "question_version": p.question_version,
        "title": p.title,
        "body_md": p.body_md,
        "status": p.status,
        "version": p.version,
        "generator_version": p.generator_version,
        "sources": p.sources.iter().map(|(id, v)| serde_json::json!({"memory_id": id, "version": v})).collect::<Vec<_>>(),
        "updated_at": p.updated_at,
    })
}

async fn list_pages(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<PagesListQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～100");
    }
    let statuses: Vec<&str> = match query.status.as_deref() {
        None => vec![],
        Some(s) => s.split(',').map(str::trim).collect(),
    };
    for s in &statuses {
        if !matches!(*s, "published" | "stale" | "archived") {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "status 只能是 published/stale/archived");
        }
    }
    let guard = state.store.lock().unwrap();
    match guard.page_list(&scope, &statuses, limit) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "pages": rows.iter().map(page_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn get_page(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(page_id): AxumPath<String>,
) -> Response {
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => return err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    };
    let guard = state.store.lock().unwrap();
    match guard.get_page(&scope, &page_id, &now) {
        Ok(Some(p)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "page": page_json(&p),
        }))
        .into_response(),
        // 跨 scope/失效/不存在一律 404（不泄露存在性；doc6/06 §2）。
        Ok(None) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "页面不存在或已失效"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionsListQuery {
    status: Option<String>,
}

async fn list_questions(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<QuestionsListQuery>,
) -> Response {
    if let Some(s) = query.status.as_deref() {
        if !matches!(s, "active" | "archived") {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "status 只能是 active/archived");
        }
    }
    let guard = state.store.lock().unwrap();
    match guard.question_list(&scope, query.status.as_deref()) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "questions": rows.iter().map(|r| serde_json::json!({
                "question_key": r.question_key,
                "question_text": r.question_text,
                "version": r.version,
                "status": r.status,
                "updated_at": r.updated_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- D6-6：page-pin 路由（doc6/06 §2；仅 published 且来源有效可 pin）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PagePinRequest {
    page_id: String,
    idempotency_key: String,
}

async fn post_page_pin(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<PagePinRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    if req.page_id.is_empty() {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "page_id 不能为空");
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.page_pin(&scope, &req.page_id)
    };
    match result {
        Ok(pin_version) => Json(serde_json::json!({
            "request_id": req_id.0,
            "page_id": req.page_id,
            "status": "pinned",
            "pin_version": pin_version,
        }))
        .into_response(),
        Err(StoreError::PageNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "页面不存在、非 published 或来源已失效",
        ),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn delete_page_pin(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(page_id): AxumPath<String>,
) -> Response {
    let mut guard = state.store.lock().unwrap();
    match guard.page_unpin(&scope, &page_id) {
        Ok(version) => Json(serde_json::json!({
            "request_id": req_id.0,
            "page_id": page_id,
            "status": "unpinned",
            "pin_version": version,
        }))
        .into_response(),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

// ---- D6-7：POST /v1/dream/triggers、GET /v1/dream/jobs(/{id})（doc6/06 §2、doc6/10 §4）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamTriggerRequest {
    trigger_kind: String,
    trigger_key: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

async fn post_dream_trigger(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DreamTriggerRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidJson, "请求不是合法 JSON")
        }
        Err(_) => return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "字段缺失、类型错误或含未知字段"),
    };
    if req.trigger_key.is_empty() || req.trigger_key.chars().count() > 128 {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "trigger_key 须 1—128 字符");
    }
    // 只入队不等待完成（doc6/10 §4：触发器只负责入队）。
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.dream_trigger(
            &scope,
            &req.trigger_kind,
            &req.trigger_key,
            req.agent_id.as_deref(),
            req.host_id.as_deref(),
            req.session_id.as_deref(),
        )
    };
    match result {
        Ok(Some(job)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "request_id": req_id.0,
                "status": "trigger_queued",
                "job": {
                    "job_id": job.id,
                    "trigger_kind": job.trigger_kind,
                    "status": job.status,
                    "input_fingerprint": job.input_fingerprint,
                    "extract_version": job.extract_version,
                },
            })),
        )
            .into_response(),
        // 无待处理 L0：不建空作业（doc6/10 §4.2）。
        Ok(None) => Json(serde_json::json!({
            "request_id": req_id.0,
            "status": "nothing_to_process",
        }))
        .into_response(),
        Err(StoreError::StateConflict) => err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "trigger_kind 非法"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamJobsQuery {
    status: Option<String>,
    limit: Option<usize>,
}

fn dream_job_json(job: &memory_store_sqlite::dream_jobs::DreamJobRow) -> serde_json::Value {
    serde_json::json!({
        "job_id": job.id,
        "trigger_kind": job.trigger_kind,
        "trigger_key": job.trigger_key,
        "extract_version": job.extract_version,
        "status": job.status,
        "attempts": job.attempts,
        "run_after": job.run_after,
        "lease_until": job.lease_until,
        "input_fingerprint": job.input_fingerprint,
        "error_code": job.error_code,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })
}

async fn list_dream_jobs(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<DreamJobsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "limit 必须 1～100");
    }
    if let Some(s) = query.status.as_deref() {
        if !matches!(s, "queued" | "running" | "succeeded" | "retryable_failed" | "provider_wait" | "dead" | "stale_input") {
            return err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "status 非法");
        }
    }
    let guard = state.store.lock().unwrap();
    match guard.dream_list(&scope, query.status.as_deref(), limit) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "jobs": rows.iter().map(dream_job_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn get_dream_job(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
) -> Response {
    let guard = state.store.lock().unwrap();
    match guard.dream_get(&scope, &job_id) {
        Ok(Some(job)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "job": dream_job_json(&job),
        }))
        .into_response(),
        // 跨用户/不存在均 404（doc6/06 §2）。
        Ok(None) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "作业不存在或不属于当前 scope"),
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

/// D6-9：restore 请求（同 retire 的证据核验；无版本 CAS）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreRequestBody {
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
        Ok(out) => {
            // D6-8：新记忆入队向量索引（旧记忆向量已在 correct 事务内置 stale）。
            enqueue_semantic_index(&state, &scope, "memory", &out.new_memory_id);
            Json(serde_json::json!({
                "request_id": req_id.0,
                "old_memory_id": out.old_memory_id,
                "new_memory_id": out.new_memory_id,
                "old_version": out.old_version,
                "new_version": out.new_version
            }))
            .into_response()
        }
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

// ---- D6-9：retire / restore（doc6/02 §7、doc6/12）----

async fn retire_memory(
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
    {
        // Rust 核：最新真实用户事件中的逐字指令 span + 目标 claim 含该 quote
        //（G-13 同法：带 ID 的泛称"退休"不得误伤未被指认的记忆）。
        let guard = state.store.lock().unwrap();
        if guard.verify_user_quote_span(&scope, &origin, &body.user_evidence_id, &body.target_quote).is_err() {
            return err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StaleUserEvidence, "引用的用户证据不是该会话最新用户事件或 quote 非逐字");
        }
        let claim_ok = guard
            .get_memory(&scope, &memory_id)
            .ok()
            .flatten()
            .map(|m| find_quote_span(&m.claim, &body.target_quote).is_some())
            .unwrap_or(false);
        if !claim_ok {
            return err(&req_id.0, StatusCode::CONFLICT, ErrorCode::AmbiguousTarget, "目标含糊：quote 未定位到该记忆 claim");
        }
    }
    let req = memory_store_sqlite::lifecycle::RetireRequest {
        expected_version: body.expected_version,
        actor_kind: "user",
        reason_code: Some("user_request".to_string()),
        user_evidence_id: Some(body.user_evidence_id),
    };
    match state.store.lock().unwrap().retire_memory(&scope, &memory_id, &req) {
        Ok(true) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": memory_id,
            "status": "retired"
        }))
        .into_response(),
        Ok(false) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": memory_id,
            "status": "retired",
            "already_retired": true
        }))
        .into_response(),
        Err(StoreError::MemoryNotFound) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "记忆不存在或非 active"),
        Err(StoreError::VersionConflict) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::VersionConflict, "版本冲突，请重读当前版本"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn restore_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<RestoreRequestBody>, axum::extract::rejection::JsonRejection>,
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
    {
        // Rust 核：最新真实用户事件中的逐字指令 span（restore 指令与 claim 无关）。
        let guard = state.store.lock().unwrap();
        if guard.verify_user_quote_span(&scope, &origin, &body.user_evidence_id, &body.target_quote).is_err() {
            return err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StaleUserEvidence, "引用的用户证据不是该会话最新用户事件或 quote 非逐字");
        }
    }
    match state.store.lock().unwrap().restore_memory(&scope, &memory_id, "user") {
        Ok(true) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": memory_id,
            "status": "active"
        }))
        .into_response(),
        Ok(false) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": memory_id,
            "status": "active",
            "was_not_retired": true
        }))
        .into_response(),
        Err(StoreError::MemoryNotFound) => err(&req_id.0, StatusCode::NOT_FOUND, ErrorCode::NotFound, "记忆不存在、非 active、已到期或无有效证据"),
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
        // D6 能力握手（doc6/06 §1）：随卡交付递增；适配器据此启用新注入路径。
        capabilities: vec![
            memory_contract::CAPABILITY_SOUL_V1,
            memory_contract::CAPABILITY_CONTEXT_BUNDLE_V1,
        ],
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
