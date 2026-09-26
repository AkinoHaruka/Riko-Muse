//! D6-8 embedding / reranker 客户端（doc6/04 §2、doc6/09 §6）。
//!
//! embedding 是单独配置的 OpenAI 兼容 `/embeddings` 端点（完整 URL，不猜路径）；
//! reranker 须单独配置专用 rerank 端点，不得以生成式 LLM 调用冒充 cross-encoder。
//! 两个客户端共用 worker 的密钥纪律：密钥从文件读取，不进日志、不进 Prompt。

use serde::Deserialize;

/// 响应正文上限（有界读取，防异常大包）。
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    /// 完整 embeddings endpoint URL（http/https）。
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub timeout: std::time::Duration,
    /// 预期维度（doc6/04 §2：维度由配置明确给出；响应维度不符即失败）。
    pub dimensions: usize,
}

/// embedding 客户端错误（doc6/04 §2 降级语义的上游来源）。
#[derive(Debug)]
pub enum EmbeddingError {
    Timeout,
    RateLimited,
    /// 提供方返回维度与配置不符。
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    /// 向量含非有限值或空。
    BadVector,
    Transport(String),
    BadJson(String),
}

impl std::fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbeddingError::Timeout => write!(f, "embedding 端点超时"),
            EmbeddingError::RateLimited => write!(f, "embedding 端点 429"),
            EmbeddingError::DimensionMismatch { expected, actual } => {
                write!(f, "向量维度不符：期望 {expected} 实际 {actual}")
            }
            EmbeddingError::BadVector => write!(f, "向量含非有限值或为空"),
            EmbeddingError::Transport(e) => write!(f, "embedding 端点请求失败: {e}"),
            EmbeddingError::BadJson(e) => write!(f, "embedding 响应解析失败: {e}"),
        }
    }
}

pub struct EmbeddingClient {
    cfg: EmbeddingConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingsItem>,
}

#[derive(Deserialize)]
struct EmbeddingsItem {
    index: usize,
    embedding: Vec<serde_json::Value>,
}

impl EmbeddingClient {
    pub fn new(cfg: EmbeddingConfig) -> Result<Self, String> {
        let url = reqwest::Url::parse(&cfg.endpoint)
            .map_err(|e| format!("embedding_endpoint 不是合法 URL: {e}"))?;
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(format!(
                    "embedding_endpoint scheme 必须是 http/https，实际 {other}"
                ))
            }
        }
        if url.host_str().is_none() || url.path().len() <= 1 {
            return Err("embedding_endpoint 必须包含主机与具体路径（完整 embeddings URL）".into());
        }
        if cfg.dimensions == 0 {
            return Err("embedding_dimensions 必须为正整数".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.timeout)
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("构建 embedding HTTP 客户端失败: {e}"))?;
        Ok(Self { cfg, http })
    }

    pub fn model_id(&self) -> &str {
        &self.cfg.model
    }

    /// 批量 embedding（OpenAI 兼容 `/embeddings`）。校验：index 顺序完整、维度与
    /// 配置一致、全部分量为有限数。失败时整体报错，不返回部分向量。
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "model": self.cfg.model,
            "input": texts,
        });
        let resp = self
            .http
            .post(&self.cfg.endpoint)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    EmbeddingError::Timeout
                } else if e.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS) {
                    EmbeddingError::RateLimited
                } else {
                    EmbeddingError::Transport(format!("{e}"))
                }
            })?;
        let status = resp.status();
        if status.as_u16() == 408 || status.as_u16() == 504 {
            return Err(EmbeddingError::Timeout);
        }
        if status.as_u16() == 429 {
            return Err(EmbeddingError::RateLimited);
        }
        if !status.is_success() {
            return Err(EmbeddingError::Transport(format!("端点返回 {status}")));
        }
        let raw = resp
            .bytes()
            .await
            .map_err(|e| EmbeddingError::Transport(format!("读取响应失败: {e}")))?;
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(EmbeddingError::Transport(format!(
                "embedding 响应超过 {MAX_RESPONSE_BYTES} 字节上限"
            )));
        }
        let parsed: EmbeddingsResponse =
            serde_json::from_slice(&raw).map_err(|e| EmbeddingError::BadJson(e.to_string()))?;
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for item in parsed.data {
            if item.index >= texts.len() {
                return Err(EmbeddingError::BadJson(format!(
                    "index {} 越界",
                    item.index
                )));
            }
            let mut v = Vec::with_capacity(item.embedding.len());
            for x in item.embedding {
                let n = x.as_f64().ok_or_else(|| EmbeddingError::BadVector)?;
                if !n.is_finite() {
                    return Err(EmbeddingError::BadVector);
                }
                v.push(n as f32);
            }
            if v.len() != self.cfg.dimensions {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.cfg.dimensions,
                    actual: v.len(),
                });
            }
            out[item.index] = Some(v);
        }
        out.into_iter()
            .map(|v| v.ok_or(EmbeddingError::BadJson("index 缺失".into())))
            .collect()
    }
}

/// reranker 配置（doc6/04 §2：单独配置 rerank 端点；未配置时 rerank_status=disabled）。
#[derive(Debug, Clone)]
pub struct RerankConfig {
    /// 完整 rerank endpoint URL（Jina/Cohere 兼容 `{model, query, documents, top_n}`）。
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub timeout: std::time::Duration,
}

pub struct RerankClient {
    cfg: RerankConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct RerankResponse {
    results: Vec<RerankItem>,
}

#[derive(Deserialize)]
struct RerankItem {
    index: usize,
    relevance_score: f64,
}

impl RerankClient {
    pub fn new(cfg: RerankConfig) -> Result<Self, String> {
        let url = reqwest::Url::parse(&cfg.endpoint)
            .map_err(|e| format!("rerank_endpoint 不是合法 URL: {e}"))?;
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(format!(
                    "rerank_endpoint scheme 必须是 http/https，实际 {other}"
                ))
            }
        }
        if url.host_str().is_none() || url.path().len() <= 1 {
            return Err("rerank_endpoint 必须包含主机与具体路径（完整 rerank URL）".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.timeout)
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("构建 rerank HTTP 客户端失败: {e}"))?;
        Ok(Self { cfg, http })
    }

    /// 对 (query, candidates) 配对重排（Hindsight cross-encoder 后置精排）。
    /// 返回按 relevance_score 降序的 (candidate_index, score)。
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f64)>, String> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "model": self.cfg.model,
            "query": query,
            "documents": documents,
            "top_n": documents.len().min(top_n),
        });
        let resp = self
            .http
            .post(&self.cfg.endpoint)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("rerank 端点请求失败: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("rerank 端点返回 {status}"));
        }
        let raw = resp
            .bytes()
            .await
            .map_err(|e| format!("读取 rerank 响应失败: {e}"))?;
        if raw.len() > MAX_RESPONSE_BYTES {
            return Err(format!("rerank 响应超过 {MAX_RESPONSE_BYTES} 字节上限"));
        }
        let parsed: RerankResponse =
            serde_json::from_slice(&raw).map_err(|e| format!("rerank 响应解析失败: {e}"))?;
        let mut out: Vec<(usize, f64)> = parsed
            .results
            .into_iter()
            .filter(|r| r.index < documents.len() && r.relevance_score.is_finite())
            .map(|r| (r.index, r.relevance_score))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }
}
