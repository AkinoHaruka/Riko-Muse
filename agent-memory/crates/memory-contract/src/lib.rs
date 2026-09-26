//! 协议常量、错误码、通用 envelope 与 v1 版本化默认配置（doc/10 B 节、doc/12）。
//! 本 crate 无业务依赖。

use serde::Serialize;

/// 协议版本：HTTP v1 语义不变（doc/12）。DSH 适配器握手依据。
pub const PROTOCOL_VERSION: u32 = 1;
/// 数据库模式版本：随迁移文件递增（0001→1，…，0004→4，0005→5，0006→6，
/// 0007→7 Dream，0008→8 语义索引，0009→9 语义裁决，0010→10 生命周期治理）。worker 按作业行
/// prompt_version/admission_version 分别选提示词与准入规则。0005 起新增表
/// （doc6/02）：0005 soul/resident/audit/receipts；0006 派生知识文档、问题目录、
/// 整理作业与页面索引；历史迁移 0001—0004 冻结不改。
pub const SCHEMA_VERSION: u32 = 11;

/// 提取 Prompt 版本（doc/13 §4），随任务保存。新建作业一律写当前版本。
/// doc2/05 §3：更新 Prompt 必须新建版本并保留老版本处理未完成作业；
/// V1/V2 常量仅为按版本分派历史作业而保留（0002 迁移的列默认值同为 extract_v1）。
/// doc5/03 §3：v3 Prompt、v2 准入与 worker 分派全部就绪后，新作业默认值在同一
/// 提交切换为 extract_v3（自该提交起新建作业写 extract_v3）。
pub const EXTRACT_PROMPT_VERSION_V1: &str = "extract_v1";
pub const EXTRACT_PROMPT_VERSION_V2: &str = "extract_v2";
pub const EXTRACT_PROMPT_VERSION: &str = "extract_v3";
pub const EXTRACT_PROMPT_VERSION_V3: &str = "extract_v3";

/// 准入规则版本（doc5/03 §1）：admission_version 选 Rust 准入及查库提交规则，
/// 与 prompt_version（只选模型提示词）分工明确。0004 迁移的列默认值为 admit_v1
/// （历史作业回填）；v3 Prompt 与 v2 准入就绪后，新作业默认值在同一提交切换为
/// admit_v2，与 EXTRACT_PROMPT_VERSION 的切换同步。
pub const ADMISSION_VERSION_V1: &str = "admit_v1";
pub const ADMISSION_VERSION: &str = "admit_v2";
pub const ADMISSION_VERSION_V2: &str = "admit_v2";

/// D6-8（doc6/09 §3）：新 Dream 作业专用准入/裁决版本；admit_v1/v2 冻结不改。
pub const ADMISSION_VERSION_V3: &str = "admit_v3";
pub const ADJUDICATION_VERSION_V1: &str = "adjudicate_v1";

// ---- v1 版本化默认限额（doc/10 D-08、doc/13 §3/§6；统一在此，不散落硬编码）----
/// 自动上下文默认最多 5 条原子记忆。
pub const COMPOSE_MAX_ITEMS_DEFAULT: usize = 5;
/// 自动上下文默认最多 2000 个 Unicode 标量字符。
pub const COMPOSE_MAX_CHARS_DEFAULT: usize = 2000;
/// compose 单次检索默认超时 500 ms。
pub const COMPOSE_TIMEOUT_MS_DEFAULT: u64 = 500;
/// 事件正文上限 64 KiB UTF-8（doc/12 §3）。
pub const EVIDENCE_CONTENT_MAX_BYTES: usize = 64 * 1024;
/// 提取窗口最大 100 个事件。
pub const EXTRACTION_WINDOW_MAX_EVENTS: usize = 100;
/// 提取输入文本上限 32 KiB。
pub const EXTRACTION_INPUT_MAX_BYTES: usize = 32 * 1024;
/// 搜索 query 上限 2048 个 Unicode 标量字符。
pub const SEARCH_QUERY_MAX_CHARS: usize = 2048;
/// 每次模型响应最多 20 条候选。
pub const MAX_CANDIDATES_PER_RESPONSE: usize = 20;
/// quote 长度 1～512 个 Unicode 标量字符。
pub const QUOTE_MAX_CHARS: usize = 512;
/// 模型调用 timeout 30 s。
pub const MODEL_CALL_TIMEOUT_SECS: u64 = 30;
/// worker lease 90 s。
pub const JOB_LEASE_SECS: u64 = 90;
/// worker 处理期间心跳续租间隔 30 s（doc4/02 §4：模型 timeout 可配置超过 90 s，
/// 执行中必须续租；续租只短暂持锁，模型网络调用绝不持锁）。
pub const JOB_HEARTBEAT_SECS: u64 = 30;
/// 最大 3 次调用。
pub const JOB_MAX_ATTEMPTS: u32 = 3;
/// 重试延迟序列 5/15/45 s（doc/13 §3，取自 TencentDB 思想）。
pub const JOB_RETRY_DELAYS_SECS: [u64; 3] = [5, 15, 45];
/// 1 字短查询的有界子串匹配范围：最近 500 条 active。
pub const SINGLE_CHAR_SCAN_LIMIT: usize = 500;
/// FTS / gram 各路取最多 100 个 ID（doc/13 §6）。
pub const SEARCH_PER_CHANNEL_LIMIT: usize = 100;
/// RRF 融合常数 k=60（doc/13 §6，取自 Hindsight 实现）。
pub const RRF_K: f64 = 60.0;

