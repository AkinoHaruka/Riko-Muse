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
    /// V2-D1 蒸馏视图（doc7/06 §5）：重建 compact_memory 与四个分面
    Derived {
        #[command(subcommand)]
        action: DerivedAction,
    },
    /// V2-R1 关系图谱（doc7/07 §5）：重建实体/别名/分节条目
    Relationships {
        #[command(subcommand)]
        action: RelationshipsAction,
    },
    /// V2-D1 只读 Markdown 投影（doc7/06 §5）：渲染 compact/分面/清单到授权目录
    Export {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        /// 目标目录；按 <tenant>/<user>/<domain> 分层写入，不落到全用户混合目录
        #[arg(long)]
        out: PathBuf,
        /// 缺省 user_main；side 域须显式指定
        #[arg(long, default_value = "user_main")]
        domain: String,
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
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        evidence_id: String,
        #[arg(long)]
        quote: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// D6-9：恢复一条退休记忆（须最新用户事件 quote 定位）
    Restore {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        #[arg(long)]
        expected_version: i64,
        #[arg(long)]
        evidence_id: String,
        #[arg(long)]
        quote: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// D6-9：purge 第一阶段 preview（只读业务记忆；写确认元数据）
    PurgePreview {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        /// 幂等键（confirm 须带同键；重复 confirm 只取回无正文结果）
        #[arg(long)]
        idempotency_key: String,
    },
    /// D6-9：purge 第二阶段 confirm（消费 token 并执行闭包）
    PurgeConfirm {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        token: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// D6-9：设置 retention 策略（可信 CLI；默认 0=关闭）
    RetentionPolicy {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = 0)]
        raw_days: i64,
        #[arg(long, default_value_t = 0)]
        expired_days: i64,
        #[arg(long, default_value_t = false)]
        enabled: bool,
    },
    /// D6-9：执行一轮 retention 清理（无 LLM；复用 purge 闭包）
    RetentionRun {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
    },
    /// doc7（Riko-Muse）M1：执行一轮 valid_until 到期转换（无 LLM）
    ExpireRun {
        #[arg(long)]
        config: PathBuf,
    },
    /// doc7（Riko-Muse）M2/M3：对一个 scope 执行 rupture 扫描 + synthesis 刷新（无 LLM）
    MuseScan {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
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
enum RelationshipsAction {
    /// 重建关系实体与条目（整体替换，batch_version 递增）
    Refresh {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "user_main")]
        domain: String,
    },
}

#[derive(Subcommand)]
enum DerivedAction {
    /// 重建 compact_memory 与四个分面（同事务整体替换，batch_version 递增）
    Refresh {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "user_main")]
        domain: String,
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
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        agent: String,
    },
    /// 导入 soul.md（CAS；expected-version 必填，初次创建用 0；doc6/03 §1）
    Import {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        expected_version: i64,
    },
    /// 导出当前正文到文件（临时文件 + 原子替换；版本写 <out>.version sidecar）
    Export {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// 列出历史版本元数据（正文用 show/HTTP 单独读取）
    History {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        agent: String,
    },
}

#[derive(Subcommand)]
enum ResidentAction {
    /// 固定一条记忆（显式 position 即重排；doc6/02 §2）
    Pin {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        #[arg(long)]
        position: Option<i64>,
        #[arg(long)]
        expected_pin_version: Option<i64>,
    },
    /// 解除固定（行保留，enabled=0）
    Unpin {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        #[arg(long)]
        expected_pin_version: Option<i64>,
    },
    /// 重排到目标下标（越界钳制到末尾）
    Move {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        memory_id: String,
        #[arg(long)]
        position: i64,
        #[arg(long)]
        expected_pin_version: Option<i64>,
    },
    /// 列出 enabled pin 与当前可见性/原因（doc6/03 §5）
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
    },
    /// 导出 pinned 清单 Markdown（D6-2 视图：仅 pinned 区；预算/召回归 D6-3 bundle）
    Export {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum QuestionsAction {
    /// 列出问题（含 archived；默认全部）
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        status: Option<String>,
    },
    /// 登记问题（key 1—64 个 [a-z0-9_]；正文 1—200 标量；key 须不存在）
    Add {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        text: String,
    },
    /// 修改问题正文（CAS；旧画像同事务立即 stale）
    Update {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        expected_version: i64,
    },
    /// 归档问题（用户"删除"首版行为；旧画像同事务 stale）
    Archive {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        expected_version: i64,
    },
    /// 重新启用（按新版本重新生成）
    Reactivate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        expected_version: i64,
    },
}

#[derive(Subcommand)]
enum PagesAction {
    /// 列出页面（默认 published；可看 stale/archived）
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 显示单页（读时复核来源；失效页不显示正文）
    Show {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        page_id: String,
    },
    /// 归档页面（CAS；保留 revision，不再搜索/注入）
    Archive {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        page_id: String,
        #[arg(long)]
        expected_version: i64,
    },
}

