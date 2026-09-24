//! 协议常量、错误码、通用 envelope 与 v1 版本化默认配置（doc/10 B 节、doc/12）。
//! 本 crate 无业务依赖。

use serde::Serialize;

/// 协议版本：HTTP v1 语义不变（doc/12）。DSH 适配器握手依据。
pub const PROTOCOL_VERSION: u32 = 1;
/// 数据库模式版本：随迁移文件递增（0001→1，0002→2）。worker 按作业行 prompt_version 选规则。
pub const SCHEMA_VERSION: u32 = 2;

/// 提取 Prompt 版本（doc/13 §4），随任务保存。
pub const EXTRACT_PROMPT_VERSION: &str = "extract_v1";

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
}