// ---- D6-8 语义支路与裁决（doc6/04 §2、doc6/09）----
/// 查询向量支路单 scope 就绪向量扫描上限；超过报 limit_exceeded 只做词法。
pub const SEMANTIC_SCAN_LIMIT: usize = 10000;
/// 实时 query embedding 超时（doc6/04 §2）。
pub const QUERY_EMBEDDING_TIMEOUT_MS: u64 = 800;
/// 裁决候选召回的混合 top-K（doc6/09 §4.2 初值）。
pub const ADJUDICATE_RECALL_TOP_K: usize = 20;
/// 旧 Held candidate 进入新裁决的最低新证据 claim cosine 相关度。
pub const HELD_REDECISION_MIN_COSINE: f32 = 0.78;
/// recency 因子（doc6/04 §3.1 Hindsight 初值，仅 episode Retrieved 排序）。
pub const RECENCY_MODE_DEFAULT: &str = "linear";
pub const RECENCY_HALFLIFE_DAYS: f64 = 90.0;
pub const RECENCY_FRESHNESS_MIN: f64 = 0.1;
pub const RECENCY_SCALE: f64 = 0.2;
/// episode recency 越级界限：base relevance 相差 ≥20% 时低分不得靠 recency 越级。
pub const RECENCY_OVERRIDE_RATIO: f64 = 1.20;
/// 宿主 ID 最大长度。
pub const HOST_ID_MAX_CHARS: usize = 256;

/// v1 错误码全集（doc/12 §8）。客户端只判断 `code`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidJson,
    InvalidField,
    Unauthenticated,
    Forbidden,
    NotFound,
    BodyTooLarge,
    EventConflict,
    QuoteMismatch,
    StaleUserEvidence,
    VersionConflict,
    StateConflict,
    AmbiguousTarget,
    /// 同一幂等键曾以不同请求体使用（doc6/02 §2；409）
    IdempotencyConflict,
    ModelUnavailable,
    IndexDegraded,
    RateLimited,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidJson => "INVALID_JSON",
            ErrorCode::InvalidField => "INVALID_FIELD",
            ErrorCode::Unauthenticated => "UNAUTHENTICATED",
            ErrorCode::Forbidden => "FORBIDDEN",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::BodyTooLarge => "BODY_TOO_LARGE",
            ErrorCode::EventConflict => "EVENT_CONFLICT",
            ErrorCode::QuoteMismatch => "QUOTE_MISMATCH",
            ErrorCode::StaleUserEvidence => "STALE_USER_EVIDENCE",
            ErrorCode::VersionConflict => "VERSION_CONFLICT",
            ErrorCode::StateConflict => "STATE_CONFLICT",
            ErrorCode::AmbiguousTarget => "AMBIGUOUS_TARGET",
            ErrorCode::IdempotencyConflict => "IDEMPOTENCY_CONFLICT",
            ErrorCode::ModelUnavailable => "MODEL_UNAVAILABLE",
            ErrorCode::IndexDegraded => "INDEX_DEGRADED",
            ErrorCode::RateLimited => "RATE_LIMITED",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

/// 失败响应 envelope（doc/12 §1）。`message` 只供诊断。
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub request_id: String,
    pub error: ApiErrorBody,
}

impl ErrorResponse {
    pub fn new(request_id: impl Into<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorResponse {
            request_id: request_id.into(),
            error: ApiErrorBody {
                code: code.as_str(),
                message: message.into(),
            },
        }
    }
}

/// 成功响应 envelope 的公共字段由端点各自定义，均含 request_id。
#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub protocol_version: u32,
    pub db: &'static str,
    pub index: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct VersionResponse {
    pub protocol_version: u32,
    pub schema_version: u32,
    pub build: &'static str,
    /// D6 能力握手（doc6/06 §1）：老客户端忽略未知字段；适配器按能力启用新注入。
    pub capabilities: Vec<&'static str>,
}

/// 已交付能力（doc6/06 §1）：D6-1/D6-2 soul 存储与接口；D6-3 resident/bundle。
pub const CAPABILITY_SOUL_V1: &str = "soul_v1";
pub const CAPABILITY_CONTEXT_BUNDLE_V1: &str = "context_bundle_v1";