#[derive(Subcommand)]
enum ConsolidateAction {
    /// 显式入队一次整理（自定义触发；doc6/05 §2）。mental_model 按已登记问题
    /// 文本词法选输入；topic_page 按 key 词法选输入（≥2 条）。
    Enqueue {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        key: String,
        /// 覆盖检索词；缺省 mental_model 用问题正文、topic_page 用 key
        #[arg(long)]
        query: Option<String>,
    },
    /// 列出整理作业（诊断）
    Status {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 重试一个 dead/终态作业（显式完整 job ID）
    Retry {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        job_id: String,
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
    /// Auto Dream 定时调度默认启用；关闭只停止自动触发，不禁用显式任务的 runner。
    #[serde(default)]
    dream: DreamConfig,
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
    /// embeddings provider 请求超时秒数（默认 30；查询路径不另设延迟预算）。
    embedding_timeout_secs: Option<u64>,
    /// 在线 query 向量召回的余弦相似度下限（含边界；默认 0.3）。
    semantic_min_similarity: Option<f32>,
    /// 专用 rerank endpoint（Jina/Cohere 兼容形状）；不配 reranker 时保留 RRF 顺序。
    rerank_endpoint: Option<String>,
    rerank_model: Option<String>,
    rerank_key_file: Option<PathBuf>,
    /// episode Retrieved 排序的 recency 模式：linear|exponential|none（默认 linear）。
    recency_mode: Option<String>,

    /// V2-S1 记忆域（doc7/04 §2.1）：缺省 false，全部请求解析为 user_main，
    /// 行为与 schema 14 一致；启用后管理端点才可用、域头才生效。
    #[serde(default)]
    domains: DomainsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DomainsConfig {
    enabled: bool,
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DreamConfig {
    enabled: bool,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Config {
    fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置文件失败 {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| format!("解析配置文件失败 {}: {e}", path.display()))?;
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
        if let Some(floor) = self.semantic_min_similarity {
            if !floor.is_finite() || !(0.0..=1.0).contains(&floor) {
                return Err("semantic_min_similarity 必须是 0 到 1 之间的有限数值".into());
            }
        }
        Ok(self.clone())
    }
}

const DEFAULT_SEMANTIC_MIN_SIMILARITY: f32 = 0.3;

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    /// D6-8：embedding/reranker 客户端（未配置为 None，语义支路降级）。
    embedding: Option<Arc<embedding::EmbeddingClient>>,
    rerank: Option<Arc<embedding::RerankClient>>,
    /// Query-time semantic candidate floor; Dream scans intentionally do not use it.
    semantic_min_similarity: f32,
    /// recency 模式（doc6/04 §3.1：linear|exponential|none，默认 linear）。
    recency_mode: &'static str,
    /// V2-S1：记忆域总开关（doc7/04 §2.1）。false 时域头与管理端点全部不生效。
    domains_enabled: bool,
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
                store
                    .doctor_summary()
                    .unwrap_or_else(|e| format!("诊断失败: {e}"))
            );
            // D6-8：embedding/reranker 客户端（全部显式配置；未配置即 disabled）。
            let embedding_client = match (
                &cfg.embedding_endpoint,
                &cfg.embedding_model,
                &cfg.embedding_key_file,
                cfg.embedding_dimensions,
            ) {
                (Some(endpoint), Some(name), Some(key_file), Some(dims)) => {
                    let api_key = std::fs::read_to_string(key_file)
                        .map_err(|e| {
                            format!("读取 embedding 密钥文件失败 {}: {e}", key_file.display())
                        })?
                        .trim()
                        .to_string();
                    if api_key.is_empty() && !cfg.model_allow_empty_key.unwrap_or(false) {
                        return Err("embedding 密钥文件为空；如确需无密钥端点请显式设置 model_allow_empty_key=true".into());
                    }
                    let client = embedding::EmbeddingClient::new(embedding::EmbeddingConfig {
                        endpoint: endpoint.clone(),
                        model: name.clone(),
                        api_key,
                        timeout: std::time::Duration::from_secs(
                            cfg.embedding_timeout_secs.unwrap_or(30),
                        ),
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
            let rerank_client = match (
                &cfg.rerank_endpoint,
                &cfg.rerank_model,
                &cfg.rerank_key_file,
            ) {
                (Some(endpoint), Some(name), Some(key_file)) => {
                    let api_key = std::fs::read_to_string(key_file)
                        .map_err(|e| {
                            format!("读取 rerank 密钥文件失败 {}: {e}", key_file.display())
                        })?
                        .trim()
                        .to_string();
                    if api_key.is_empty() && !cfg.model_allow_empty_key.unwrap_or(false) {
                        return Err("rerank 密钥文件为空；如确需无密钥端点请显式设置 model_allow_empty_key=true".into());
                    }
                    let client = embedding::RerankClient::new(embedding::RerankConfig {
                        endpoint: endpoint.clone(),
                        model: name.clone(),
                        api_key,
                        timeout: std::time::Duration::from_secs(
                            cfg.embedding_timeout_secs.unwrap_or(30),
                        ),
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
                semantic_min_similarity: cfg
                    .semantic_min_similarity
                    .unwrap_or(DEFAULT_SEMANTIC_MIN_SIMILARITY),
                recency_mode: match cfg.recency_mode.as_deref() {
                    Some("none") => "none",
                    Some("exponential") => "exponential",
                    _ => memory_contract::RECENCY_MODE_DEFAULT,
                },
                domains_enabled: cfg.domains.enabled,
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
            worker::spawn_worker(state.clone(), model_cfg);
            // Dream 模型调用只能来自 DSH 常驻 runner；memoryd 仅运行语义索引作业。
            dream_worker::spawn_dream_pipeline(state.clone(), None, embedding_client, false);
            // D6-7：Auto Dream scheduler（doc6/10 §4.2，默认启用；memoryd 内置受控
            // runner，doc6/10 §8 路径——由持久 trigger/jobs 驱动）。周期 15 分钟
            // tick；每 scope 24 小时一次 + 空闲 15 分钟 + ≥1 条新 user event 才入队。
            // 缺少 DSH chat、embedding 或 live runner 时保留队列与 L0，doctor 报具体缺项。
            if cfg.dream.enabled {
                let sched_state = state.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
                    loop {
                        tick.tick().await;
                        let Ok(mut store) = sched_state.store.lock() else {
                            continue;
                        };
                        let Ok(now) = memory_store_sqlite::now_rfc3339_pub() else {
                            continue;
                        };
                        let Ok(due) = store.dream_auto_due(&now, 24, 15) else {
                            continue;
                        };
                        for (tenant, user, _) in due {
                            let scope = ScopeKey {
                                tenant_id: tenant,
                                user_id: user,
                            };
                            let key = format!("auto-{}", &now[..10.min(now.len())]);
                            let _ = store.dream_trigger(
                                &scope,
                                "scheduled",
                                &key,
                                None,
                                None,
                                None,
                                &memory_domain::DomainScope::user_main(),
                            );
                        }
                    }
                });
            } else {
                eprintln!("[memoryd] Auto Dream 已关闭：保留已持久作业与 L0，不新建定时 trigger");
            }
            // Retention 是纯 Rust/SQLite 清理，不调用模型；仅扫描用户已经启用
            // 正值策略的 scope。retention_run 在单一 IMMEDIATE transaction 内
            // 复核策略版本、删除依赖闭包并写入完成回执。
            let retention_state = state.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
                loop {
                    tick.tick().await;
                    let scopes = {
                        let Ok(store) = retention_state.store.lock() else {
                            eprintln!("[memoryd] retention scheduler 无法取得 Store 锁");
                            continue;
                        };
                        match store.retention_enabled_scopes() {
                            Ok(scopes) => scopes,
                            Err(error) => {
                                eprintln!("[memoryd] retention scope 扫描失败: {error}");
                                continue;
                            }
                        }
                    };
                    for scope in scopes {
                        let result = match retention_state.store.lock() {
                            Ok(mut store) => store.retention_run(&scope),
                            Err(_) => {
                                eprintln!("[memoryd] retention scheduler 无法取得 Store 锁");
                                continue;
                            }
                        };
                        match result {
                            Ok(Some(result)) => {
                                eprintln!("[memoryd] retention 批次完成: {}", result)
                            }
                            Ok(None) => {}
                            Err(error) => eprintln!("[memoryd] retention 批次失败: {error}"),
                        }
                    }
                }
            });
            // doc7（Riko-Muse）：M1 到期转换 + M2 rupture 扫描 + M3 synthesis 刷新。
            // 纯 Rust/SQLite，无 LLM（doc7/01 §2/§6）；15 分钟 tick，与 retention 同模式。
            let muse_state = state.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
                loop {
                    tick.tick().await;
                    // M1：到期记忆转 expired（全 scope 单批）。
                    let expired = {
                        match muse_state.store.lock() {
                            Ok(mut s) => s.expire_due_memories(),
                            Err(_) => {
                                eprintln!("[memoryd] Muse 调度器无法取得 Store 锁");
                                continue;
                            }
                        }
                    };
                    match expired {
                        Ok(0) => {}
                        Ok(n) => eprintln!("[memoryd] 到期转换：{n} 条记忆已转 expired"),
                        Err(error) => eprintln!("[memoryd] 到期转换失败: {error}"),
                    }
                    // M2/M3：逐 scope 扫描 rupture 并按策略刷新 synthesis。
                    let scopes = {
                        match muse_state.store.lock() {
                            Ok(s) => match s.all_scopes() {
                                Ok(scopes) => scopes,
                                Err(error) => {
                                    eprintln!("[memoryd] Muse scope 扫描失败: {error}");
                                    continue;
                                }
                            },
                            Err(_) => continue,
                        }
                    };
                    for scope in scopes {
                        let scan = match muse_state.store.lock() {
                            Ok(mut s) => {
                                s.rupture_scan(&scope, &memory_domain::DomainScope::user_main())
                            }
                            Err(_) => {
                                eprintln!("[memoryd] Muse 调度器无法取得 Store 锁");
                                continue;
                            }
                        };
                        match scan {
                            Ok(out) if out.inserted_ruptures > 0 || out.opened_threads > 0 => {
                                eprintln!(
                                    "[memoryd] rupture 扫描（{}/{}）：新事件 {} 条、新线程 {}",
                                    scope.tenant_id,
                                    scope.user_id,
                                    out.inserted_ruptures,
                                    out.opened_threads
                                );
                            }
                            Ok(_) => {}
                            Err(error) => {
                                eprintln!("[memoryd] rupture 扫描失败: {error}");
                                continue;
                            }
                        }
                        let refreshed = match muse_state.store.lock() {
                            Ok(mut s) => s.alignment_synthesis_refresh(
                                &scope,
                                &memory_domain::DomainScope::user_main(),
                            ),
                            Err(_) => {
                                eprintln!("[memoryd] Muse 调度器无法取得 Store 锁");
                                continue;
                            }
                        };
                        if let Err(error) = refreshed {
                            eprintln!("[memoryd] synthesis 刷新失败: {error}");
                        }
                    }
                }
            });
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
                .route("/v1/memories/{memory_id}/explain", get(explain_memory))
                // V2-D1（doc7/06 §5）：蒸馏视图
                .route("/v1/compact", get(get_compact))
                .route("/v1/facets", get(get_facets))
                .route("/v1/derived/refresh", post(refresh_derived))
                // V2-R1（doc7/07 §5）：关系图谱。静态段先于 {entity_id} 注册。
                .route("/v1/relationships", get(list_relationships))
                .route("/v1/relationships/resolve", get(resolve_relationship))
                .route("/v1/relationships/refresh", post(refresh_relationships))
                .route("/v1/relationships/{entity_id}", get(get_relationship))
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
                .route("/v1/dream/runner/heartbeat", post(dream_runner_heartbeat))
                .route("/v1/dream/runner/claim", post(dream_runner_claim))
                .route("/v1/dream/runner/lease", post(dream_runner_lease))
                .route("/v1/dream/runner/failure", post(dream_runner_failure))
                .route(
                    "/v1/dream/jobs/{job_id}/candidates",
                    post(dream_runner_candidates),
                )
                .route(
                    "/v1/dream/adjudications/{job_id}/submit",
                    post(dream_runner_adjudication),
                )
                .route(
                    "/v1/consolidation/jobs/{job_id}/publish",
                    post(dream_runner_publish_page),
                )
                .route(
                    "/v1/dream/candidates/{candidate_id}/rejudge",
                    post(post_dream_rejudge),
                )
                .route("/v1/dream/jobs", get(list_dream_jobs))
                .route("/v1/dream/jobs/{job_id}", get(get_dream_job))
                .route("/v1/dream/jobs/{job_id}/read", post(dream_scoped_read))
                .route("/v1/resident/page-pins", post(post_page_pin))
                .route("/v1/resident/page-pins/{page_id}", delete(delete_page_pin))
                // doc7（Riko-Muse）：alignment synthesis、repair 线程与 rupture 诊断。
                .route(
                    "/v1/alignment/synthesis",
                    get(get_alignment_synthesis).post(refresh_alignment_synthesis),
                )
                .route("/v1/repair/threads", get(list_repair_threads))
                .route(
                    "/v1/repair/threads/{thread_id}/close",
                    post(close_repair_thread),
                )
                .route("/v1/ruptures", get(list_ruptures))
                // V2-S1（doc7/04 §4）：记忆域自助配置。enabled=false 时全部 403。
                .route("/v1/domains", get(list_domains).post(create_domain))
                .route("/v1/domains/close", post(close_domain_endpoint))
                .route(
                    "/v1/domains/bindings",
                    get(list_domain_bindings).post(put_domain_binding),
                )
                .route(
                    "/v1/domains/bindings/{host_id}/{session_id}",
                    delete(delete_domain_binding),
                )
                .route(
                    "/v1/domains/grants",
                    get(list_domain_grants).post(create_domain_grant),
                )
                .route("/v1/domains/grants/{grant_id}", delete(delete_domain_grant))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    request_pipeline,
                ))
                .with_state(state);
            eprintln!("[memoryd] 监听 {addr}（loopback only）");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        Commands::Relationships { action } => match action {
            RelationshipsAction::Refresh {
                config,
                tenant,
                user,
                domain,
            } => {
                let cfg = Config::load(&config)?;
                let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
                let scope = ScopeKey {
                    tenant_id: tenant,
                    user_id: user,
                };
                let dom = cli_domain_scope(&store, &scope, &domain)?;
                let out = store
                    .relationship_refresh(&scope, &dom)
                    .map_err(|e| e.to_string())?;
                println!(
                    "已重建关系图谱 batch_version={} 实体={} 条目={}（上一批实体 {}）",
                    out.batch_version, out.entities, out.items, out.previous_entities
                );
                Ok(())
            }
        },
        Commands::Derived { action } => match action {
            DerivedAction::Refresh {
                config,
                tenant,
                user,
                domain,
            } => {
                let cfg = Config::load(&config)?;
                let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
                let scope = ScopeKey {
                    tenant_id: tenant,
                    user_id: user,
                };
                let dom = cli_domain_scope(&store, &scope, &domain)?;
                let out = store
                    .derived_refresh(&scope, &dom)
                    .map_err(|e| e.to_string())?;
                println!(
                    "已重建 batch_version={} compact={} 分面={}（上一批条目 {}）",
                    out.batch_version, out.compact_items, out.facet_items, out.stale_removed
                );
                Ok(())
            }
        },
        Commands::Export {
            config,
            tenant,
            user,
            out,
            domain,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let dom = cli_domain_scope(&store, &scope, &domain)?;
            let written = export_markdown_projection(&mut store, &scope, &dom, &out)?;
            println!("已导出 {} 个文件到 {}", written, out.display());
            Ok(())
        }
        Commands::Job { action } => match action {
            JobAction::Skip {
                config,
                tenant,
                user,
                job_id,
                reason,
            } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    let reason_chars = reason.chars().count();
                    if reason_chars == 0 || reason_chars > 256 {
                        return Err("skip reason 必须 1—256 个字符，且不得填用户正文或密钥".into());
                    }
                    let mut store = Store::open(&cfg.db_path, &cfg.migrations_dir)
                        .map_err(|e| e.to_string())?;
                    let scope = ScopeKey {
                        tenant_id: tenant,
                        user_id: user,
                    };
                    // 先按 scope 精确查询，缺失/跨 scope 与状态不符给出可区分错误。
                    if store
                        .get_job(&scope, &job_id)
                        .map_err(|e| e.to_string())?
                        .is_none()
                    {
                        return Err(format!(
                            "作业 {job_id} 不存在或不属于当前 scope，未变更任何行"
                        ));
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
            CandidatesAction::List {
                config,
                tenant,
                user,
                status,
                limit,
                before,
            } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    if !(1..=100).contains(&limit) {
                        return Err("limit 必须 1～100".into());
                    }
                    if !matches!(status.as_str(), "held" | "candidate" | "rejected") {
                        return Err("status 必须是 held/candidate/rejected".into());
                    }
                    let store = Store::open(&cfg.db_path, &cfg.migrations_dir)
                        .map_err(|e| e.to_string())?;
                    let scope = ScopeKey {
                        tenant_id: tenant,
                        user_id: user,
                    };
                    let rows = store
                        .list_candidates(&scope, &status, limit, before.as_deref())
                        .map_err(|e| match e {
                            StoreError::JobNotFound => {
                                "before 候选不存在或不属于当前 scope".to_string()
                            }
                            other => other.to_string(),
                        })?;
                    println!(
                        "{:<40} {:<12} {:<24} {:<30} {:<24} {}",
                        "ID", "kind", "reason", "created_at", "evidence_id", "quote_len"
                    );
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
            CandidatesAction::Show {
                config,
                tenant,
                user,
                id,
            } => {
                let run = || -> Result<(), String> {
                    let cfg = Config::load(&config)?;
                    let store = Store::open(&cfg.db_path, &cfg.migrations_dir)
                        .map_err(|e| e.to_string())?;
                    let scope = ScopeKey {
                        tenant_id: tenant,
                        user_id: user,
                    };
                    let detail: Option<CandidateDetail> = store
                        .get_candidate(&scope, &id)
                        .map_err(|e| e.to_string())?;
                    let Some(c) = detail else {
                        return Err(format!("候选 {id} 不存在或不属于当前 scope"));
                    };
                    println!("id: {}", c.id);
                    println!(
                        "kind: {} status: {} reason: {}",
                        c.kind,
                        c.status,
                        c.reason_code.clone().unwrap_or_else(|| "-".into())
                    );
                    println!("created_at: {}", c.created_at);
                    println!("primary_evidence_id: {}", c.primary_evidence_id);
                    println!(
                        "evidence_span: {}..{}",
                        c.evidence_start_byte
                            .map(|v| v.to_string())
                            .unwrap_or("-".into()),
                        c.evidence_end_byte
                            .map(|v| v.to_string())
                            .unwrap_or("-".into())
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
            PrincipalAction::Add {
                tenant,
                user,
                token_out,
                db,
                migrations,
            } => {
                let mut store = Store::open(&db, &migrations)?;
                store.principal_add(&tenant, &user, &token_out)?;
                println!(
                    "已创建 principal tenant={tenant} user={user}，令牌写入 {}",
                    token_out.display()
                );
                Ok(())
            }
            PrincipalAction::RotateToken {
                tenant,
                user,
                token_out,
                db,
                migrations,
            } => {
                let mut store = Store::open(&db, &migrations)?;
                store.principal_rotate_token(&tenant, &user, &token_out)?;
                println!(
                    "已轮换 tenant={tenant} user={user} 的令牌，原令牌立即失效，新令牌写入 {}",
                    token_out.display()
                );
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
            let now = memory_store_sqlite::now_rfc3339_pub()?;
            let chat_ready = store.dream_live_runner_capability_count(&now, "chat")? > 0;
            let embedding_ready = cfg.embedding_endpoint.is_some()
                && cfg.embedding_model.is_some()
                && cfg.embedding_key_file.is_some()
                && cfg.embedding_dimensions.is_some();
            let runner_ready = store.dream_live_runner_count(&now)? > 0;
            let missing = [
                (!chat_ready, "chat"),
                (!embedding_ready, "embedding"),
                (!runner_ready, "runner"),
            ]
            .into_iter()
            .filter_map(|(is_missing, name)| is_missing.then_some(name))
            .collect::<Vec<_>>();
            let readiness = if !cfg.dream.enabled {
                "scheduler_disabled".to_string()
            } else if missing.is_empty() {
                "ready".to_string()
            } else {
                format!("missing_{}", missing.join(","))
            };
            println!(
                "dream.auto_enabled={} dream_readiness={} chat={} embedding={} runner={}",
                cfg.dream.enabled,
                readiness,
                if chat_ready { "configured" } else { "missing" },
                if embedding_ready {
                    "configured"
                } else {
                    "missing"
                },
                if runner_ready {
                    "online"
                } else {
                    "missing_runner"
                },
            );
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
                return Err(
                    "embedding 未配置：reindex-semantic 无意义（语义支路 disabled）".into(),
                );
            }
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let n = store.semantic_reindex_all(cfg.embedding_model.as_deref().unwrap())?;
            println!("reindex-semantic 完成：入队 {n} 个对象（worker 将按当前版本生成向量）");
            Ok(())
        }
        Commands::Retire {
            config,
            tenant,
            user,
            memory_id,
            expected_version,
            evidence_id,
            quote,
            idempotency_key,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let origin = Origin {
                host_id: "cli".into(),
                agent_id: "admin".into(),
                session_id: "cli".into(),
            };
            // Rust 核逐字 span（最新用户事件）+ 目标 claim 双向定位（G-13 同法）。
            let (start_byte, end_byte) = store
                .verify_user_quote_span(&scope, &origin, &evidence_id, &quote)
                .map_err(|e| format!("quote 核验失败: {e}"))?;
            let claim_ok = store
                .get_memory(
                    &scope,
                    &memory_id,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
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
                idempotency_key,
                origin,
                user_evidence_id: evidence_id,
                target_quote: quote,
                start_byte: start_byte as i64,
                end_byte: end_byte as i64,
            };
            let retired = store
                .retire_memory(
                    &scope,
                    &memory_id,
                    &req,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            println!("retire 完成：memory_id={memory_id} retired={retired}");
            Ok(())
        }
        Commands::Restore {
            config,
            tenant,
            user,
            memory_id,
            expected_version,
            evidence_id,
            quote,
            idempotency_key,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let origin = Origin {
                host_id: "cli".into(),
                agent_id: "admin".into(),
                session_id: "cli".into(),
            };
            let (start_byte, end_byte) = store
                .verify_user_quote_span(&scope, &origin, &evidence_id, &quote)
                .map_err(|e| format!("quote 核验失败: {e}"))?;
            let req = memory_store_sqlite::lifecycle::RestoreRequest {
                expected_version,
                actor_kind: "user",
                idempotency_key,
                origin,
                user_evidence_id: evidence_id,
                target_quote: quote,
                start_byte: start_byte as i64,
                end_byte: end_byte as i64,
            };
            let restored = store
                .restore_memory(
                    &scope,
                    &memory_id,
                    &req,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            println!("restore 完成：memory_id={memory_id} restored={restored}");
            Ok(())
        }
        Commands::PurgePreview {
            config,
            tenant,
            user,
            memory_id,
            idempotency_key,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let (token, preview) = store
                .purge_preview(
                    &scope,
                    &memory_id,
                    &idempotency_key,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            // 明文 token 只输出一次（确认后即弃；库中仅存哈希）。
            println!("preview token（一次性，15 分钟内有效）：{token}");
            println!(
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Commands::PurgeConfirm {
            config,
            tenant,
            user,
            token,
            idempotency_key,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let out = store
                .purge_confirm(
                    &scope,
                    &token,
                    &idempotency_key,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            println!(
                "purge confirm 完成：job_id={} deleted={}",
                out.job_id, out.deleted
            );
            Ok(())
        }
        Commands::RetentionPolicy {
            config,
            tenant,
            user,
            raw_days,
            expired_days,
            enabled,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let v = store
                .retention_set_policy(&scope, raw_days, expired_days, enabled)
                .map_err(|e| e.to_string())?;
            println!("retention 策略已设置：version={v} raw_days={raw_days} expired_days={expired_days} enabled={enabled}");
            Ok(())
        }
        Commands::RetentionRun {
            config,
            tenant,
            user,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            match store.retention_run(&scope).map_err(|e| e.to_string())? {
                Some(r) => println!(
                    "retention 完成：{}",
                    serde_json::to_string(&r).map_err(|e| e.to_string())?
                ),
                None => println!("retention 无操作（策略未配置/关闭或本批次已执行）"),
            }
            Ok(())
        }
        Commands::ExpireRun { config } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let n = store.expire_due_memories().map_err(|e| e.to_string())?;
            println!("到期转换完成：{n} 条记忆已转 expired");
            Ok(())
        }
        Commands::MuseScan {
            config,
            tenant,
            user,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let scan = store
                .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
                .map_err(|e| e.to_string())?;
            println!(
                "rupture 扫描完成：扫描事件 {} 条、新 rupture {} 条、新线程 {}",
                scan.scanned_events, scan.inserted_ruptures, scan.opened_threads
            );
            let synthesis = store
                .alignment_synthesis_refresh(
                    &scope,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            println!(
                "synthesis 版本 {}：窗口 {} → {}，纠正 {}/{}，无纠正率 {:.2}，待修复线程 {}",
                synthesis.version,
                synthesis.window_since,
                synthesis.window_until,
                synthesis.rupture_turns,
                synthesis.user_turns,
                synthesis.correction_free_rate,
                synthesis.open_repair_threads
            );
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

fn memory_claim_hash(kind: &str, claim: &str) -> Option<String> {
    let parsed = match kind {
        "fact" => MemoryKind::Fact,
        "preference" => MemoryKind::Preference,
        "instruction" => MemoryKind::Instruction,
        "episode" => MemoryKind::Episode,
        _ => return None,
    };
    Some(memory_domain::claim_sha256(parsed, claim))
}

/// 导出文件原子替换（doc6/02 §8.6）：同目录临时文件 + write_all + sync_all + rename。
fn atomic_write_file(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写入临时文件失败: {e}"))?;
        f.sync_all().map_err(|e| format!("刷盘失败: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("原子替换失败: {e}"))
}

/// Markdown 单行化：折叠换行，避免用户正文破坏导出文件结构（doc6/03 §2）。
fn md_single_line(text: &str) -> String {
    text.split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_soul_action(action: SoulAction) -> Result<(), String> {
    match action {
        SoulAction::Show {
            config,
            tenant,
            user,
            agent,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
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
        SoulAction::Import {
            config,
            tenant,
            user,
            agent,
            file,
            expected_version,
        } => {
            let cfg = Config::load(&config)?;
            if expected_version < 0 {
                return Err("expected-version 不能为负；初次创建用 0".into());
            }
            let body = std::fs::read_to_string(&file)
                .map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
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
            println!(
                "body_sha256: {}",
                profile.map(|p| p.body_sha256).unwrap_or_default()
            );
            let _ = version;
            Ok(())
        }
        SoulAction::Export {
            config,
            tenant,
            user,
            agent,
            out,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
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
        SoulAction::History {
            config,
            tenant,
            user,
            agent,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let revisions = store
                .list_soul_revisions(&scope, &agent)
                .map_err(|e| e.to_string())?;
            if revisions.is_empty() {
                println!("（无历史版本）");
                return Ok(());
            }
            println!(
                "{:<10} {:<20} {:<12} {}",
                "version", "body_sha256", "actor", "changed_at"
            );
            for r in revisions {
                println!(
                    "{:<10} {:<20} {:<12} {}",
                    r.version,
                    &r.body_sha256[..20.min(r.body_sha256.len())],
                    r.actor_kind,
                    r.changed_at
                );
            }
            Ok(())
        }
    }
}

fn run_resident_action(action: ResidentAction) -> Result<(), String> {
    match action {
        ResidentAction::Pin {
            config,
            tenant,
            user,
            memory_id,
            position,
            expected_pin_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            match store.resident_pin(&scope, &memory_id, position, expected_pin_version, None) {
                Ok(memory_store_sqlite::resident::PinOutcome::Pinned { version, position }) => {
                    println!("已固定 {memory_id}：pin_version={version} position={position}");
                    Ok(())
                }
                Ok(memory_store_sqlite::resident::PinOutcome::Unchanged { version, position }) => {
                    println!(
                        "{memory_id} 已固定（幂等）：pin_version={version} position={position}"
                    );
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => {
                    Err("记忆不存在或不属于当前 scope（404 语义），未变更".into())
                }
                Err(StoreError::VersionConflict) => Err(
                    "pin 版本冲突（409）：expected-pin-version 与当前不符；用 resident list 查看"
                        .into(),
                ),
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::Unpin {
            config,
            tenant,
            user,
            memory_id,
            expected_pin_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            match store.resident_unpin(&scope, &memory_id, expected_pin_version, None) {
                Ok(o) => {
                    if o.already_disabled {
                        println!(
                            "{memory_id} 已处于解除状态（幂等）：pin_version={}",
                            o.version
                        );
                    } else {
                        println!("已解除固定 {memory_id}：pin_version={}", o.version);
                    }
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => {
                    Err("记忆不存在或不属于当前 scope（404 语义），未变更".into())
                }
                Err(StoreError::VersionConflict) => {
                    Err("pin 版本冲突（409）：expected-pin-version 与当前不符".into())
                }
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::Move {
            config,
            tenant,
            user,
            memory_id,
            position,
            expected_pin_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            match store.resident_move(&scope, &memory_id, position, expected_pin_version) {
                Ok(version) => {
                    println!("已重排 {memory_id} 到 position={position}：pin_version={version}");
                    Ok(())
                }
                Err(StoreError::MemoryNotFound) => {
                    Err("pin 行/记忆不存在或不属于当前 scope（404 语义）".into())
                }
                Err(StoreError::VersionConflict) => Err("pin 版本冲突（409）".into()),
                Err(StoreError::StateConflict) => Err("disabled 行不可重排；先重新 pin".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ResidentAction::List {
            config,
            tenant,
            user,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let rows = store
                .resident_pins_with_status(
                    &scope,
                    &now,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无 enabled pin）");
                return Ok(());
            }
            println!(
                "{:<40} {:<6} {:<8} {:<14} {:<20} {}",
                "memory_id", "pos", "pin_ver", "status", "visible", "reason"
            );
            for r in rows {
                println!(
                    "{:<40} {:<6} {:<8} {:<14} {:<20} {}",
                    r.memory_id, r.position, r.version, r.memory_status, r.visible, r.reason
                );
            }
            Ok(())
        }
        ResidentAction::Export {
            config,
            tenant,
            user,
            out,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let rows = store
                .resident_pins_with_status(
                    &scope,
                    &now,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
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
                    .get_memory_claim(
                        &scope,
                        &r.memory_id,
                        /*DOM*/ &memory_domain::DomainScope::user_main(),
                    )
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| "（正文不可读）".into());
                let visibility = if r.visible {
                    "可见".into()
                } else {
                    format!("不可见（{}）", r.reason)
                };
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
        .search_memories(
            scope,
            query,
            20,
            false,
            /*DOM*/ &memory_domain::DomainScope::user_main(),
        )
        .map_err(|e| e.to_string())?;
    let mut inputs: Vec<(String, i64, String)> = Vec::new();
    for hit in hits {
        let (v, s) = store
            .memory_version_sha(
                scope,
                &hit.memory_id,
                /*DOM*/ &memory_domain::DomainScope::user_main(),
            )
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("记忆 {} 读取失败", hit.memory_id))?;
        inputs.push((hit.memory_id, v, s));
    }
    Ok(inputs)
}

fn run_questions_action(action: QuestionsAction) -> Result<(), String> {
    match action {
        QuestionsAction::List {
            config,
            tenant,
            user,
            status,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let rows = store
                .question_list(&scope, status.as_deref())
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（问题目录为空——doc6/05：首版默认空，Dream 不生成画像）");
                return Ok(());
            }
            println!(
                "{:<28} {:<8} {:<10} {:<22} {}",
                "key", "version", "status", "updated_at", "text"
            );
            for r in rows {
                println!(
                    "{:<28} {:<8} {:<10} {:<22} {}",
                    r.question_key, r.version, r.status, r.updated_at, r.question_text
                );
            }
            Ok(())
        }
        QuestionsAction::Add {
            config,
            tenant,
            user,
            key,
            text,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let v = store
                .question_add(&scope, &key, &text, "user_cli")
                .map_err(|e| match e {
                    StoreError::InvalidQuestionKey => "问题键须为 1—64 个 ASCII [a-z0-9_]".into(),
                    StoreError::InvalidQuestionText => {
                        "问题正文须 1—200 个 Unicode 标量字符".into()
                    }
                    StoreError::StateConflict => "该问题键已存在（add 要求 key 不存在）".into(),
                    other => other.to_string(),
                })?;
            println!("已登记问题 {key} version={v}");
            Ok(())
        }
        QuestionsAction::Update {
            config,
            tenant,
            user,
            key,
            text,
            expected_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let v = store
                .question_update(&scope, &key, &text, expected_version, "user_cli")
                .map_err(|e| match e {
                    StoreError::QuestionNotFound => "问题不存在或不属于当前 scope".into(),
                    other => other.to_string(),
                })?;
            println!("已更新问题 {key} version={v}（旧画像已立即 stale）");
            Ok(())
        }
        QuestionsAction::Archive {
            config,
            tenant,
            user,
            key,
            expected_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let v = store
                .question_archive(&scope, &key, expected_version, "user_cli")
                .map_err(|e| e.to_string())?;
            println!("已归档问题 {key} version={v}（旧画像已立即 stale）");
            Ok(())
        }
        QuestionsAction::Reactivate {
            config,
            tenant,
            user,
            key,
            expected_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let v = store
                .question_reactivate(&scope, &key, expected_version, "user_cli")
                .map_err(|e| e.to_string())?;
            println!("已重新启用问题 {key} version={v}（按新版本与当前有效 L1 重新生成）");
            Ok(())
        }
    }
}

fn run_pages_action(action: PagesAction) -> Result<(), String> {
    match action {
        PagesAction::List {
            config,
            tenant,
            user,
            status,
            limit,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            if !(1..=100).contains(&limit) {
                return Err("limit 必须 1～100".into());
            }
            let statuses: Vec<&str> = match status.as_deref() {
                None => vec![],
                Some(s) => s.split(',').map(str::trim).collect(),
            };
            let rows = store
                .page_list(
                    &scope,
                    &statuses,
                    limit,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无页面）");
                return Ok(());
            }
            println!(
                "{:<40} {:<14} {:<24} {:<10} {:<8} {:<22} {}",
                "page_id", "kind", "key", "status", "version", "updated_at", "title"
            );
            for r in rows {
                println!(
                    "{:<40} {:<14} {:<24} {:<10} {:<8} {:<22} {}",
                    r.page_id,
                    r.document_kind,
                    r.document_key,
                    r.status,
                    r.version,
                    r.updated_at,
                    r.title
                );
            }
            Ok(())
        }
        PagesAction::Show {
            config,
            tenant,
            user,
            page_id,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            match store
                .get_page(
                    &scope,
                    &page_id,
                    &now,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?
            {
                None => Err(format!(
                    "页面 {page_id} 不存在、非 published 或来源已失效（读时复核）"
                )),
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
        PagesAction::Archive {
            config,
            tenant,
            user,
            page_id,
            expected_version,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let ok = store
                .page_archive(&scope, &page_id, expected_version)
                .map_err(|e| e.to_string())?;
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
        ConsolidateAction::Enqueue {
            config,
            tenant,
            user,
            kind,
            key,
            query,
        } => {
            if !matches!(kind.as_str(), "mental_model" | "topic_page") {
                return Err("kind 必须是 mental_model/topic_page".into());
            }
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            // mental_model：问题必须已登记且 active（doc6/05 §2 空目录跳过画像）。
            let question_version = if kind == "mental_model" {
                let q = store
                    .question_list(&scope, Some("active"))
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|q| q.question_key == key)
                    .ok_or_else(|| {
                        format!("问题 {key} 未登记或非 active——先 mental-model questions add")
                    })?;
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
                return Err(format!(
                    "主题页至少需要 2 条 active 来源（当前 {} 条）；doc6/05 §2 不满足不生成",
                    inputs.len()
                ));
            }
            if kind == "mental_model" && inputs.is_empty() {
                return Err("没有相关 active L1 来源；不生成画像".into());
            }
            let parts: Vec<String> = inputs
                .iter()
                .map(|(id, v, _)| format!("{id}:{v}"))
                .collect();
            let part_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            let fingerprint = memory_store_sqlite::soul::receipt_hash(&part_refs);
            let now = memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?;
            let generator = if kind == "mental_model" {
                memory_store_sqlite::pages::GENERATE_MENTAL_MODEL_V1
            } else {
                memory_store_sqlite::pages::GENERATE_CONSOLIDATE_V2
            };
            let (job, dream) = store
                .consolidation_enqueue_manual_dream(
                    &scope,
                    &kind,
                    &key,
                    question_version,
                    generator,
                    &fingerprint,
                    &inputs,
                    &now,
                    /*DOM*/ &memory_domain::DomainScope::user_main(),
                )
                .map_err(|e| e.to_string())?;
            println!(
                "已入队整理作业 {}（Dream trigger={} kind={kind} key={key} 输入 {} 条 status={}）",
                job.id,
                dream.id,
                inputs.len(),
                job.status
            );
            Ok(())
        }
        ConsolidateAction::Status {
            config,
            tenant,
            user,
            status,
            limit,
        } => {
            let cfg = Config::load(&config)?;
            let store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            if !(1..=100).contains(&limit) {
                return Err("limit 必须 1～100".into());
            }
            let rows = store
                .consolidation_list(&scope, status.as_deref(), limit)
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("（无整理作业）");
                return Ok(());
            }
            println!(
                "{:<40} {:<14} {:<24} {:<12} {:<6} {:<22} {}",
                "job_id", "kind", "key", "status", "gen", "updated_at", "error"
            );
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
        ConsolidateAction::Retry {
            config,
            tenant,
            user,
            job_id,
        } => {
            let cfg = Config::load(&config)?;
            let mut store = open_store_warned(&cfg.db_path, &cfg.migrations_dir)?;
            let scope = ScopeKey {
                tenant_id: tenant,
                user_id: user,
            };
            let job = store
                .consolidation_get(&scope, &job_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("作业 {job_id} 不存在或不属于当前 scope"))?;
            if job.status != "dead" && job.status != "stale_input" {
                return Err(format!(
                    "仅 dead/stale_input 作业可 retry；当前 {}",
                    job.status
                ));
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

// ---- V2-S1 记忆域管理端点（doc7/04 §4；enabled=false 时全部 403 DOMAIN_DISABLED）----

fn domains_guard(state: &AppState, req_id: &str) -> Option<Response> {
    if state.domains_enabled {
        None
    } else {
        Some(err(
            req_id,
            StatusCode::FORBIDDEN,
            ErrorCode::DomainDisabled,
            "记忆域功能未启用（[domains] enabled=false）",
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainCreateRequest {
    domain_id: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainCloseRequest {
    domain_id: String,
}

#[derive(Debug, serde::Serialize)]
struct DomainDto {
    domain_id: String,
    kind: String,
    status: String,
    policy_version: i64,
    created_reason: String,
    created_at: String,
    updated_at: String,
}

impl From<memory_store_sqlite::domains::DomainRow> for DomainDto {
    fn from(r: memory_store_sqlite::domains::DomainRow) -> Self {
        DomainDto {
            domain_id: r.domain_id,
            kind: r.kind,
            status: r.status,
            policy_version: r.policy_version,
            created_reason: r.created_reason,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct DomainsResponse {
    request_id: String,
    domains: Vec<DomainDto>,
}

#[derive(Debug, serde::Serialize)]
struct DomainWriteResponse {
    request_id: String,
    domain: DomainDto,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    created: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    changed: bool,
}

/// 可信配置者身份（doc7/04 §4：本 scope 自助配置，不由模型提供 tenant/user）。
fn domain_actor(state: &AppState) -> (&'static str, &'static str) {
    let _ = state;
    ("user", "trusted_user")
}

async fn list_domains(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.domain_list(&scope) {
        Ok(rows) => (
            StatusCode::OK,
            Json(DomainsResponse {
                request_id: req_id.0,
                domains: rows.into_iter().map(DomainDto::from).collect(),
            }),
        )
            .into_response(),
        Err(e) => domain_err(&req_id.0, &e),
    }
}

async fn create_domain(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DomainCreateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let (row, created) = match guard.domain_create_side(&scope, &body.domain_id, &body.reason) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    let (actor_kind, actor_id) = domain_actor(&state);
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &row.domain_id.clone(),
        serde_json::json!({"op": "create", "domain_id": row.domain_id, "created": created, "actor": actor_id}),
    );
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (
        status,
        Json(DomainWriteResponse {
            request_id: req_id.0,
            domain: DomainDto::from(row),
            created,
            changed: created,
        }),
    )
        .into_response()
}

async fn close_domain_endpoint(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DomainCloseRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let changed = match guard.domain_close(&scope, &body.domain_id) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    let row = match guard.domain_get(&scope, &body.domain_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err(
                &req_id.0,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "记忆域不存在",
            )
        }
        Err(e) => return domain_err(&req_id.0, &e),
    };
    let (actor_kind, actor_id) = domain_actor(&state);
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &body.domain_id,
        serde_json::json!({"op": "close", "domain_id": body.domain_id, "changed": changed, "actor": actor_id}),
    );
    (
        StatusCode::OK,
        Json(DomainWriteResponse {
            request_id: req_id.0,
            domain: DomainDto::from(row),
            created: false,
            changed,
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainBindingRequest {
    host_id: String,
    session_id: String,
    domain_id: String,
}

#[derive(Debug, serde::Serialize)]
struct DomainBindingDto {
    host_id: String,
    session_id: String,
    domain_id: String,
    registered_by: String,
    created_at: String,
}

#[derive(Debug, serde::Serialize)]
struct DomainBindingsResponse {
    request_id: String,
    bindings: Vec<DomainBindingDto>,
}

#[derive(Debug, serde::Serialize)]
struct DomainMutationResponse {
    request_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    created: bool,
}

async fn list_domain_bindings(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.domain_binding_list(&scope) {
        Ok(rows) => (
            StatusCode::OK,
            Json(DomainBindingsResponse {
                request_id: req_id.0,
                bindings: rows
                    .into_iter()
                    .map(|r| DomainBindingDto {
                        host_id: r.host_id,
                        session_id: r.session_id,
                        domain_id: r.domain_id,
                        registered_by: r.registered_by,
                        created_at: r.created_at,
                    })
                    .collect(),
            }),
        )
            .into_response(),
        Err(e) => domain_err(&req_id.0, &e),
    }
}

async fn put_domain_binding(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DomainBindingRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let (actor_kind, actor_id) = domain_actor(&state);
    let created = match guard.domain_binding_put(
        &scope,
        &body.host_id,
        &body.session_id,
        &body.domain_id,
        actor_id,
    ) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &body.session_id,
        serde_json::json!({
            "op": "bind", "host_id": body.host_id, "session_id": body.session_id,
            "domain_id": body.domain_id, "created": created, "actor": actor_id
        }),
    );
    (
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(DomainMutationResponse {
            request_id: req_id.0,
            status: "ok",
            created,
        }),
    )
        .into_response()
}

async fn delete_domain_binding(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath((host_id, session_id)): AxumPath<(String, String)>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let deleted = match guard.domain_binding_delete(&scope, &host_id, &session_id) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    if !deleted {
        return err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "会话绑定不存在",
        );
    }
    let (actor_kind, actor_id) = domain_actor(&state);
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &session_id,
        serde_json::json!({"op": "unbind", "host_id": host_id, "session_id": session_id, "actor": actor_id}),
    );
    (
        StatusCode::OK,
        Json(DomainMutationResponse {
            request_id: req_id.0,
            status: "ok",
            created: false,
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainGrantRequest {
    reader_domain: String,
    granted_domain: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, serde::Serialize)]
struct DomainGrantDto {
    id: String,
    reader_domain: String,
    granted_domain: String,
    granted_by: String,
    reason: String,
    created_at: String,
    revoked_at: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct DomainGrantsResponse {
    request_id: String,
    grants: Vec<DomainGrantDto>,
}

async fn list_domain_grants(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.domain_grant_list(&scope) {
        Ok(rows) => (
            StatusCode::OK,
            Json(DomainGrantsResponse {
                request_id: req_id.0,
                grants: rows
                    .into_iter()
                    .map(|g| DomainGrantDto {
                        id: g.id,
                        reader_domain: g.reader_domain,
                        granted_domain: g.granted_domain,
                        granted_by: g.granted_by,
                        reason: g.reason,
                        created_at: g.created_at,
                        revoked_at: g.revoked_at,
                    })
                    .collect(),
            }),
        )
            .into_response(),
        Err(e) => domain_err(&req_id.0, &e),
    }
}

async fn create_domain_grant(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DomainGrantRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let (actor_kind, actor_id) = domain_actor(&state);
    let (row, created) = match guard.domain_grant_add(
        &scope,
        &body.reader_domain,
        &body.granted_domain,
        actor_id,
        &body.reason,
    ) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &row.id.clone(),
        serde_json::json!({
            "op": "grant", "id": row.id, "reader_domain": row.reader_domain,
            "granted_domain": row.granted_domain, "created": created, "actor": actor_id
        }),
    );
    (
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(serde_json::json!({
            "request_id": req_id.0,
            "created": created,
            "grant": {
                "id": row.id, "reader_domain": row.reader_domain,
                "granted_domain": row.granted_domain, "granted_by": row.granted_by,
                "reason": row.reason, "created_at": row.created_at
            }
        })),
    )
        .into_response()
}

async fn delete_domain_grant(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(grant_id): AxumPath<String>,
) -> Response {
    if let Some(resp) = domains_guard(&state, &req_id.0) {
        return resp;
    }
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    let revoked = match guard.domain_grant_revoke(&scope, &grant_id) {
        Ok(v) => v,
        Err(e) => return domain_err(&req_id.0, &e),
    };
    if !revoked {
        return err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "生效中的授权不存在",
        );
    }
    let (actor_kind, actor_id) = domain_actor(&state);
    let _ = guard.domain_audit(
        &scope,
        actor_kind,
        actor_id,
        &grant_id,
        serde_json::json!({"op": "revoke", "id": grant_id, "actor": actor_id}),
    );
    (
        StatusCode::OK,
        Json(DomainMutationResponse {
            request_id: req_id.0,
            status: "ok",
            created: false,
        }),
    )
        .into_response()
}

/// CLI 的域上下文（doc7/06 §5）：可信运维路径显式给出域名，服务端核其存在且 active。
fn cli_domain_scope(
    store: &memory_store_sqlite::Store,
    scope: &ScopeKey,
    domain: &str,
) -> Result<memory_domain::DomainScope, String> {
    store
        .domain_require_active(scope, domain)
        .map_err(|e| format!("域不可用: {e}"))?;
    Ok(memory_domain::DomainScope::resolve(
        domain,
        domain != memory_domain::USER_MAIN_DOMAIN,
        &[],
        domain,
    ))
}

/// V2-D1 只读 Markdown 投影（doc7/06 §5）。
///
/// 先写临时目录再整体替换，避免半成品覆盖上一版；每行带稳定引用与来源；
/// manifest 入库（正文不入库），失败记 incomplete 而不是假装成功。
fn export_markdown_projection(
    store: &mut memory_store_sqlite::Store,
    scope: &ScopeKey,
    dom: &memory_domain::DomainScope,
    out: &std::path::Path,
) -> Result<usize, String> {
    let (entries, batch) = store
        .derived_export_entries(scope, dom)
        .map_err(|e| e.to_string())?;
    let base = out
        .join(&scope.tenant_id)
        .join(&scope.user_id)
        .join(&dom.write);
    let staging = base.with_extension(format!("staging-{}", uuid::Uuid::now_v7()));
    let mut written = 0usize;
    let mut outcome = "complete";
    let mut render = |rel: &str, body: String| -> Result<(), String> {
        let target = staging.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&target, body).map_err(|e| e.to_string())?;
        written += 1;
        Ok(())
    };

    let render_view = |store: &memory_store_sqlite::Store,
                       kind: &str,
                       dom: &memory_domain::DomainScope|
     -> Result<String, String> {
        let view = store
            .derived_view_readable(scope, dom, kind)
            .map_err(|e| e.to_string())?;
        let mut text = format!(
            "# {kind}\n\n<!-- batch_version={} items={} skipped_stale={} -->\n",
            view.batch_version,
            view.items.len(),
            view.skipped_stale
        );
        for item in &view.items {
            let refs: Vec<String> = item
                .sources
                .iter()
                .map(|(mid, v, _)| {
                    memory_domain::refs::memory_stable_ref(
                        &scope.tenant_id,
                        &scope.user_id,
                        &dom.write,
                        mid,
                        *v,
                    )
                })
                .collect();
            text.push_str(&format!("- {}  <sub>{}</sub>\n", item.body, refs.join(" ")));
        }
        Ok(text)
    };

    for entry in &entries {
        match render_view(store, &entry.document_kind, dom) {
            Ok(text) => render(&entry.path, text)?,
            Err(e) => {
                eprintln!("[memoryd] 导出 {entry:?} 失败: {e}");
                outcome = "incomplete";
            }
        }
    }
    // manifest.json：版本、条目数、生成时刻与 outcome。
    let manifest = serde_json::json!({
        "tenant_id": scope.tenant_id,
        "user_id": scope.user_id,
        "domain_id": dom.write,
        "batch_version": batch,
        "outcome": outcome,
        "generated_at": memory_store_sqlite::now_rfc3339_pub().map_err(|e| e.to_string())?,
        "files": entries,
        "note": "文件行号只是该导出版本的位置，不是主键；正文以数据库为准",
    });
    render(
        "manifest.json",
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )?;

    std::fs::create_dir_all(base.parent().unwrap_or(out)).map_err(|e| e.to_string())?;
    if base.exists() {
        std::fs::remove_dir_all(&base).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&staging, &base).map_err(|e| e.to_string())?;
    store
        .derived_write_manifest(scope, dom, batch, &entries, outcome)
        .map_err(|e| e.to_string())?;
    Ok(written)
}

fn err(req_id: &str, status: StatusCode, code: ErrorCode, msg: &str) -> Response {
    (status, Json(ErrorResponse::new(req_id, code, msg))).into_response()
}

// ---- V2-S1 记忆域请求上下文（doc7/04 §2）----

/// 请求级域上下文：域功能关闭时恒为 user_main，行为与 schema 14 一致。
/// 读域选择器来自请求头 `X-Riko-Memory-Domain`（缺省 user_main）；
/// 写域**不**取自请求头或正文，而是由服务端从可信会话绑定解析（§2.2）。
#[derive(Clone, Debug)]
struct DomainCtx {
    enabled: bool,
    selected: String,
}

impl DomainCtx {
    fn main_only() -> Self {
        DomainCtx {
            enabled: false,
            selected: memory_domain::USER_MAIN_DOMAIN.to_string(),
        }
    }
}

/// 域相关 StoreError → HTTP 语义（doc7/04 §2.3：非法/未知 404、closed 409）。
fn domain_err(req_id: &str, e: &memory_store_sqlite::StoreError) -> Response {
    use memory_store_sqlite::StoreError as E;
    match e {
        E::DomainNotFound => err(
            req_id,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆域不存在",
        ),
        E::DomainClosed => err(
            req_id,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "记忆域已关闭",
        ),
        E::DomainBindingConflict => err(
            req_id,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "会话已绑定到其他域",
        ),
        E::InvalidDomainId | E::DomainReserved => err(
            req_id,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "域名不合法",
        ),
        _ => err(
            req_id,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "域操作失败",
        ),
    }
}

/// 解析本次请求的 DomainScope。
///
/// - `enabled=false`：恒 `user_main`（与 14 版本一致，不写 evidence_domain_map）。
/// - `origin` 给出时写域来自可信会话绑定；否则写域 = 读域选择器。
/// - 读域集 = {D} ∪（D 为 side 时 user_main）∪ 已授权域 ∪ {写域}（§2.3、§3）。
fn domain_scope_for(
    state: &AppState,
    scope: &ScopeKey,
    ctx: &DomainCtx,
    req_id: &str,
    origin: Option<&Origin>,
) -> Result<memory_domain::DomainScope, Response> {
    if !ctx.enabled {
        return Ok(memory_domain::DomainScope::user_main());
    }
    if !ctx.selected.is_empty() && !ctx.selected.eq(memory_domain::USER_MAIN_DOMAIN) {
        // 选择器只表达「想读哪个域」，权限由 Rust 计算（§2.3）。
        memory_store_sqlite::domains::validate_domain_id(&ctx.selected)
            .map_err(|e| domain_err(req_id, &e))?;
    }
    let store = state.store.lock().map_err(|_| {
        err(
            req_id,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "存储不可用",
        )
    })?;
    let write = match origin {
        Some(o) => store
            .resolve_write_domain(scope, &o.host_id, &o.session_id)
            .map_err(|e| domain_err(req_id, &e))?,
        None => ctx.selected.clone(),
    };
    let selected_row = store
        .domain_require_active(scope, &ctx.selected)
        .map_err(|e| domain_err(req_id, &e))?;
    let grants = store
        .domain_grants_for(scope, &ctx.selected)
        .map_err(|e| domain_err(req_id, &e))?;
    Ok(memory_domain::DomainScope::resolve(
        &ctx.selected,
        selected_row.kind == "side",
        &grants,
        &write,
    ))
}

/// 取请求的域上下文；解析失败直接返回响应。
macro_rules! dom_or_return {
    ($state:expr, $scope:expr, $ctx:expr, $req_id:expr, $origin:expr) => {
        match domain_scope_for($state, $scope, $ctx, $req_id, $origin) {
            Ok(d) => d,
            Err(resp) => return resp,
        }
    };
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
                    return err(
                        &rid,
                        StatusCode::UNAUTHORIZED,
                        ErrorCode::Unauthenticated,
                        "令牌无效或用户已停用",
                    );
                }
            }
        }
        None => {
            let rid = req
                .extensions()
                .get::<RequestId>()
                .map(|r| r.0.clone())
                .unwrap_or_default();
            return err(
                &rid,
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthenticated,
                "缺少 Bearer 令牌",
            );
        }
    };
    req.extensions_mut().insert(scope.clone());

    // V2-S1（doc7/04 §2.3）：读域选择器只表达「想读哪个域」，权限由 Rust 计算。
    // 域功能关闭时不读请求头，行为与 schema 14 一致。
    let dom_ctx = if state.domains_enabled {
        let rid = req
            .extensions()
            .get::<RequestId>()
            .map(|r| r.0.clone())
            .unwrap_or_default();
        let selected = req
            .headers()
            .get("x-riko-memory-domain")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(memory_domain::USER_MAIN_DOMAIN)
            .to_string();
        if selected != memory_domain::USER_MAIN_DOMAIN {
            if let Err(e) = memory_store_sqlite::domains::validate_domain_id(&selected) {
                return domain_err(&rid, &e);
            }
        }
        // 非法/未知 → 404，已关闭 → 409；拒绝在进入业务前就发生。
        match state
            .store
            .lock()
            .map_err(|_| memory_store_sqlite::StoreError::StateConflict)
            .and_then(|store| store.domain_require_active(&scope, &selected))
        {
            Ok(_) => {}
            Err(e) => return domain_err(&rid, &e),
        }
        DomainCtx {
            enabled: true,
            selected,
        }
    } else {
        DomainCtx::main_only()
    };
    req.extensions_mut().insert(dom_ctx);

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
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<IngestRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        // JSON 语法错误 → INVALID_JSON；反序列化失败（含未知字段/类型错）→ INVALID_FIELD（doc/12 §1/§8）。
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_code, msg)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                msg,
            )
        }
    };
    if body.event_seq < 0 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "event_seq 必须非负",
        );
    }
    if !matches!(body.role.as_str(), "user" | "assistant" | "tool" | "system") {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "role 必须是 user/assistant/tool/system",
        );
    }
    if !matches!(
        body.source_kind.as_str(),
        "user" | "assistant" | "tool" | "plugin" | "system"
    ) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "source_kind 必须是 user/assistant/tool/plugin/system",
        );
    }
    let occurred_at = match chrono::DateTime::parse_from_rfc3339(&body.occurred_at) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "occurred_at 必须是带时区的 RFC3339 时间",
            )
        }
    };
    if body.content.is_empty() || body.content.len() > EVIDENCE_CONTENT_MAX_BYTES {
        return err(
            &req_id.0,
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::BodyTooLarge,
            "content 必须 1～64 KiB",
        );
    }

    let outcome = {
        let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
        let mut guard = state.store.lock().unwrap();
        guard.record_evidence(
            &scope,
            &origin,
            body.event_seq,
            &body.role,
            &body.source_kind,
            &occurred_at,
            &body.content,
            &dom,
        )
    };
    match outcome {
        Ok(IngestOutcome::Recorded(id)) => (
            StatusCode::CREATED,
            Json(IngestResponse {
                request_id: req_id.0,
                status: "recorded",
                evidence_id: id,
            }),
        )
            .into_response(),
        Ok(IngestOutcome::AlreadyRecorded(id)) => (
            StatusCode::OK,
            Json(IngestResponse {
                request_id: req_id.0,
                status: "already_recorded",
                evidence_id: id,
            }),
        )
            .into_response(),
        Err(StoreError::EventConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::EventConflict,
            "事件键已存在且内容哈希不同",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<RememberRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_, msg)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                msg,
            )
        }
    };
    let kind = match body.kind.as_str() {
        "fact" => MemoryKind::Fact,
        "preference" => MemoryKind::Preference,
        "instruction" => MemoryKind::Instruction,
        "episode" => MemoryKind::Episode,
        _ => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "kind 必须是 fact/preference/instruction/episode",
            )
        }
    };
    let quote_chars = body.quote.chars().count();
    if quote_chars == 0 || quote_chars > 512 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "quote 必须 1～512 个 Unicode 标量字符",
        );
    }
    let outcome = {
        let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
        let mut guard = state.store.lock().unwrap();
        guard.remember(
            &scope,
            &origin,
            &body.user_evidence_id,
            &body.quote,
            kind,
            &dom,
        )
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
        Err(StoreError::QuoteMismatch) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::QuoteMismatch,
            "quote 不是该用户消息的连续原文子串",
        ),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StaleUserEvidence,
            "引用的用户证据不是该会话最新用户事件或角色不符",
        ),
        // 直写内容护栏已按用户产品决定（2026-09-25 深夜）全部解除；直写路径不再有
        // 内容类别类 409。quote/证据类错误（QUOTE_MISMATCH、STALE_USER_EVIDENCE）
        // 沿既有映射。
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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

// ---- V2-P1 精读（doc7/05 §3、§4）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplainQuery {
    /// \`history=1|true\` 才返回 superseded/expired 链；缺省 active-only。
    #[serde(default)]
    history: Option<String>,
}

/// \`GET /v1/memories/{memory_id}/explain\`。\`memory_id\` 既可以是裸 ID，也可以是
/// URL 编码后的 \`riko://memory/...\` 稳定引用；引用里的 tenant/user 必须与认证 scope
/// 完全一致，domain 必须在本次读域集内，否则按不存在处理（不得跨 scope / 跨域）。
// ---- V2-D1 蒸馏视图端点（doc7/06 §5）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FacetsQuery {
    #[serde(default)]
    kind: Option<String>,
}

// ---- V2-R1 关系图谱端点（doc7/07 §5）----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelationshipsQuery {
    #[serde(default)]
    expected_version: Option<i64>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn entity_entry_json(
    e: &memory_store_sqlite::relationships::EntityIndexEntry,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id": e.entity_id,
        "entity_kind": e.entity_kind,
        "display_name": e.display_name,
        "relation": e.relation,
        "aliases": e.aliases,
        "summary": e.summary,
        "version": e.version,
        "rank_source": e.rank_source,
        "closeness_rank": e.closeness_rank,
        "updated_at": e.updated_at,
        "detail_ref": e.detail_ref,
    })
}

async fn list_relationships(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<RelationshipsQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let limit = query
        .limit
        .unwrap_or(memory_store_sqlite::relationships::INDEX_MAX_ENTITIES)
        .min(200);
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.relationship_index(&scope, &dom, limit) {
        Ok(index) => Json(serde_json::json!({
            "request_id": req_id.0,
            "domain_id": index.domain_id,
            "batch_version": index.batch_version,
            "total": index.total,
            "omitted": index.omitted,
            "entities": index.entities.iter().map(entity_entry_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("读取关系索引失败: {e}"),
        ),
    }
}

async fn get_relationship(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(entity_id): AxumPath<String>,
    axum::extract::Query(query): axum::extract::Query<RelationshipsQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.relationship_get(&scope, &dom, &entity_id, query.expected_version) {
        Ok(Some(detail)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "entity": entity_entry_json(&detail.entity),
            "skipped_stale": detail.skipped_stale,
            "items": detail.items.iter().map(|i| serde_json::json!({
                "item_id": i.item_id,
                "section": i.section,
                "body": i.body,
                "observed_or_inferred": i.observed_or_inferred,
                "version": i.version,
                "sources": i.sources.iter().map(|(mid, v, sha)| serde_json::json!({
                    "memory_id": mid,
                    "memory_version": v,
                    "claim_sha256": sha,
                    "stable_ref": memory_domain::refs::memory_stable_ref(
                        &scope.tenant_id, &scope.user_id, &dom.write, mid, *v),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "实体不存在或不可见",
        ),
        Err(memory_store_sqlite::StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "实体版本已变化，请重新读取",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("读取实体失败: {e}"),
        ),
    }
}

async fn resolve_relationship(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<RelationshipsQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) else {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "q 不能为空",
        );
    };
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.relationship_resolve(&scope, &dom, q) {
        Ok(memory_store_sqlite::relationships::Resolution::None) => Json(serde_json::json!({
            "request_id": req_id.0,
            "query": q,
            "resolution": "none",
            "candidates": [],
        }))
        .into_response(),
        Ok(memory_store_sqlite::relationships::Resolution::One(e)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "query": q,
            "resolution": "one",
            "candidates": [entity_entry_json(&e)],
        }))
        .into_response(),
        Ok(memory_store_sqlite::relationships::Resolution::Ambiguous(list)) => {
            Json(serde_json::json!({
                "request_id": req_id.0,
                "query": q,
                "resolution": "ambiguous",
                "candidates": list.iter().map(entity_entry_json).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("解析失败: {e}"),
        ),
    }
}

async fn refresh_relationships(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.relationship_refresh(&scope, &dom) {
        Ok(out) => Json(serde_json::json!({
            "request_id": req_id.0,
            "status": "ok",
            "batch_version": out.batch_version,
            "entities": out.entities,
            "items": out.items,
            "previous_entities": out.previous_entities,
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("重建关系图谱失败: {e}"),
        ),
    }
}

async fn get_compact(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.compact_view(&scope, &dom) {
        Ok(view) => Json(derived_view_response(&req_id.0, &scope, &dom, &view)).into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("读取 compact 失败: {e}"),
        ),
    }
}

fn derived_view_response(
    req_id: &str,
    scope: &ScopeKey,
    dom: &memory_domain::DomainScope,
    view: &memory_store_sqlite::derived::DerivedView,
) -> serde_json::Value {
    let _ = req_id;
    serde_json::json!({
        "request_id": req_id,
        "document_kind": view.document_kind,
        "batch_version": view.batch_version,
        "item_count": view.items.len(),
        "skipped_stale": view.skipped_stale,
        "total_chars": view.total_chars,
        "budget_items": view.budget_items,
        "budget_chars": view.budget_chars,
        "items": view.items.iter().map(|i| serde_json::json!({
            "item_id": i.id,
            "position": i.position,
            "body": i.body,
            "observed_or_inferred": i.observed_or_inferred,
            "generator_version": i.generator_version,
            "source_fingerprint": i.source_fingerprint,
            "sources": i.sources.iter().map(|(mid, v, sha)| serde_json::json!({
                "memory_id": mid,
                "memory_version": v,
                "claim_sha256": sha,
                "stable_ref": memory_domain::refs::memory_stable_ref(
                    &scope.tenant_id, &scope.user_id, &dom.write, mid, *v),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

async fn get_facets(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<FacetsQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match query.kind.as_deref() {
        Some(kind) => {
            // 先归一化再校验：裸名与带 facet_ 前缀都接受（doc7/06 §5）。
            let full = if kind.starts_with("facet_") {
                kind.to_string()
            } else {
                format!("facet_{kind}")
            };
            if !memory_store_sqlite::derived::is_facet(&full) {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "kind 必须是 experience|opinions|reflections|world（可带 facet_ 前缀）",
                );
            }
            match guard.facet_view(&scope, &dom, &full) {
                Ok(view) => {
                    Json(derived_view_response(&req_id.0, &scope, &dom, &view)).into_response()
                }
                Err(e) => err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &format!("读取分面失败: {e}"),
                ),
            }
        }
        None => {
            let mut facets = serde_json::Map::new();
            for facet in memory_store_sqlite::derived::FACETS {
                match guard.facet_view(&scope, &dom, facet) {
                    Ok(view) => {
                        facets.insert(
                            facet.to_string(),
                            derived_view_response(&req_id.0, &scope, &dom, &view),
                        );
                    }
                    Err(e) => {
                        return err(
                            &req_id.0,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            ErrorCode::Internal,
                            &format!("读取分面失败: {e}"),
                        )
                    }
                }
            }
            Json(serde_json::json!({
                "request_id": req_id.0,
                "facets": facets,
            }))
            .into_response()
        }
    }
}

async fn refresh_derived(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let mut guard = match state.store.lock() {
        Ok(g) => g,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "存储不可用",
            )
        }
    };
    match guard.derived_refresh(&scope, &dom) {
        Ok(out) => Json(serde_json::json!({
            "request_id": req_id.0,
            "status": "ok",
            "batch_version": out.batch_version,
            "compact_items": out.compact_items,
            "facet_items": out.facet_items,
            "previous_items": out.stale_removed,
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("重建派生视图失败: {e}"),
        ),
    }
}

async fn explain_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
    axum::extract::Query(query): axum::extract::Query<ExplainQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let include_history = matches!(
        query.history.as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes")
    );

    let lookup_id = if memory_id.starts_with(memory_domain::refs::MEMORY_REF_PREFIX) {
        let Some(parsed) = memory_domain::refs::parse_memory_ref(&memory_id) else {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "稳定引用格式不合法",
            );
        };
        // 引用不是凭据：解析出的身份必须与令牌解析出的 scope 一致。
        if parsed.tenant_id != scope.tenant_id || parsed.user_id != scope.user_id {
            return err(
                &req_id.0,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "记忆不存在或不可见",
            );
        }
        if !dom.allows_read(&parsed.domain_id) {
            return err(
                &req_id.0,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "记忆不存在或不可见",
            );
        }
        parsed.memory_id
    } else {
        memory_id
    };

    let result =
        state
            .store
            .lock()
            .unwrap()
            .memory_explain(&scope, &lookup_id, &dom, include_history);
    match result {
        Ok(Some(ex)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": ex.memory_id,
            "kind": ex.kind,
            "claim": ex.claim,
            "status": ex.status,
            "version": ex.version,
            "domain_id": ex.domain_id,
            "source_class": ex.source_class,
            "occurred_at": ex.occurred_at,
            "valid_from": ex.valid_from,
            "valid_until": ex.valid_until,
            "created_at": ex.created_at,
            "updated_at": ex.updated_at,
            "speaker": ex.speaker,
            "subject": ex.subject,
            "subject_source": ex.subject_source,
            "reason_code": ex.reason_code,
            "stable_ref": ex.stable_ref,
            "visible": ex.visible,
            "relations": {
                "superseded_by": ex.relations.superseded_by,
                "supersedes": ex.relations.supersedes,
                "retired": ex.relations.retired,
                "retirement_reason_code": ex.relations.retirement_reason_code,
            },
            "evidence": ex.evidence.iter().map(|e| serde_json::json!({
                "evidence_id": e.evidence_id,
                "start_byte": e.start_byte,
                "end_byte": e.end_byte,
                "quote": e.quote,
                "span_exact": e.span_exact,
                "role": e.role,
                "source_kind": e.source_kind,
                "occurred_at": e.occurred_at,
                "host_id": e.host_id,
                "session_id": e.session_id,
                "suppressed": e.suppressed,
                "tombstoned": e.tombstoned,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或不可见",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &format!("精读失败: {e}"),
        ),
    }
}

async fn get_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let row = state
        .store
        .lock()
        .unwrap()
        .get_memory(&scope, &memory_id, /*DOM*/ &dom);
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
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或不可见",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<SearchRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let q_chars = body.query.chars().count();
    if q_chars == 0 || q_chars > SEARCH_QUERY_MAX_CHARS {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "query 必须 1～2048 个 Unicode 标量字符",
        );
    }
    let limit = body.limit.unwrap_or(5);
    if !(1..=20).contains(&limit) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～20",
        );
    }
    let include_history = body.include_history.unwrap_or(false);
    if include_history && !memory_store_sqlite::has_history_cue(&body.query) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "include_history=true 要求 query 含明确历史词（以前/过去/曾经/当时 等）",
        );
    }
    let result = state.store.lock().unwrap().search_memories(
        &scope,
        &body.query,
        limit,
        include_history,
        /*DOM*/ &dom,
    );
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    /// doc7（Riko-Muse）M3：true 时响应附带 alignment synthesis（缺省 false，
    /// 缺省时响应与旧版逐字节一致——v1 契约不变）。
    #[serde(default)]
    include_alignment: bool,
}

async fn compose_context(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<ComposeRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let max_items = body.max_items.unwrap_or(COMPOSE_MAX_ITEMS_DEFAULT);
    if !(1..=5).contains(&max_items) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "max_items 必须 1～5",
        );
    }
    let max_chars = body.max_chars.unwrap_or(COMPOSE_MAX_CHARS_DEFAULT);
    if !(100..=2000).contains(&max_chars) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "max_chars 必须 100～2000",
        );
    }
    let result = state.store.lock().unwrap().compose_context(
        &scope,
        &body.agent_id,
        &body.query,
        max_items,
        max_chars,
        /*DOM*/ &dom,
    );
    match result {
        Ok(ComposeResult {
            text,
            items,
            truncated,
            index_degraded,
        }) => {
            let item_objs: Vec<serde_json::Value> = items
                .into_iter()
                .map(|(memory_id, evidence_ids)| serde_json::json!({"memory_id": memory_id, "evidence_ids": evidence_ids}))
                .collect();
            // doc7（Riko-Muse）M3：opt-in 注入 alignment synthesis。缺省 false 时不取
            // 不附（响应结构与旧版一致）；取最新版本，若存在则在 text 头部前置
            // <alignment_synthesis> 块并附 "alignment" 对象。
            let mut alignment_json: Option<serde_json::Value> = None;
            let mut final_text = text;
            if body.include_alignment {
                let synthesis = {
                    let guard = state.store.lock().unwrap();
                    guard.alignment_synthesis_latest(&scope, &dom)
                };
                match synthesis {
                    Ok(Some(row)) => {
                        final_text = format!(
                            "<alignment_synthesis version=\"{}\">\n{}\n</alignment_synthesis>\n{}",
                            row.version, row.body, final_text
                        );
                        alignment_json = Some(serde_json::json!({
                            "version": row.version,
                            "window_since": row.window_since,
                            "window_until": row.window_until,
                            "rupture_turns": row.rupture_turns,
                            "user_turns": row.user_turns,
                            "correction_free_rate": row.correction_free_rate,
                            "open_repair_threads": row.open_repair_threads,
                            "body": row.body,
                            "generated_at": row.generated_at,
                        }));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return err(
                            &req_id.0,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            ErrorCode::Internal,
                            &e.to_string(),
                        )
                    }
                }
            }
            let mut payload = serde_json::json!({
                "request_id": req_id.0,
                "text": final_text,
                "items": item_objs,
                "truncated": truncated,
                "index_degraded": index_degraded
            });
            if let Some(alignment) = alignment_json {
                payload["alignment"] = alignment;
            }
            return Json(payload).into_response();
        }
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

// ---- doc7（Riko-Muse）：alignment synthesis / repair 线程 / rupture 诊断 ----

async fn get_alignment_synthesis(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let guard = state.store.lock().unwrap();
    match guard.alignment_synthesis_latest(&scope, &dom) {
        Ok(Some(row)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "synthesis": alignment_json(&row),
        }))
        .into_response(),
        Ok(None) => Json(serde_json::json!({
            "request_id": req_id.0,
            "synthesis": serde_json::Value::Null,
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn refresh_alignment_synthesis(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let mut guard = state.store.lock().unwrap();
    match guard.alignment_synthesis_refresh(&scope, &dom) {
        Ok(row) => Json(serde_json::json!({
            "request_id": req_id.0,
            "synthesis": alignment_json(&row),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

fn alignment_json(
    row: &memory_store_sqlite::alignment::AlignmentSynthesisRow,
) -> serde_json::Value {
    serde_json::json!({
        "version": row.version,
        "window_since": row.window_since,
        "window_until": row.window_until,
        "rupture_turns": row.rupture_turns,
        "user_turns": row.user_turns,
        "correction_free_rate": row.correction_free_rate,
        "open_repair_threads": row.open_repair_threads,
        "body": row.body,
        "source_refs": serde_json::from_str::<serde_json::Value>(&row.source_refs_json)
            .unwrap_or(serde_json::Value::Null),
        "generated_at": row.generated_at,
    })
}

#[derive(Debug, Deserialize)]
struct RepairThreadsListQuery {
    status: Option<String>,
    limit: Option<usize>,
}

async fn list_repair_threads(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<RepairThreadsListQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    if let Some(s) = query.status.as_deref() {
        if !matches!(s, "open" | "closed") {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "status 只能是 open/closed",
            );
        }
    }
    let limit = query.limit.unwrap_or(50);
    let guard = state.store.lock().unwrap();
    match guard.repair_threads_list(&scope, query.status.as_deref(), limit, &dom) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "threads": rows.iter().map(|r| serde_json::json!({
                "id": r.id,
                "title": r.title,
                "status": r.status,
                "rupture_count": r.rupture_count,
                "first_rupture_at": r.first_rupture_at,
                "last_rupture_at": r.last_rupture_at,
                "closed_at": r.closed_at,
                "close_reason": r.close_reason,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairThreadCloseRequest {
    reason: String,
}

async fn close_repair_thread(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Path(thread_id): axum::extract::Path<String>,
    body: Result<Json<RepairThreadCloseRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "须为 {\"reason\": …} 的合法 JSON",
            )
        }
    };
    if body.reason.is_empty() || body.reason.chars().count() > 200 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "reason 须 1—200 字符",
        );
    }
    let mut guard = state.store.lock().unwrap();
    match guard.repair_thread_close(&scope, &thread_id, &body.reason, "api", &dom) {
        Ok(changed) => Json(serde_json::json!({
            "request_id": req_id.0,
            "changed": changed,
        }))
        .into_response(),
        Err(memory_store_sqlite::StoreError::ThreadNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "修复线程不存在或不属于当前 scope",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct RupturesListQuery {
    limit: Option<usize>,
}

async fn list_ruptures(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<RupturesListQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let limit = query.limit.unwrap_or(50);
    let guard = state.store.lock().unwrap();
    match guard.ruptures_list(&scope, limit, &dom) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "ruptures": rows.iter().map(|r| serde_json::json!({
                "id": r.id,
                "evidence_id": r.evidence_id,
                "host_id": r.host_id,
                "session_id": r.session_id,
                "event_seq": r.event_seq,
                "signal": r.signal,
                "cue": r.cue,
                "start_byte": r.start_byte,
                "end_byte": r.end_byte,
                "thread_id": r.thread_id,
                "detected_at": r.detected_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<FlushRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if body.host_id.is_empty() || body.session_id.is_empty() || body.through_event_seq < 0 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "host_id/session_id 非空且 through_event_seq 非负",
        );
    }
    let outcome = {
        let dom = dom_or_return!(
            &state,
            &scope,
            &dom_ctx,
            &req_id.0,
            Some(&Origin {
                host_id: body.host_id.clone(),
                agent_id: String::new(),
                session_id: body.session_id.clone()
            })
        );
        let mut guard = state.store.lock().unwrap();
        guard.flush_window(
            &scope,
            &body.host_id,
            &body.session_id,
            body.through_event_seq,
            &dom,
        )
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
        Err(StoreError::StateConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "through_event_seq 越界或乱序 flush",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            msg,
        );
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if let Err(msg) = validate_agent_id(&req.agent_id) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            msg,
        );
    }
    if req.expected_version < 0 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "expected_version 不能为负",
        );
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
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
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
                memory_store_sqlite::soul::SoulUpsertOutcome::Created { .. } => {
                    ("created", StatusCode::CREATED)
                }
                memory_store_sqlite::soul::SoulUpsertOutcome::Updated { .. } => {
                    ("updated", StatusCode::OK)
                }
                memory_store_sqlite::soul::SoulUpsertOutcome::Unchanged { .. } => {
                    ("unchanged", StatusCode::OK)
                }
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
        Err(StoreError::InvalidAgentId) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "agent_id 须为 1—256 字符",
        ),
        Err(StoreError::InvalidIdempotencyKey) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "idempotency_key 须为 1—128 个 ASCII [A-Za-z0-9._-]",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn list_soul_revisions(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    axum::extract::Query(query): axum::extract::Query<SoulRevisionsQuery>,
) -> Response {
    if let Err(msg) = validate_agent_id(&query.agent_id) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            msg,
        );
    }
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～200",
        );
    }
    let guard = state.store.lock().unwrap();
    match guard.list_soul_revisions(&scope, &query.agent_id) {
        Ok(all) => {
            let page: Vec<_> = all
                .into_iter()
                .filter(|r| {
                    query
                        .after_version
                        .map(|after| r.version > after)
                        .unwrap_or(true)
                })
                .take(limit)
                .collect();
            let next_cursor = if page.len() == limit {
                page.last().map(|r| r.version.to_string())
            } else {
                None
            };
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if req.memory_id.is_empty() {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "memory_id 不能为空",
        );
    }
    // 规范化哈希：None 用空串占位，字段集固定（doc6/06 §2）。
    let hash = memory_store_sqlite::soul::receipt_hash(&[
        &req.memory_id,
        &req.position.map(|p| p.to_string()).unwrap_or_default(),
        &req.expected_pin_version
            .map(|v| v.to_string())
            .unwrap_or_default(),
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
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
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
                    let code = if *version == 1 {
                        StatusCode::CREATED
                    } else {
                        StatusCode::OK
                    };
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        &expected_pin_version
            .map(|v| v.to_string())
            .unwrap_or_default(),
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
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
        }
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.resident_unpin(
            &scope,
            &memory_id,
            expected_pin_version,
            Some((idem_key, &hash)),
        )
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        0.5f64
            .powf(age_days / memory_contract::RECENCY_HALFLIFE_DAYS)
            .clamp(0.4, 1.0)
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
        assert_eq!(
            recency_factor("none", Some("2026-09-26T00:00:00Z"), NOW, NOW),
            1.0
        );
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
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<ResidentQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let limit = match budget_check(
        &req_id.0,
        "limit",
        query.limit,
        RESIDENT_MAX_ITEMS_DEFAULT,
        RESIDENT_MAX_ITEMS_MAX,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    let guard = state.store.lock().unwrap();
    match guard.select_resident(&scope, &now, limit, RESIDENT_MAX_CHARS_MAX, &dom) {
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～100",
        );
    }
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn post_context_bundle(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<BundleRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if let Err(msg) = validate_agent_id(&req.agent_id) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            msg,
        );
    }
    let r_items = match budget_check(
        &req_id.0,
        "resident_max_items",
        req.resident_max_items,
        RESIDENT_MAX_ITEMS_DEFAULT,
        RESIDENT_MAX_ITEMS_MAX,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let r_chars = match budget_check(
        &req_id.0,
        "resident_max_chars",
        req.resident_max_chars,
        RESIDENT_MAX_CHARS_DEFAULT,
        RESIDENT_MAX_CHARS_MAX,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let q_items = match budget_check(
        &req_id.0,
        "retrieved_max_items",
        req.retrieved_max_items,
        RETRIEVED_MAX_ITEMS_DEFAULT,
        RETRIEVED_MAX_ITEMS_MAX,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let q_chars = match budget_check(
        &req_id.0,
        "retrieved_max_chars",
        req.retrieved_max_chars,
        RETRIEVED_MAX_CHARS_DEFAULT,
        RETRIEVED_MAX_CHARS_MAX,
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    let query_trim = req.query.trim();
    // resident 段（查询无关，每轮重算；doc6/03 §3）。
    let selection = {
        let guard = state.store.lock().unwrap();
        guard.select_resident(&scope, &now, r_items, r_chars, &dom)
    };
    let selection = match selection {
        Ok(s) => s,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    // retrieved 段（doc6/04 §2/§3，D6-8）：各通道独立取候选与排名 → 单次全局 RRF
    // → 可选 cross-encoder 精排 → 对象复核（active/published/有效期）→ 排除
    // resident → 页/原子覆盖去重 → episode recency → 预算装配。分数仅用于排序。
    let mut lexical_status = "empty_query";
    let mut semantic_status = if state.embedding.is_some() {
        "ok"
    } else {
        "disabled"
    };
    let mut rerank_status = if state.rerank.is_some() {
        "ok"
    } else {
        "disabled"
    };
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
            guard.search_memories(&scope, query_trim, lane_k, false, &dom)
        };
        let (mem_lex, index_degraded) = match search_res {
            Ok(v) => v,
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
        };
        lexical_status = if index_degraded {
            "index_degraded"
        } else {
            "ok"
        };
        // ---- 通道 2：词法页面（published；读时复核在装配段统一做）。----
        let page_lex: Vec<String> = {
            let guard = state.store.lock().unwrap();
            guard
                .page_fts_search(&scope, query_trim, lane_k, &dom)
                .unwrap_or_default()
        };
        page_index_status = "ok";
        // ---- 通道 3/4：向量（embedding 已配置才启用；等待 provider 按配置超时或
        // 调用方取消，不另设查询延迟预算）。ready 总数达上限 → limit_exceeded 只做词法。----
        let mut mem_vec: Vec<String> = Vec::new();
        let mut page_vec: Vec<String> = Vec::new();
        if let Some(emb) = &state.embedding {
            let qtexts = vec![query_trim.to_string()];
            match emb.embed(&qtexts).await {
                Ok(vs) if !vs.is_empty() => {
                    let qv = &vs[0];
                    let guard = state.store.lock().unwrap();
                    let scan_m = guard.semantic_scan_with_floor(
                        &scope,
                        "memory",
                        emb.model_id(),
                        qv,
                        lane_k,
                        state.semantic_min_similarity,
                        &dom,
                    );
                    let scan_p = guard.semantic_scan_with_floor(
                        &scope,
                        "page",
                        emb.model_id(),
                        qv,
                        lane_k,
                        state.semantic_min_similarity,
                        &dom,
                    );
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
                Ok(_) => {
                    semantic_status = "unavailable";
                } // 空向量
                Err(e) => {
                    semantic_status = "unavailable";
                    eprintln!("[bundle] query embedding 失败: {e}");
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
        let mut mem_info: std::collections::HashMap<
            String,
            memory_store_sqlite::memories::MemoryRow,
        > = std::collections::HashMap::new();
        {
            let guard = state.store.lock().unwrap();
            for mid in &mem_ids {
                if let Ok(Some(m)) = guard.get_memory(&scope, mid, &dom) {
                    mem_info.insert(mid.clone(), m);
                }
            }
        }
        let mut page_info: std::collections::HashMap<String, memory_store_sqlite::pages::PageRow> =
            std::collections::HashMap::new();
        {
            let guard = state.store.lock().unwrap();
            for pid in &page_ids {
                if let Ok(Some(p)) = guard.get_page(&scope, pid, &now, &dom) {
                    page_info.insert(pid.clone(), p);
                }
            }
        }
        // ---- 单次全局 RRF（doc6/04 §2：禁止向量支路内部再 RRF）。----
        let mut channels: Vec<(&str, Vec<String>)> = Vec::new();
        channels.push((
            "lexical",
            mem_lex
                .iter()
                .filter(|h| mem_info.contains_key(&h.memory_id))
                .map(|h| bundle_channel_key("m", &h.memory_id))
                .collect(),
        ));
        channels.push((
            "page",
            page_lex
                .iter()
                .filter(|p| page_info.contains_key(*p))
                .map(|p| bundle_channel_key("p", p))
                .collect(),
        ));
        if semantic_status == "ok" {
            channels.push((
                "semantic",
                mem_vec
                    .iter()
                    .filter(|m| mem_info.contains_key(*m))
                    .map(|m| bundle_channel_key("m", m))
                    .collect(),
            ));
            channels.push((
                "semantic",
                page_vec
                    .iter()
                    .filter(|p| page_info.contains_key(*p))
                    .map(|p| bundle_channel_key("p", p))
                    .collect(),
            ));
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
                        Some(("m", id)) => mem_info
                            .get(id)
                            .map(|m| m.claim.clone())
                            .unwrap_or_default(),
                        Some(("p", id)) => page_info
                            .get(id)
                            .map(|p| p.title.clone())
                            .unwrap_or_default(),
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
        let channel_of =
            |k: &str, rrf: &std::collections::HashMap<String, (Vec<u32>, Vec<&str>)>| -> String {
                rrf.get(k)
                    .map(|(_, names)| names.join("+"))
                    .unwrap_or_else(|| "fused".into())
            };
        for (key, _score) in &scored {
            match key.split_once(':') {
                Some(("m", mid)) => {
                    if covered.contains(mid) {
                        continue; // resident 已含/冲突 ID 或已被页面覆盖。
                    }
                    let Some(m) = mem_info.get(mid) else { continue };
                    let refs: Vec<String> =
                        m.evidence_refs.iter().map(|(e, _, _)| e.clone()).collect();
                    let entry = format!("- [memory: {}] {}", m.memory_id, m.claim);
                    let entry_chars = entry.chars().count();
                    if retrieved_items.len() >= q_items
                        || (used_chars_retrieved > 0
                            && used_chars_retrieved + entry_chars + 1 > q_chars)
                    {
                        retrieved_omitted.push(serde_json::json!({
                            "memory_id": mid,
                            "reason": if retrieved_items.len() >= q_items { "ITEM_LIMIT" } else { "CHAR_LIMIT" },
                        }));
                        retrieved_truncated = true;
                        continue;
                    }
                    used_chars_retrieved +=
                        entry_chars + if retrieved_items.is_empty() { 0 } else { 1 };
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
                    let Some(p) = page_info.get(pid) else {
                        continue;
                    }; // 来源失效 → 不可见
                    let source_ids: Vec<String> =
                        p.sources.iter().map(|(id, _)| id.clone()).collect();
                    let uncovered = source_ids.iter().filter(|s| !covered.contains(*s)).count();
                    let min_needed = if p.document_kind == "mental_model" {
                        1
                    } else {
                        2
                    };
                    if uncovered < min_needed {
                        continue; // doc6/04 §3：未覆盖来源不足 → 跳过文档。
                    }
                    let entry = format!(
                        "- [page: {}] {}{}",
                        p.page_id,
                        p.title,
                        if p.description.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", p.description)
                        }
                    );
                    let entry_chars = entry.chars().count();
                    if retrieved_items.len() >= q_items
                        || (used_chars_retrieved > 0
                            && used_chars_retrieved + entry_chars + 1 > q_chars)
                    {
                        retrieved_omitted
                            .push(serde_json::json!({"page_id": pid, "reason": "ITEM_LIMIT"}));
                        retrieved_truncated = true;
                        continue;
                    }
                    used_chars_retrieved +=
                        entry_chars + if retrieved_items.is_empty() { 0 } else { 1 };
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
                        "description": p.description,
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

fn bundle_channel_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

#[cfg(test)]
mod bundle_channel_tests {
    use super::bundle_channel_key;

    #[test]
    fn memory_and_page_lanes_use_distinct_namespaced_keys() {
        assert_eq!(bundle_channel_key("m", "same-id"), "m:same-id");
        assert_eq!(bundle_channel_key("p", "same-id"), "p:same-id");
    }
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
        "description": p.description,
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
    Extension(dom_ctx): Extension<DomainCtx>,
    axum::extract::Query(query): axum::extract::Query<PagesListQuery>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～100",
        );
    }
    let statuses: Vec<&str> = match query.status.as_deref() {
        None => vec![],
        Some(s) => s.split(',').map(str::trim).collect(),
    };
    for s in &statuses {
        if !matches!(*s, "published" | "stale" | "archived") {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "status 只能是 published/stale/archived",
            );
        }
    }
    let guard = state.store.lock().unwrap();
    match guard.page_list(&scope, &statuses, limit, &dom) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "pages": rows.iter().map(page_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn get_page(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(page_id): AxumPath<String>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(t) => t,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    let guard = state.store.lock().unwrap();
    match guard.get_page(&scope, &page_id, &now, &dom) {
        Ok(Some(p)) => Json(serde_json::json!({
            "request_id": req_id.0,
            "page": page_json(&p),
        }))
        .into_response(),
        // 跨 scope/失效/不存在一律 404（不泄露存在性；doc6/06 §2）。
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "页面不存在或已失效",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "status 只能是 active/archived",
            );
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if req.page_id.is_empty() {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "page_id 不能为空",
        );
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<DreamTriggerRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    if req.trigger_key.is_empty() || req.trigger_key.chars().count() > 128 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "trigger_key 须 1—128 字符",
        );
    }
    // 只入队不等待完成（doc6/10 §4：触发器只负责入队）。
    let result = {
        let dom = dom_or_return!(
            &state,
            &scope,
            &dom_ctx,
            &req_id.0,
            Some(&Origin {
                host_id: req.host_id.clone().unwrap_or_default(),
                agent_id: req.agent_id.clone().unwrap_or_default(),
                session_id: req.session_id.clone().unwrap_or_default()
            })
        );
        let mut guard = state.store.lock().unwrap();
        guard.dream_trigger(
            &scope,
            &req.trigger_kind,
            &req.trigger_key,
            req.agent_id.as_deref(),
            req.host_id.as_deref(),
            req.session_id.as_deref(),
            &dom,
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
        Err(StoreError::StateConflict) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "trigger_kind 非法",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamRedecisionRequest {
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamRunnerHeartbeatRequest {
    runner_id: String,
    host_id: String,
    agent_id: String,
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamRunnerClaimRequest {
    runner_id: String,
}

async fn dream_runner_heartbeat(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DreamRunnerHeartbeatRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "runner heartbeat 请求字段非法",
            )
        }
    };
    if req.capabilities.len() > 16
        || req.capabilities.iter().any(|c| {
            !matches!(
                c.as_str(),
                "chat" | "dream_v1" | "adjudicate_v1" | "consolidate_v1" | "dream_scoped_read_v1"
            )
        })
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "runner capabilities 非法",
        );
    }
    let capabilities = match serde_json::to_string(&req.capabilities) {
        Ok(v) => v,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "runner capabilities 非法",
            )
        }
    };
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.dream_runner_heartbeat(
            &scope,
            &req.runner_id,
            &req.host_id,
            &req.agent_id,
            &capabilities,
            45,
        )
    };
    match result {
        Ok(()) => Json(serde_json::json!({"request_id": req_id.0, "status": "runner_live", "lease_seconds": 45})).into_response(),
        Err(StoreError::InvalidPageField) => err(&req_id.0, StatusCode::BAD_REQUEST, ErrorCode::InvalidField, "runner id/host/agent/capability 非法"),
        Err(e) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

async fn dream_runner_claim(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    body: Result<Json<DreamRunnerClaimRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "runner claim 请求字段非法",
            )
        }
    };
    if state.embedding.is_none() {
        return Json(serde_json::json!({"request_id": req_id.0, "status": "missing_embedding"}))
            .into_response();
    }
    let now = match memory_store_sqlite::now_rfc3339_pub() {
        Ok(value) => value,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    let has_chat = {
        let guard = state.store.lock().unwrap();
        match guard.dream_runner_has_capability(&scope, &req.runner_id, "chat", &now) {
            Ok(value) => value,
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
        }
    };
    if !has_chat {
        return Json(serde_json::json!({"request_id": req_id.0, "status": "missing_chat"}))
            .into_response();
    }
    let has_scoped_read = {
        let guard = state.store.lock().unwrap();
        match guard.dream_runner_has_capability(
            &scope,
            &req.runner_id,
            "dream_scoped_read_v1",
            &now,
        ) {
            Ok(value) => value,
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
        }
    };
    if !has_scoped_read {
        return Json(serde_json::json!({"request_id": req_id.0, "status": "missing_dream_read"}))
            .into_response();
    }
    let mut claim = {
        let mut guard = state.store.lock().unwrap();
        match guard.dream_runner_claim(&scope, &req.runner_id, &now, 120) {
            Ok(value) => value,
            Err(StoreError::StateConflict) => {
                return err(
                    &req_id.0,
                    StatusCode::CONFLICT,
                    ErrorCode::StateConflict,
                    "runner 未注册或 lease 已过期",
                )
            }
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    &e.to_string(),
                )
            }
        }
    };
    let Some(mut work) = claim.take() else {
        return Json(serde_json::json!({"request_id": req_id.0, "status": "idle"})).into_response();
    };
    if work.adjudication_job.is_none() {
        let accepted = {
            let guard = state.store.lock().unwrap();
            if work.dream_job.purpose == "redecision" {
                None
            } else {
                match guard.dream_accepted_candidates(&scope, &work.dream_job.id) {
                    Ok(candidates) if !candidates.is_empty() => Some(candidates),
                    _ => None,
                }
            }
        };
        let prep = if work.dream_job.purpose == "redecision" {
            dream_worker::prepare_runner_redecision(&state, &scope, &work.dream_job).await
        } else if accepted.is_some() {
            dream_worker::prepare_runner_adjudication(&state, &scope, &work.dream_job).await
        } else {
            Ok(())
        };
        if let Err(failure) = prep {
            let mut guard = state.store.lock().unwrap();
            match failure {
                dream_worker::FreezeError::EmbeddingUnavailable { error_code } => {
                    let _ = guard.dream_provider_wait(
                        &scope,
                        &work.dream_job.id,
                        work.dream_job.claim_generation,
                        &error_code,
                        Some(dream_worker::PROVIDER_RECHECK_SECS),
                    );
                    return Json(serde_json::json!({
                        "request_id": req_id.0,
                        "status": "waiting",
                        "reason": "embedding_provider_unavailable",
                        "error_code": error_code,
                        "retry_after_secs": dream_worker::PROVIDER_RECHECK_SECS,
                    }))
                    .into_response();
                }
                dream_worker::FreezeError::StaleInput => {
                    let _ = guard.dream_stale_input(
                        &scope,
                        &work.dream_job.id,
                        work.dream_job.claim_generation,
                        "DREAM_SOURCE_STALE",
                    );
                    return Json(
                        serde_json::json!({"request_id": req_id.0, "status": "stale_input"}),
                    )
                    .into_response();
                }
                dream_worker::FreezeError::Store(e) => {
                    let _ = guard.dream_dead(
                        &scope,
                        &work.dream_job.id,
                        work.dream_job.claim_generation,
                        "DREAM_PREPARE_FAILED",
                    );
                    return err(
                        &req_id.0,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorCode::Internal,
                        &e.to_string(),
                    );
                }
            }
        }
        if work.dream_job.purpose == "redecision" || accepted.is_some() {
            let mut guard = state.store.lock().unwrap();
            work = match guard.dream_runner_claim(
                &scope,
                &req.runner_id,
                &memory_store_sqlite::now_rfc3339_pub().unwrap_or(now),
                120,
            ) {
                Ok(Some(next)) => next,
                Ok(None) => {
                    return Json(serde_json::json!({"request_id": req_id.0, "status": "idle"}))
                        .into_response()
                }
                Err(e) => {
                    return err(
                        &req_id.0,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorCode::Internal,
                        &e.to_string(),
                    )
                }
            };
        }
    }

    if let Some(adjudication_job) = work.adjudication_job {
        let (candidates, recalls) = {
            let guard = state.store.lock().unwrap();
            match guard.adjudication_inputs(&scope, &adjudication_job.id) {
                Ok(v) => v,
                Err(e) => {
                    return err(
                        &req_id.0,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorCode::Internal,
                        &e.to_string(),
                    )
                }
            }
        };
        let targets = {
            let guard = state.store.lock().unwrap();
            recalls.iter().filter_map(|r| guard.get_memory(&scope, &r.target_memory_id, &dom).ok().flatten().map(|m| serde_json::json!({
                "candidate_id": r.candidate_id, "target_memory_id": r.target_memory_id,
                "kind": m.kind, "claim": m.claim, "version": m.version,
            }))).collect::<Vec<_>>()
        };
        return Json(serde_json::json!({
            "request_id": req_id.0,
            "status": "claimed",
            "work": {
                "phase": "adjudicate",
                "runner_id": req.runner_id,
                "dream_job_id": work.dream_job.id,
                "dream_generation": work.dream_job.claim_generation,
                "adjudication_job_id": adjudication_job.id,
                "adjudication_generation": adjudication_job.claim_generation,
                "adjudication_version": adjudication_job.adjudication_version,
                "semantic_search_complete": adjudication_job.embedding_model_id.is_some(),
                "candidates": candidates.iter().map(|c| serde_json::json!({
                    "candidate_id": c.candidate_id, "kind": c.kind, "claim": c.claim,
                    "quote": c.quote, "status": c.status, "evidence_id": c.evidence_id,
                    "start_byte": c.start_byte, "end_byte": c.end_byte,
                })).collect::<Vec<_>>(),
                "recalled_targets": targets,
            }
        }))
        .into_response();
    }
    if let Some(consolidation_job) = work.consolidation_job {
        let data = {
            let mut guard = state.store.lock().unwrap();
            let inputs = match guard.consolidation_inputs(&scope, &consolidation_job.id) {
                Ok(v) => v,
                Err(e) => {
                    return err(
                        &req_id.0,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorCode::Internal,
                        &e.to_string(),
                    )
                }
            };
            let mut sources = Vec::with_capacity(inputs.len());
            for (id, version, sha) in inputs {
                match guard.get_memory(&scope, &id, &dom) {
                    Ok(Some(m))
                        if m.status == "active"
                            && m.version == version
                            && memory_claim_hash(&m.kind, &m.claim).as_deref()
                                == Some(sha.as_str()) =>
                    {
                        sources.push(serde_json::json!({"memory_id":id,"memory_version":version,"claim":m.claim}));
                    }
                    _ => {
                        let _ = guard.consolidation_stale_input(
                            &scope,
                            &consolidation_job.id,
                            consolidation_job.claim_generation,
                        );
                        let _ = guard.dream_stale_input(
                            &scope,
                            &work.dream_job.id,
                            work.dream_job.claim_generation,
                            "PAGE_SOURCE_STALE",
                        );
                        return Json(
                            serde_json::json!({"request_id":req_id.0,"status":"stale_input"}),
                        )
                        .into_response();
                    }
                }
            }
            let question = if consolidation_job.document_kind == "mental_model" {
                guard
                    .question_list(&scope, Some("active"))
                    .ok()
                    .and_then(|rows| {
                        rows.into_iter().find(|q| {
                            q.question_key == consolidation_job.document_key
                                && Some(q.version) == consolidation_job.question_version
                        })
                    })
            } else {
                None
            };
            if consolidation_job.document_kind == "mental_model" && question.is_none() {
                let _ = guard.consolidation_stale_input(
                    &scope,
                    &consolidation_job.id,
                    consolidation_job.claim_generation,
                );
                let _ = guard.dream_stale_input(
                    &scope,
                    &work.dream_job.id,
                    work.dream_job.claim_generation,
                    "QUESTION_STALE",
                );
                return Json(serde_json::json!({"request_id":req_id.0,"status":"stale_input"}))
                    .into_response();
            }
            serde_json::json!({
                "phase":"consolidate","runner_id":req.runner_id,
                "dream_job_id":work.dream_job.id,"dream_generation":work.dream_job.claim_generation,
                "consolidation_job_id":consolidation_job.id,"consolidation_generation":consolidation_job.claim_generation,
                "document_kind":consolidation_job.document_kind,"document_key":consolidation_job.document_key,
                "question_version":consolidation_job.question_version,
                "question_text":question.map(|q|q.question_text),
                "generator_version":consolidation_job.generator_version,
                "input_fingerprint":consolidation_job.input_fingerprint,"sources":sources,
            })
        };
        return Json(serde_json::json!({"request_id":req_id.0,"status":"claimed","work":data}))
            .into_response();
    }
    let (frozen_evidence, legacy_inputs) = if work.dream_job.extract_version
        == memory_store_sqlite::dream_jobs::DREAM_EXTRACT_V1
    {
        let guard = state.store.lock().unwrap();
        match guard.dream_frozen_inputs(&scope, &work.dream_job.id) {
            Ok(rows) => (Vec::new(), rows),
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::CONFLICT,
                    ErrorCode::StateConflict,
                    &e.to_string(),
                )
            }
        }
    } else {
        let guard = state.store.lock().unwrap();
        match guard.dream_read_manifest(&scope,&req.runner_id,&work.dream_job.id,work.dream_job.claim_generation) {
            Ok(manifest) => (manifest.evidence.iter().map(|(id,role,seq)|serde_json::json!({"evidence_id":id,"role":role,"event_seq":seq})).collect::<Vec<_>>(), Vec::new()),
            Err(e) => return dream_read_error(&req_id.0,e),
        }
    };
    Json(serde_json::json!({
        "request_id": req_id.0,
        "status": "claimed",
        "work": {
            "phase": if work.dream_job.purpose == "redecision" { "redecision" } else { "extract" },
            "runner_id": req.runner_id,
            "dream_job_id": work.dream_job.id,
            "dream_generation": work.dream_job.claim_generation,
            "purpose": work.dream_job.purpose,
            "extract_version": work.dream_job.extract_version,
            "input_fingerprint": work.dream_job.input_fingerprint,
            "frozen_evidence": frozen_evidence,
            "inputs": legacy_inputs.iter().map(|(id,role,content)|serde_json::json!({"evidence_id":id,"role":role,"content":content})).collect::<Vec<_>>(),
        }
    })).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamRunnerLeaseRequest {
    runner_id: String,
    dream_job_id: String,
    dream_generation: i64,
    #[serde(default)]
    adjudication_job_id: Option<String>,
    #[serde(default)]
    adjudication_generation: Option<i64>,
    #[serde(default)]
    consolidation_job_id: Option<String>,
    #[serde(default)]
    consolidation_generation: Option<i64>,
}

async fn dream_runner_lease(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DreamRunnerLeaseRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "runner lease 请求字段非法",
            )
        }
    };
    if req.adjudication_job_id.is_some() != req.adjudication_generation.is_some()
        || req.consolidation_job_id.is_some() != req.consolidation_generation.is_some()
        || (req.adjudication_job_id.is_some() && req.consolidation_job_id.is_some())
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "runner lease 阶段字段不匹配",
        );
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.dream_runner_lease(
            &scope,
            &req.runner_id,
            &req.dream_job_id,
            req.dream_generation,
            req.adjudication_job_id
                .as_deref()
                .zip(req.adjudication_generation),
            req.consolidation_job_id
                .as_deref()
                .zip(req.consolidation_generation),
            120,
        )
    };
    match result {
        Ok(true) => {
            Json(serde_json::json!({"request_id":req_id.0,"status":"leased","lease_seconds":120}))
                .into_response()
        }
        Ok(false) | Err(StoreError::StaleClaim) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "runner claim 已过期或 generation 不匹配",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamRunnerFailureRequest {
    runner_id: String,
    phase: String,
    dream_job_id: String,
    dream_generation: i64,
    #[serde(default)]
    adjudication_job_id: Option<String>,
    #[serde(default)]
    adjudication_generation: Option<i64>,
    #[serde(default)]
    consolidation_job_id: Option<String>,
    #[serde(default)]
    consolidation_generation: Option<i64>,
    error_code: String,
}

async fn dream_runner_failure(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    body: Result<Json<DreamRunnerFailureRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "runner failure 请求字段非法",
            )
        }
    };
    const FAILURE_CODES: &[&str] = &[
        "SUBAGENT_FAILED",
        "BAD_CHILD_OUTPUT",
        "SUBMIT_FAILED",
        "RUNNER_DISPATCH_FAILED",
    ];
    if !FAILURE_CODES.contains(&req.error_code.as_str())
        || !matches!(
            req.phase.as_str(),
            "extract" | "redecision" | "adjudicate" | "consolidate"
        )
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "runner failure code/phase 非法",
        );
    }
    let result = {
        let mut guard = state.store.lock().unwrap();
        if !guard
            .dream_runner_owns(
                &scope,
                &req.runner_id,
                &req.dream_job_id,
                req.dream_generation,
            )
            .unwrap_or(false)
        {
            return err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "runner 不持有当前 Dream claim",
            );
        }
        let deterministic_failure = req.error_code == "BAD_CHILD_OUTPUT";
        if let Some(id) = req.adjudication_job_id.as_deref() {
            let Some(generation) = req.adjudication_generation else {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "缺少 adjudication generation",
                );
            };
            let _ = guard.adjudication_finish(
                &scope,
                id,
                generation,
                if deterministic_failure {
                    "dead"
                } else {
                    "provider_wait"
                },
                Some(&req.error_code),
                None,
                None,
                None,
                if deterministic_failure {
                    None
                } else {
                    Some(dream_worker::PROVIDER_RECHECK_SECS)
                },
            );
        } else if let Some(id) = req.consolidation_job_id.as_deref() {
            let Some(generation) = req.consolidation_generation else {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "缺少 consolidation generation",
                );
            };
            if deterministic_failure {
                let _ = guard.consolidation_dead(&scope, id, generation, &req.error_code);
            } else {
                let _ = guard.consolidation_retryable_fail(
                    &scope,
                    id,
                    generation,
                    &req.error_code,
                    dream_worker::PROVIDER_RECHECK_SECS as u64,
                );
            }
        }
        if deterministic_failure {
            guard.dream_dead(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                &req.error_code,
            )
        } else {
            guard.dream_provider_wait(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                &req.error_code,
                Some(dream_worker::PROVIDER_RECHECK_SECS),
            )
        }
    };
    match result {
        Ok(true) => {
            Json(serde_json::json!({"request_id":req_id.0,"status":"recorded"})).into_response()
        }
        Ok(false) | Err(StoreError::StaleClaim) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "Dream generation 已失效",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamCandidateSubmitRequest {
    runner_id: String,
    generation: i64,
    output: serde_json::Value,
}

async fn dream_runner_candidates(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
    body: Result<Json<DreamCandidateSubmitRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "candidate submit 请求字段非法",
            )
        }
    };
    let (job, proposals) = {
        let mut guard = state.store.lock().unwrap();
        if !guard
            .dream_runner_owns(&scope, &req.runner_id, &job_id, req.generation)
            .unwrap_or(false)
        {
            return err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "runner 不持有当前 Dream claim",
            );
        }
        let job = match guard.dream_get(&scope, &job_id) {
            Ok(Some(j)) => j,
            _ => {
                return err(
                    &req_id.0,
                    StatusCode::NOT_FOUND,
                    ErrorCode::NotFound,
                    "Dream job 不存在",
                )
            }
        };
        let raw = match serde_json::to_string(&req.output) {
            Ok(v) => v,
            Err(_) => {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "candidate 输出非法",
                )
            }
        };
        let candidates = match memory_store_sqlite::dream_jobs::parse_dream_extract_v1(&raw) {
            Ok(v) => v,
            Err(_) => {
                let _ = guard.dream_dead(&scope, &job_id, req.generation, "BAD_JSON");
                return err(
                    &req_id.0,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    ErrorCode::InvalidField,
                    "Dream 提案 schema 不合法，作业已 dead",
                );
            }
        };
        let frozen = match guard.dream_frozen_inputs(&scope, &job_id) {
            Ok(rows) => rows
                .into_iter()
                .map(|(id, _, body)| (id, body))
                .collect::<std::collections::HashMap<_, _>>(),
            Err(e) => {
                return err(
                    &req_id.0,
                    StatusCode::CONFLICT,
                    ErrorCode::StateConflict,
                    &e.to_string(),
                )
            }
        };
        let proposals = candidates
            .into_iter()
            .map(|c| {
                let span = frozen.get(&c.evidence_id).and_then(|body| {
                    memory_store_sqlite::dream_jobs::locate_quote_span(body, &c.quote)
                });
                let (start_byte, end_byte) = span.unwrap_or((-1, -1));
                memory_store_sqlite::dream_jobs::DreamProposal {
                    kind: c.kind,
                    claim: c.claim,
                    quote: c.quote,
                    evidence_id: c.evidence_id,
                    start_byte,
                    end_byte,
                    status: "candidate".into(),
                    reason_code: None,
                    occurred_at: c.occurred_at,
                }
            })
            .collect::<Vec<_>>();
        (job, proposals)
    };
    match dream_worker::runner_submit_candidates(&state, &scope, &job, &proposals).await {
        Ok((accepted,rejected)) => Json(serde_json::json!({"request_id":req_id.0,"status":"submitted","accepted":accepted,"rejected":rejected})).into_response(),
        Err(dream_worker::FreezeError::StaleInput) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StateConflict, "Dream 输入已失效"),
        Err(dream_worker::FreezeError::EmbeddingUnavailable { error_code }) => Json(serde_json::json!({
            "request_id": req_id.0,
            "status": "waiting",
            "reason": "embedding_provider_unavailable",
            "error_code": error_code,
            "retry_after_secs": dream_worker::PROVIDER_RECHECK_SECS,
        })).into_response(),
        Err(dream_worker::FreezeError::Store(StoreError::StaleClaim)) => err(&req_id.0, StatusCode::CONFLICT, ErrorCode::StateConflict, "Dream generation 已失效"),
        Err(dream_worker::FreezeError::Store(e)) => err(&req_id.0, StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, &e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamAdjudicationSubmitRequest {
    runner_id: String,
    dream_generation: i64,
    generation: i64,
    output: serde_json::Value,
}

async fn dream_runner_adjudication(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(job_id): AxumPath<String>,
    body: Result<Json<DreamAdjudicationSubmitRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "adjudication submit 请求字段非法",
            )
        }
    };
    let mut guard = state.store.lock().unwrap();
    if !guard
        .dream_runner_owns(
            &scope,
            &req.runner_id,
            &guard
                .adjudication_get(&scope, &job_id)
                .ok()
                .flatten()
                .map(|j| j.dream_job_id)
                .unwrap_or_default(),
            req.dream_generation,
        )
        .unwrap_or(false)
    {
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "runner 不持有当前 Dream claim",
        );
    }
    let adj = match guard.adjudication_get(&scope, &job_id) {
        Ok(Some(j)) if j.status == "running" && j.claim_generation == req.generation => j,
        _ => {
            return err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "adjudication generation 已失效",
            )
        }
    };
    let raw = match serde_json::to_string(&req.output) {
        Ok(v) => v,
        Err(_) => String::new(),
    };
    let items = match memory_store_sqlite::adjudication::parse_adjudicate_v1(&raw) {
        Ok(v) => v,
        Err(_) => {
            let _ = guard.adjudication_finish(
                &scope,
                &job_id,
                req.generation,
                "dead",
                Some("BAD_JSON"),
                None,
                None,
                None,
                None,
            );
            let _ = guard.dream_dead(
                &scope,
                &adj.dream_job_id,
                req.dream_generation,
                "ADJUDICATION_BAD_JSON",
            );
            return err(
                &req_id.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidField,
                "adjudication 输出 schema 不合法，作业已 dead",
            );
        }
    };
    let proposals = items
        .into_iter()
        .map(
            |i| memory_store_sqlite::adjudication::AdjudicationProposal {
                candidate_id: i.candidate_id,
                durability: i.durability,
                action: i.action,
                reason_code: i.reason_code,
                target_memory_id: i.target_memory_id,
                expected_target_version: i.expected_target_version,
                model_confidence: i.model_confidence,
                valid_until: i.valid_until,
            },
        )
        .collect::<Vec<_>>();
    let apply = guard.adjudication_apply_and_complete(
        &scope,
        &job_id,
        req.generation,
        req.dream_generation,
        &proposals,
    );
    match apply {
        Ok(outcome) => Json(serde_json::json!({"request_id":req_id.0,"status":"applied","applied":outcome.applied,"rejected":outcome.rejected,"held":outcome.held})).into_response(),
        Err(StoreError::InvalidAdjudicationCoverage) => {
            let _ = guard.adjudication_finish(
                &scope,
                &job_id,
                req.generation,
                "dead",
                Some("ADJUDICATION_BAD_JSON"),
                None,
                None,
                None,
                None,
            );
            let _ = guard.dream_dead(
                &scope,
                &adj.dream_job_id,
                req.dream_generation,
                "ADJUDICATION_BAD_JSON",
            );
            err(
                &req_id.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidField,
                "adjudication 输出必须覆盖每个冻结候选且不得重复",
            )
        }
        Err(StoreError::DreamSearchIncomplete) => {
            let _ = guard.adjudication_finish(&scope,&job_id,req.generation,"dead",Some("DREAM_SEARCH_REQUIRED"),None,None,None,None);
            let _ = guard.dream_dead(&scope,&adj.dream_job_id,req.dream_generation,"DREAM_SEARCH_REQUIRED");
            err(&req_id.0,StatusCode::UNPROCESSABLE_ENTITY,ErrorCode::InvalidField,
                "adjudicate_v2 必须对每个冻结候选完成语义搜索后才能提交")
        }
        Err(StoreError::StaleInput) => {
            let _ = guard.adjudication_finish(
                &scope,
                &job_id,
                req.generation,
                "stale_input",
                Some("INPUT_DRIFT"),
                None,
                None,
                None,
                None,
            );
            let _ = guard.dream_stale_input(
                &scope,
                &adj.dream_job_id,
                req.dream_generation,
                "ADJUDICATION_INPUT_DRIFT",
            );
            err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "adjudication 冻结输入已失效",
            )
        }
        Err(StoreError::StaleClaim) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "adjudication generation 已失效",
        ),
        Err(e) => {
            let _ = guard.adjudication_finish(
                &scope,
                &job_id,
                req.generation,
                "dead",
                Some("APPLY_FAILED"),
                None,
                None,
                None,
                None,
            );
            let _ = guard.dream_dead(
                &scope,
                &adj.dream_job_id,
                req.dream_generation,
                "ADJUDICATION_APPLY_FAILED",
            );
            err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamPagePublishRequest {
    runner_id: String,
    dream_job_id: String,
    dream_generation: i64,
    generation: i64,
    output: serde_json::Value,
}

async fn dream_runner_publish_page(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(job_id): AxumPath<String>,
    body: Result<Json<DreamPagePublishRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "page publish 请求字段非法",
            )
        }
    };
    let mut guard = state.store.lock().unwrap();
    if !guard
        .dream_runner_owns(
            &scope,
            &req.runner_id,
            &req.dream_job_id,
            req.dream_generation,
        )
        .unwrap_or(false)
        || !guard
            .dream_consolidation_linked(&scope, &req.dream_job_id, &job_id)
            .unwrap_or(false)
    {
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "runner 不持有该派生页作业",
        );
    }
    let job = match guard.consolidation_get(&scope, &job_id) {
        Ok(Some(j)) if j.status == "running" && j.claim_generation == req.generation => j,
        _ => {
            return err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "consolidation generation 已失效",
            )
        }
    };
    let title = req
        .output
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let body_md = req
        .output
        .get("body_md")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let description = req
        .output
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let page_v2 = job.generator_version == memory_store_sqlite::pages::GENERATE_CONSOLIDATE_V2
        && job.document_kind == "topic_page";
    let known_version = page_v2
        || ((job.generator_version == memory_store_sqlite::pages::GENERATE_CONSOLIDATE_V1
            || job.generator_version == memory_store_sqlite::pages::GENERATE_MENTAL_MODEL_V1)
            && !page_v2);
    if !known_version
        || req
            .output
            .as_object()
            .is_none_or(|o| o.len() != if page_v2 { 3 } else { 2 })
        || title.is_empty()
        || body_md.is_empty()
        || (page_v2 && (description.is_empty() || description.chars().count() > 240))
    {
        let _ = guard.consolidation_dead(&scope, &job_id, req.generation, "BAD_PAGE_OUTPUT");
        let _ = guard.dream_dead(
            &scope,
            &req.dream_job_id,
            req.dream_generation,
            "BAD_PAGE_OUTPUT",
        );
        return err(
            &req_id.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidField,
            "派生页输出 schema 不合法，作业已 dead",
        );
    }
    let inputs = match guard.consolidation_inputs(&scope, &job_id) {
        Ok(rows) => rows,
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    let mut sources = Vec::with_capacity(inputs.len());
    for (id, version, sha) in inputs {
        match guard.get_memory(&scope, &id, &dom) {
            Ok(Some(memory))
                if memory.status == "active"
                    && memory.version == version
                    && memory_claim_hash(&memory.kind, &memory.claim).as_deref()
                        == Some(sha.as_str()) =>
            {
                sources.push((id, version, sha))
            }
            _ => {
                let _ = guard.consolidation_stale_input(&scope, &job_id, req.generation);
                let _ = guard.dream_stale_input(
                    &scope,
                    &req.dream_job_id,
                    req.dream_generation,
                    "PAGE_SOURCE_STALE",
                );
                return err(
                    &req_id.0,
                    StatusCode::CONFLICT,
                    ErrorCode::StateConflict,
                    "派生页来源已变化",
                );
            }
        }
    }
    let question = if job.document_kind == "mental_model" {
        match guard.question_list(&scope, Some("active")) {
            Ok(rows) => rows.into_iter().find(|q| {
                q.question_key == job.document_key && Some(q.version) == job.question_version
            }),
            Err(_) => None,
        }
    } else {
        None
    };
    if job.document_kind == "mental_model" && question.is_none() {
        let _ = guard.consolidation_stale_input(&scope, &job_id, req.generation);
        let _ = guard.dream_stale_input(
            &scope,
            &req.dream_job_id,
            req.dream_generation,
            "QUESTION_STALE",
        );
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "问题目录版本已变化",
        );
    }
    let request = memory_store_sqlite::pages::PublishRequest {
        scope: &scope,
        document_kind: &job.document_kind,
        document_key: &job.document_key,
        question_version: job.question_version,
        question_text: question.as_ref().map(|q| q.question_text.as_str()),
        title,
        body_md,
        generator_version: &job.generator_version,
        input_fingerprint: &job.input_fingerprint,
        sources: &sources,
        // memory_pages/page_revisions uses the frozen actor CHECK from 0006;
        // background Dream work is recorded as the allowed system actor.
        actor_kind: "system",
    };
    let publish_result = if page_v2 {
        let compared = guard
            .dream_page_search_ready(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                &job.document_key,
                &dom,
            )
            .unwrap_or(false);
        if !compared {
            let _ =
                guard.consolidation_dead(&scope, &job_id, req.generation, "PAGE_COMPARE_REQUIRED");
            let _ = guard.dream_dead(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                "PAGE_COMPARE_REQUIRED",
            );
            return err(
                &req_id.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidField,
                "consolidate_v2 必须先完成语义搜索；若已有有效同 key 页面，还必须读取其冻结版本",
            );
        }
        guard.publish_page_with_description_idempotent(&request, description, &dom)
    } else {
        guard.publish_page_idempotent(&request, &dom)
    };
    let (page_id, page_version) = match publish_result {
        Ok(v) => v,
        Err(StoreError::StaleInput) => {
            let _ = guard.consolidation_stale_input(&scope, &job_id, req.generation);
            let _ = guard.dream_stale_input(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                "PAGE_SOURCE_STALE",
            );
            return err(
                &req_id.0,
                StatusCode::CONFLICT,
                ErrorCode::StateConflict,
                "派生页来源已变化，未发布",
            );
        }
        Err(e) => {
            let _ =
                guard.consolidation_dead(&scope, &job_id, req.generation, "PAGE_PUBLISH_FAILED");
            let _ = guard.dream_dead(
                &scope,
                &req.dream_job_id,
                req.dream_generation,
                "PAGE_PUBLISH_FAILED",
            );
            return err(
                &req_id.0,
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidField,
                &e.to_string(),
            );
        }
    };
    let succeeded = guard
        .consolidation_succeed(&scope, &job_id, req.generation, None, None, None)
        .unwrap_or(false);
    let dream_succeeded = guard
        .dream_succeed(
            &scope,
            &req.dream_job_id,
            req.dream_generation,
            None,
            None,
            None,
        )
        .unwrap_or(false);
    if !succeeded || !dream_succeeded {
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "页面已发布，但作业 generation 已变化，需 runner 恢复对账",
        );
    }
    if let Some(embedding) = &state.embedding {
        let _ = guard.semantic_enqueue(&scope, "page", &page_id, embedding.model_id());
    }
    Json(serde_json::json!({"request_id":req_id.0,"status":"published","page_id":page_id,"version":page_version})).into_response()
}

async fn post_dream_rejudge(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    AxumPath(candidate_id): AxumPath<String>,
    body: Result<Json<DreamRedecisionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let result = {
        let mut guard = state.store.lock().unwrap();
        guard.dream_redecision_trigger(
            &scope,
            &candidate_id,
            &req.idempotency_key,
            None,
            None,
            None,
        )
    };
    match result {
        Ok(job) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "request_id": req_id.0,
                "status": "redecision_queued",
                "job": dream_job_json(&job),
            })),
        )
            .into_response(),
        Err(StoreError::JobNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "Held candidate 不存在或不属于当前 scope",
        ),
        Err(StoreError::StaleInput) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "Held candidate 已没有有效来源，不能重裁",
        ),
        Err(StoreError::InvalidPageField) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "idempotency_key 须为 1—128 字符",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        "purpose": job.purpose,
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
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～100",
        );
    }
    if let Some(s) = query.status.as_deref() {
        if !matches!(
            s,
            "queued"
                | "running"
                | "succeeded"
                | "retryable_failed"
                | "provider_wait"
                | "dead"
                | "stale_input"
        ) {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "status 非法",
            );
        }
    }
    let guard = state.store.lock().unwrap();
    match guard.dream_list(&scope, query.status.as_deref(), limit) {
        Ok(rows) => Json(serde_json::json!({
            "request_id": req_id.0,
            "jobs": rows.iter().map(dream_job_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "作业不存在或不属于当前 scope",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DreamScopedReadRequest {
    runner_id: String,
    dream_generation: i64,
    operation: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    candidate_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn dream_read_error(req_id: &str, error: StoreError) -> Response {
    match error {
        StoreError::StaleClaim => err(
            req_id,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "Dream runner lease 或 generation 已失效",
        ),
        StoreError::StaleInput => err(
            req_id,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "Dream 读取目标版本已变化；不得继续使用旧内容",
        ),
        StoreError::JobNotFound
        | StoreError::EvidenceNotFound
        | StoreError::MemoryNotFound
        | StoreError::PageNotFound => err(
            req_id,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "读取目标不存在、超出本 job 范围或已失效",
        ),
        StoreError::InvalidPageField => err(
            req_id,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "Dream 读取参数超出限制",
        ),
        StoreError::DreamReadBudgetExceeded => err(
            req_id,
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimited,
            "Dream 子 Agent 已达到本 job 的 32 次只读调用预算",
        ),
        other => err(
            req_id,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &other.to_string(),
        ),
    }
}

/// One bounded, read-only endpoint for the DSH Dream child. Authentication
/// scope comes from Bearer middleware; runner/job/generation are additionally
/// checked in SQLite before each read and again when search results are frozen.
async fn dream_scoped_read(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(job_id): AxumPath<String>,
    body: Result<Json<DreamScopedReadRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, None);
    let Json(req) = match body {
        Ok(body) => body,
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "Dream read 请求字段非法",
            )
        }
    };
    if req.runner_id.trim().is_empty()
        || req.runner_id.chars().count() > 256
        || req.dream_generation <= 0
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "runner_id/generation 非法",
        );
    }
    let limit = req.limit.unwrap_or(8);
    if !(1..=10).contains(&limit) {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须为 1—10",
        );
    }
    let ids = req.ids.unwrap_or_default();
    if ids.len() > 20
        || ids
            .iter()
            .any(|id| id.is_empty() || id.chars().count() > 128)
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "ids 最多 20 个，每个 1—128 字符",
        );
    }
    if req
        .candidate_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.chars().count() > 128)
    {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "candidate_id 非法",
        );
    }
    let query = req.query.unwrap_or_default().trim().to_string();
    if query.chars().count() > 512 {
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "query 最多 512 个 Unicode 标量字符",
        );
    }

    let budget = state.store.lock().unwrap().dream_consume_read_budget(
        &scope,
        &req.runner_id,
        &job_id,
        req.dream_generation,
    );
    if let Err(error) = budget {
        return dream_read_error(&req_id.0, error);
    }

    match req.operation.as_str() {
        "manifest" => {
            let guard = state.store.lock().unwrap();
            match guard.dream_read_manifest(&scope, &req.runner_id, &job_id, req.dream_generation) {
                Ok(manifest) => Json(serde_json::json!({
                    "request_id": req_id.0,
                    "manifest": {
                        "job_id": manifest.job_id,
                        "purpose": manifest.purpose,
                        "status": manifest.status,
                        "generation": manifest.generation,
                        "input_fingerprint": manifest.input_fingerprint,
                        "frozen_evidence": manifest.evidence.iter().map(|(id,role,seq)| serde_json::json!({"evidence_id":id,"role":role,"event_seq":seq})).collect::<Vec<_>>(),
                    },
                })).into_response(),
                Err(e) => dream_read_error(&req_id.0, e),
            }
        }
        "evidence" => {
            if ids.is_empty() || !query.is_empty() {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "evidence 需要 ids，不接受 query",
                );
            }
            let guard = state.store.lock().unwrap();
            match guard.dream_read_evidence(&scope, &req.runner_id, &job_id, req.dream_generation, &ids) {
                Ok(rows) => Json(serde_json::json!({
                    "request_id": req_id.0,
                    "evidence": rows.iter().map(|e| serde_json::json!({"evidence_id":e.evidence_id,"role":e.role,"event_seq":e.event_seq,"content":e.content})).collect::<Vec<_>>(),
                    "returned": rows.len(),
                })).into_response(),
                Err(e) => dream_read_error(&req_id.0, e),
            }
        }
        "search_memories" => {
            if query.is_empty() || !ids.is_empty() {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "search_memories 需要 query，不接受 ids",
                );
            }
            let live = {
                let guard = state.store.lock().unwrap();
                match guard.dream_runner_owns(&scope, &req.runner_id, &job_id, req.dream_generation)
                {
                    Ok(true) => true,
                    Ok(false) => return dream_read_error(&req_id.0, StoreError::StaleClaim),
                    Err(e) => return dream_read_error(&req_id.0, e),
                }
            };
            if !live {
                return dream_read_error(&req_id.0, StoreError::StaleClaim);
            }
            let (lexical, index_degraded) = match state
                .store
                .lock()
                .unwrap()
                .search_memories(&scope, &query, 20, false, &dom)
            {
                Ok(result) => result,
                Err(e) => return dream_read_error(&req_id.0, e),
            };
            let mut semantic_ids = Vec::new();
            let mut semantic_status = "disabled";
            if let Some(embedding) = &state.embedding {
                match embedding.embed(&[query.clone()]).await {
                    Ok(vectors) if !vectors.is_empty() => {
                        let vector = &vectors[0];
                        let scan = state.store.lock().unwrap().semantic_scan_with_floor(
                            &scope,
                            "memory",
                            embedding.model_id(),
                            vector,
                            20,
                            state.semantic_min_similarity,
                            &dom,
                        );
                        match scan {
                            Ok((hits, ready)) if ready < memory_contract::SEMANTIC_SCAN_LIMIT => {
                                semantic_ids = hits.into_iter().map(|(id, _)| id).collect();
                                semantic_status = "ok";
                            }
                            Ok(_) => semantic_status = "limit_exceeded",
                            Err(_) => semantic_status = "unavailable",
                        }
                    }
                    Ok(_) | Err(_) => semantic_status = "unavailable",
                }
            }
            let mut info = std::collections::HashMap::new();
            {
                let guard = state.store.lock().unwrap();
                for hit in &lexical {
                    if let Ok(Some(memory)) = guard.get_memory(&scope, &hit.memory_id, &dom) {
                        info.insert(hit.memory_id.clone(), memory);
                    }
                }
                for id in &semantic_ids {
                    if !info.contains_key(id) {
                        if let Ok(Some(memory)) = guard.get_memory(&scope, id, &dom) {
                            info.insert(id.clone(), memory);
                        }
                    }
                }
            }
            let mut ranks: std::collections::HashMap<String, Vec<u32>> =
                std::collections::HashMap::new();
            for (i, hit) in lexical.iter().enumerate() {
                if info.contains_key(&hit.memory_id) {
                    ranks
                        .entry(hit.memory_id.clone())
                        .or_default()
                        .push(i as u32 + 1);
                }
            }
            for (i, id) in semantic_ids.iter().enumerate() {
                if info.contains_key(id) {
                    ranks.entry(id.clone()).or_default().push(i as u32 + 1);
                }
            }
            let mut ordered: Vec<(String, f64)> = ranks
                .iter()
                .map(|(id, r)| (id.clone(), memory_recall::rrf_score(r)))
                .collect();
            ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ordered.truncate(limit);
            let candidates: Vec<(String, i64, String)> = ordered
                .iter()
                .filter_map(|(id, _)| {
                    info.get(id)
                        .map(|m| (id.clone(), m.version, m.claim.clone()))
                })
                .collect();
            let accepted = match state.store.lock().unwrap().dream_snapshot_memories(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
                &candidates,
            ) {
                Ok(ids) => ids.into_iter().collect::<std::collections::HashSet<_>>(),
                Err(e) => return dream_read_error(&req_id.0, e),
            };
            let items = ordered.into_iter().filter(|(id,_)| accepted.contains(id)).filter_map(|(id,score)| info.get(&id).map(|m| serde_json::json!({
                "memory_id":id,"version":m.version,"kind":m.kind,"claim":m.claim,"score":score,
                "matched_by": {"lexical": lexical.iter().any(|h| h.memory_id == id), "semantic": semantic_ids.contains(&id)},
            }))).collect::<Vec<_>>();
            if let Err(e) = state.store.lock().unwrap().dream_record_search(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
                "memory",
                req.candidate_id.as_deref(),
                &query,
                semantic_status == "ok",
                items.len(),
            ) {
                return dream_read_error(&req_id.0, e);
            }
            Json(serde_json::json!({"request_id":req_id.0,"items":items,"lexical_status":if index_degraded{"index_degraded"}else{"ok"},"semantic_status":semantic_status,"complete":semantic_status=="ok"})).into_response()
        }
        "search_pages" => {
            if query.is_empty() || !ids.is_empty() {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "search_pages 需要 query，不接受 ids",
                );
            }
            match state.store.lock().unwrap().dream_runner_owns(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
            ) {
                Ok(true) => {}
                Ok(false) => return dream_read_error(&req_id.0, StoreError::StaleClaim),
                Err(e) => return dream_read_error(&req_id.0, e),
            }
            let now = match memory_store_sqlite::now_rfc3339_pub() {
                Ok(v) => v,
                Err(e) => return dream_read_error(&req_id.0, e),
            };
            let lexical = match state
                .store
                .lock()
                .unwrap()
                .page_fts_search(&scope, &query, 20, &dom)
            {
                Ok(v) => v,
                Err(e) => return dream_read_error(&req_id.0, e),
            };
            let mut semantic_ids = Vec::new();
            let mut semantic_status = "disabled";
            if let Some(embedding) = &state.embedding {
                match embedding.embed(&[query.clone()]).await {
                    Ok(vectors) if !vectors.is_empty() => {
                        match state.store.lock().unwrap().semantic_scan_with_floor(
                            &scope,
                            "page",
                            embedding.model_id(),
                            &vectors[0],
                            20,
                            state.semantic_min_similarity,
                            &dom,
                        ) {
                            Ok((hits, ready)) if ready < memory_contract::SEMANTIC_SCAN_LIMIT => {
                                semantic_ids = hits.into_iter().map(|(id, _)| id).collect();
                                semantic_status = "ok";
                            }
                            Ok(_) => semantic_status = "limit_exceeded",
                            Err(_) => semantic_status = "unavailable",
                        }
                    }
                    Ok(_) | Err(_) => semantic_status = "unavailable",
                }
            }
            let mut ids_order = lexical.clone();
            ids_order.extend(semantic_ids.iter().cloned());
            ids_order.sort();
            ids_order.dedup();
            let mut pages = std::collections::HashMap::new();
            for id in &ids_order {
                if let Ok(Some(page)) = state.store.lock().unwrap().get_page(&scope, id, &now, &dom)
                {
                    pages.insert(id.clone(), page);
                }
            }
            let mut ranks: std::collections::HashMap<String, Vec<u32>> =
                std::collections::HashMap::new();
            for (i, id) in lexical.iter().enumerate() {
                if pages.contains_key(id) {
                    ranks.entry(id.clone()).or_default().push(i as u32 + 1);
                }
            }
            for (i, id) in semantic_ids.iter().enumerate() {
                if pages.contains_key(id) {
                    ranks.entry(id.clone()).or_default().push(i as u32 + 1);
                }
            }
            let mut ordered: Vec<(String, f64)> = ranks
                .iter()
                .map(|(id, r)| (id.clone(), memory_recall::rrf_score(r)))
                .collect();
            ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ordered.truncate(limit);
            let candidates: Vec<(String, i64)> = ordered
                .iter()
                .filter_map(|(id, _)| pages.get(id).map(|p| (id.clone(), p.version)))
                .collect();
            let accepted = match state.store.lock().unwrap().dream_snapshot_pages(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
                &candidates,
            ) {
                Ok(v) => v.into_iter().collect::<std::collections::HashSet<_>>(),
                Err(e) => return dream_read_error(&req_id.0, e),
            };
            let items=ordered.into_iter().filter(|(id,_)|accepted.contains(id)).filter_map(|(id,score)|pages.get(&id).map(|p|serde_json::json!({"page_id":id,"version":p.version,"document_kind":p.document_kind,"document_key":p.document_key,"title":p.title,"description":p.description,"score":score,"matched_by":{"lexical":lexical.contains(&id),"semantic":semantic_ids.contains(&id)}}))).collect::<Vec<_>>();
            if let Err(e) = state.store.lock().unwrap().dream_record_search(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
                "page",
                None,
                &query,
                semantic_status == "ok",
                items.len(),
            ) {
                return dream_read_error(&req_id.0, e);
            }
            Json(serde_json::json!({"request_id":req_id.0,"items":items,"semantic_status":semantic_status,"complete":semantic_status=="ok"})).into_response()
        }
        "get_memory" => {
            if ids.len() != 1 || !query.is_empty() {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "get_memory 需要一个 id",
                );
            }
            let guard = state.store.lock().unwrap();
            match guard.dream_memory_snapshot_valid(&scope,&req.runner_id,&job_id,req.dream_generation,&ids[0]) {
                Ok(true)=>match guard.get_memory(&scope, &ids[0], &dom){Ok(Some(m))=>Json(serde_json::json!({"request_id":req_id.0,"memory":{"memory_id":m.memory_id,"version":m.version,"kind":m.kind,"claim":m.claim,"evidence_refs":m.evidence_refs}})).into_response(),Ok(None)=>dream_read_error(&req_id.0,StoreError::StaleInput),Err(e)=>dream_read_error(&req_id.0,e)},
                Ok(false)=>dream_read_error(&req_id.0,StoreError::MemoryNotFound),Err(e)=>dream_read_error(&req_id.0,e),
            }
        }
        "get_page" => {
            if ids.len() != 1 || !query.is_empty() {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    "get_page 需要一个 id",
                );
            }
            let guard = state.store.lock().unwrap();
            match guard.dream_read_page_detail(
                &scope,
                &req.runner_id,
                &job_id,
                req.dream_generation,
                &ids[0],
            ) {
                Ok(Some(page)) => {
                    Json(serde_json::json!({"request_id":req_id.0,"page":page_json(&page)}))
                        .into_response()
                }
                Ok(None) => dream_read_error(&req_id.0, StoreError::PageNotFound),
                Err(e) => dream_read_error(&req_id.0, e),
            }
        }
        _ => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "operation 必须是 manifest/evidence/search_memories/get_memory/search_pages/get_page",
        ),
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
        return err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidField,
            "limit 必须 1～100",
        );
    }
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => match decode_job_cursor(raw) {
            Ok(c) => Some(c),
            Err(msg) => {
                return err(
                    &req_id.0,
                    StatusCode::BAD_REQUEST,
                    ErrorCode::InvalidField,
                    msg,
                )
            }
        },
    };
    let result = {
        let guard = state.store.lock().unwrap();
        guard.list_jobs(
            &scope,
            status,
            limit,
            cursor.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
        )
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
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "cursor 不是合法 JSON")?;
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
        Ok(None) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "作业不存在",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
        Ok(None) => {
            return err(
                &req_id.0,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "作业不存在",
            )
        }
        Err(e) => {
            return err(
                &req_id.0,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                &e.to_string(),
            )
        }
    };
    if detail.item.status != "dead" {
        return err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "仅 dead 状态作业可重试",
        );
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
        Ok(true) => Json(
            serde_json::json!({ "request_id": req_id.0, "job_id": job_id, "status": "queued" }),
        )
        .into_response(),
        Ok(false) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StateConflict,
            "仅 dead 状态作业可重试",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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

/// D6-9：生命周期请求都携带最新用户事件的精确 UTF-8 byte span 与幂等键。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreRequestBody {
    expected_version: i64,
    idempotency_key: String,
    origin: OriginDto,
    user_evidence_id: String,
    target_quote: String,
    start_byte: i64,
    end_byte: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetireRequestBody {
    expected_version: i64,
    idempotency_key: String,
    origin: OriginDto,
    user_evidence_id: String,
    target_quote: String,
    start_byte: i64,
    end_byte: i64,
}

async fn correct_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<CorrectRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                m,
            )
        }
    };
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
    let req = memory_store_sqlite::CorrectRequest {
        expected_version: body.expected_version,
        origin,
        user_evidence_id: body.user_evidence_id,
        old_quote: body.old_quote,
        replacement_quote: body.replacement_quote,
    };
    match state
        .store
        .lock()
        .unwrap()
        .correct_memory(&scope, &memory_id, &req, &dom)
    {
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
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或非 active",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "版本冲突，请重读当前版本",
        ),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StaleUserEvidence,
            "引用的用户证据不是该会话最新用户事件",
        ),
        Err(StoreError::QuoteMismatch) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::QuoteMismatch,
            "最新用户消息缺少替代原文",
        ),
        Err(StoreError::AmbiguousTarget) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::AmbiguousTarget,
            "目标含糊，请用户更明确表达",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn forget_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<ForgetRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                m,
            )
        }
    };
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
    let req = memory_store_sqlite::ForgetRequest {
        expected_version: body.expected_version,
        origin,
        user_evidence_id: body.user_evidence_id,
        target_quote: body.target_quote,
    };
    match state
        .store
        .lock()
        .unwrap()
        .forget_memory(&scope, &memory_id, &req, &dom)
    {
        Ok(out) => Json(serde_json::json!({
            "request_id": req_id.0,
            "memory_id": out.memory_id,
            "status": "forgotten",
            "version": out.version,
            "raw_evidence_retained": true
        }))
        .into_response(),
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "版本冲突，请重读当前版本",
        ),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StaleUserEvidence,
            "引用的用户证据不是该会话最新用户事件",
        ),
        Err(StoreError::AmbiguousTarget) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::AmbiguousTarget,
            "含糊请求：需明确遗忘动词与目标原话",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

// ---- D6-9：retire / restore（doc6/02 §7、doc6/12）----

async fn retire_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<RetireRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                m,
            )
        }
    };
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
    let req = memory_store_sqlite::lifecycle::RetireRequest {
        expected_version: body.expected_version,
        actor_kind: "user",
        reason_code: Some("user_request".to_string()),
        idempotency_key: body.idempotency_key,
        origin,
        user_evidence_id: body.user_evidence_id,
        target_quote: body.target_quote,
        start_byte: body.start_byte,
        end_byte: body.end_byte,
    };
    match state
        .store
        .lock()
        .unwrap()
        .retire_memory(&scope, &memory_id, &req, &dom)
    {
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
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在或非 active",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "版本冲突，请重读当前版本",
        ),
        Err(StoreError::QuoteMismatch) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::QuoteMismatch,
            "指令 quote/span 与最新用户原文不一致",
        ),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StaleUserEvidence,
            "引用的用户证据不是当前会话最新用户事件",
        ),
        Err(StoreError::AmbiguousTarget) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::AmbiguousTarget,
            "目标 quote 未定位到该记忆",
        ),
        Err(StoreError::IdempotencyConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            "幂等键已用于不同请求",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
    }
}

async fn restore_memory(
    State(state): State<AppState>,
    Extension(scope): Extension<ScopeKey>,
    Extension(req_id): Extension<RequestId>,
    Extension(dom_ctx): Extension<DomainCtx>,
    AxumPath(memory_id): AxumPath<String>,
    body: Result<Json<RestoreRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(b) => b,
        Err(axum::extract::rejection::JsonRejection::JsonSyntaxError(_)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidJson,
                "请求不是合法 JSON",
            )
        }
        Err(_) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                "字段缺失、类型错误或含未知字段",
            )
        }
    };
    let origin = match validate_origin(body.origin) {
        Ok(o) => o,
        Err((_c, m)) => {
            return err(
                &req_id.0,
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidField,
                m,
            )
        }
    };
    let dom = dom_or_return!(&state, &scope, &dom_ctx, &req_id.0, Some(&origin));
    let req = memory_store_sqlite::lifecycle::RestoreRequest {
        expected_version: body.expected_version,
        actor_kind: "user",
        idempotency_key: body.idempotency_key,
        origin,
        user_evidence_id: body.user_evidence_id,
        target_quote: body.target_quote,
        start_byte: body.start_byte,
        end_byte: body.end_byte,
    };
    match state
        .store
        .lock()
        .unwrap()
        .restore_memory(&scope, &memory_id, &req, &dom)
    {
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
        Err(StoreError::MemoryNotFound) => err(
            &req_id.0,
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "记忆不存在、非 active、已到期或无有效证据",
        ),
        Err(StoreError::VersionConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::VersionConflict,
            "版本冲突，请重读当前版本",
        ),
        Err(StoreError::QuoteMismatch) => err(
            &req_id.0,
            StatusCode::BAD_REQUEST,
            ErrorCode::QuoteMismatch,
            "指令 quote/span 与最新用户原文不一致",
        ),
        Err(StoreError::StaleUserEvidence) | Err(StoreError::EvidenceNotFound) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::StaleUserEvidence,
            "引用的用户证据不是当前会话最新用户事件",
        ),
        Err(StoreError::IdempotencyConflict) => err(
            &req_id.0,
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            "幂等键已用于不同请求",
        ),
        Err(e) => err(
            &req_id.0,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            &e.to_string(),
        ),
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
    // /v1/version 在 request_pipeline 里早于认证返回，没有 DomainCtx 扩展；
    // 该端点不读取记忆，不参与域解析（doc7/04 §2.3）。
    Json(VersionResponse {
        protocol_version: PROTOCOL_VERSION,
        schema_version: SCHEMA_VERSION,
        build: BUILD,
        // D6 能力握手（doc6/06 §1）：随卡交付递增；适配器据此启用新注入路径。
        capabilities: vec![
            memory_contract::CAPABILITY_SOUL_V1,
            memory_contract::CAPABILITY_CONTEXT_BUNDLE_V1,
            memory_contract::CAPABILITY_DREAM_SCOPED_READ_V1,
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
        assert!(
            decode_job_cursor("not-base64!!").is_err(),
            "非法 base64url 拒绝"
        );
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
        let big = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vec![b'x'; 600]);
        assert!(decode_job_cursor(&big).is_err(), "解码超长拒绝");
    }
}

#[cfg(test)]
mod config_tests {
    use super::Config;

    const BASE: &str = r#"
listen_addr = "127.0.0.1:8791"
db_path = "memory.db"
migrations_dir = "migrations"
"#;

    #[test]
    fn auto_dream_defaults_to_enabled_and_can_be_disabled() {
        let default: Config = toml::from_str(BASE).unwrap();
        assert!(default.dream.enabled);

        let disabled: Config =
            toml::from_str(&format!("{BASE}\n[dream]\nenabled = false\n")).unwrap();
        assert!(!disabled.dream.enabled);
    }

    #[test]
    fn semantic_similarity_floor_defaults_and_validates_range() {
        let default: Config = toml::from_str(BASE).unwrap();
        assert_eq!(default.semantic_min_similarity, None);
        assert!(default.validate().is_ok());

        for floor in [0.0, 0.3, 1.0] {
            let configured: Config =
                toml::from_str(&format!("{BASE}\nsemantic_min_similarity = {floor}\n")).unwrap();
            assert!(
                configured.validate().is_ok(),
                "floor {floor} should be valid"
            );
        }

        for floor in [-0.1, 1.1] {
            let configured: Config =
                toml::from_str(&format!("{BASE}\nsemantic_min_similarity = {floor}\n")).unwrap();
            assert!(
                configured.validate().is_err(),
                "floor {floor} should be rejected"
            );
        }

        let non_finite: Config =
            toml::from_str(&format!("{BASE}\nsemantic_min_similarity = nan\n")).unwrap();
        assert!(
            non_finite.validate().is_err(),
            "non-finite floor should be rejected"
        );
    }
}

#[cfg(test)]
mod dream_runner_config_tests {
    use super::*;
    use axum::body::to_bytes;
    use memory_domain::Origin;

    #[tokio::test]
    async fn disabling_auto_dream_does_not_block_an_explicit_manual_job() {
        let disabled: Config = toml::from_str(
            r#"
listen_addr = "127.0.0.1:8791"
db_path = "memory.db"
migrations_dir = "migrations"
[dream]
enabled = false
"#,
        )
        .unwrap();
        assert!(!disabled.dream.enabled);

        let mut store =
            Store::open_in_memory(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations"))
                .unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "agent-memory-manual-dream-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let token_path = temp_dir.join("principal.token");
        store
            .principal_add("test-tenant", "test-user", &token_path)
            .unwrap();
        let token = std::fs::read_to_string(&token_path).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        let origin = Origin {
            host_id: "test-host".into(),
            agent_id: "test-agent".into(),
            session_id: "test-session".into(),
        };
        let occurred_at = chrono::Utc::now();
        store
            .record_evidence(
                &scope,
                &origin,
                1,
                "user",
                "user",
                &occurred_at,
                "仅用于本地测试的手动整理事件",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap();
        store
            .dream_trigger(
                &scope,
                "manual",
                "manual-trigger-test",
                Some("test-agent"),
                Some("test-host"),
                Some("test-session"),
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
            .unwrap();
        store
            .dream_runner_heartbeat(
                &scope,
                "test-runner",
                "test-host",
                "test-agent",
                r#"["chat","dream_scoped_read_v1"]"#,
                120,
            )
            .unwrap();

        let embedding = embedding::EmbeddingClient::new(embedding::EmbeddingConfig {
            endpoint: "http://127.0.0.1:1/v1/embeddings".into(),
            model: "local-test-only".into(),
            api_key: "local-test-only".into(),
            timeout: std::time::Duration::from_secs(1),
            dimensions: 1,
        })
        .unwrap();
        let state = AppState {
            store: Arc::new(Mutex::new(store)),
            embedding: Some(Arc::new(embedding)),
            rerank: None,
            semantic_min_similarity: super::DEFAULT_SEMANTIC_MIN_SIMILARITY,
            recency_mode: "linear",
            domains_enabled: false,
        };
        let response = dream_runner_claim(
            State(state.clone()),
            Extension(scope.clone()),
            Extension(RequestId("test-request".into())),
            Extension(DomainCtx::main_only()),
            Ok(Json(DreamRunnerClaimRequest {
                runner_id: "test-runner".into(),
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "claimed");
        assert_eq!(value["work"]["phase"], "extract");
        let dream_job_id = value["work"]["dream_job_id"].as_str().unwrap().to_string();
        let dream_generation = value["work"]["dream_generation"].as_i64().unwrap();

        let failure = dream_runner_failure(
            State(state.clone()),
            Extension(scope.clone()),
            Extension(RequestId("test-failure".into())),
            Ok(Json(DreamRunnerFailureRequest {
                runner_id: "test-runner".into(),
                phase: "extract".into(),
                dream_job_id: dream_job_id.clone(),
                dream_generation,
                adjudication_job_id: None,
                adjudication_generation: None,
                consolidation_job_id: None,
                consolidation_generation: None,
                error_code: "SUBAGENT_FAILED".into(),
            })),
        )
        .await;
        assert_eq!(failure.status(), StatusCode::OK);
        let job = state
            .store
            .lock()
            .unwrap()
            .dream_get(&scope, &dream_job_id)
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "provider_wait");
        assert_eq!(job.error_code.as_deref(), Some("SUBAGENT_FAILED"));
        let retry_at = chrono::DateTime::parse_from_rfc3339(&job.run_after).unwrap();
        assert!(retry_at > chrono::Utc::now() + chrono::Duration::hours(23));
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
