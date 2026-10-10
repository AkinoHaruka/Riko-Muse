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
    /// Provider returned an HTTP error. Persist only this status code, never its body.
    HttpStatus(u16),
    /// 提供方返回维度与配置不符。
    DimensionMismatch {
        expected: usize,
        actual: usize,
    },
    /// 向量含非有限值或空。
    BadVector,
    Transport,
    BadJson,
}

impl std::fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbeddingError::Timeout => write!(f, "embedding 端点超时"),
            EmbeddingError::RateLimited => write!(f, "embedding 端点 429"),
            EmbeddingError::HttpStatus(status) => {
                write!(f, "embedding 端点返回 HTTP {status}")
            }
            EmbeddingError::DimensionMismatch { expected, actual } => {
                write!(f, "向量维度不符：期望 {expected} 实际 {actual}")
            }
            EmbeddingError::BadVector => write!(f, "向量含非有限值或为空"),
            EmbeddingError::Transport => write!(f, "embedding 端点传输失败"),
            EmbeddingError::BadJson => write!(f, "embedding 响应解析失败"),
        }
    }
}

impl EmbeddingError {
    /// Stable, safe diagnostic suitable for persistence. Never includes provider
    /// response bodies, endpoint URLs, request headers, or credentials.
    pub fn safe_error_code(&self) -> String {
        match self {
            Self::Timeout => "EMBED_TIMEOUT".into(),
            Self::RateLimited => "EMBED_HTTP_429".into(),
            Self::HttpStatus(status) => format!("EMBED_HTTP_{status}"),
            Self::DimensionMismatch { .. } => "EMBED_DIMENSION_MISMATCH".into(),
            Self::BadVector => "EMBED_BAD_VECTOR".into(),
            Self::Transport => "EMBED_TRANSPORT".into(),
            Self::BadJson => "EMBED_BAD_RESPONSE".into(),
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
    /// OpenAI requires an index, while Google's OpenAI-compatible embeddings
    /// endpoint currently omits it. If every item omits the field, the client
    /// maps items by response order after checking the result count.
    #[serde(default, deserialize_with = "deserialize_index")]
    index: EmbeddingIndex,
    embedding: Vec<serde_json::Value>,
}

enum EmbeddingIndex {
    Missing,
    Present(usize),
}

impl Default for EmbeddingIndex {
    fn default() -> Self {
        Self::Missing
    }
}

fn deserialize_index<'de, D>(deserializer: D) -> Result<EmbeddingIndex, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let index = value
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            serde::de::Error::custom("embedding index must be a non-negative integer")
        })?;
    Ok(EmbeddingIndex::Present(index))
}

enum BoundedReadError {
    Timeout,
    Transport,
    TooLarge,
}

fn append_bounded(raw: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), BoundedReadError> {
    if raw.len().saturating_add(chunk.len()) > limit {
        return Err(BoundedReadError::TooLarge);
    }
    raw.extend_from_slice(chunk);
    Ok(())
}

async fn read_bounded_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, BoundedReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(BoundedReadError::TooLarge);
    }

    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            BoundedReadError::Timeout
        } else {
            BoundedReadError::Transport
        }
    })? {
        append_bounded(&mut raw, &chunk, MAX_RESPONSE_BYTES)?;
    }
    Ok(raw)
}

fn decode_embeddings_response(
    raw: &[u8],
    expected_count: usize,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    let parsed: EmbeddingsResponse =
        serde_json::from_slice(raw).map_err(|_| EmbeddingError::BadJson)?;
    if parsed.data.len() != expected_count {
        return Err(EmbeddingError::BadJson);
    }

    let indexed_count = parsed
        .data
        .iter()
        .filter(|item| matches!(item.index, EmbeddingIndex::Present(_)))
        .count();
    if indexed_count != 0 && indexed_count != parsed.data.len() {
        return Err(EmbeddingError::BadJson);
    }

    let mut out: Vec<Option<Vec<f32>>> = vec![None; expected_count];
    for (position, item) in parsed.data.into_iter().enumerate() {
        let index = match item.index {
            EmbeddingIndex::Missing => position,
            EmbeddingIndex::Present(index) => index,
        };
        if index >= expected_count || out[index].is_some() {
            return Err(EmbeddingError::BadJson);
        }
        let mut vector = Vec::with_capacity(item.embedding.len());
        for value in item.embedding {
            let number = value.as_f64().ok_or(EmbeddingError::BadVector)?;
            if !number.is_finite() {
                return Err(EmbeddingError::BadVector);
            }
            let number = number as f32;
            if !number.is_finite() {
                return Err(EmbeddingError::BadVector);
            }
            vector.push(number);
        }
        if vector.len() != dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: dimensions,
                actual: vector.len(),
            });
        }
        out[index] = Some(vector);
    }

    out.into_iter()
        .map(|vector| vector.ok_or(EmbeddingError::BadJson))
        .collect()
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
                    EmbeddingError::Transport
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
            return Err(EmbeddingError::HttpStatus(status.as_u16()));
        }
        let raw = read_bounded_response(resp)
            .await
            .map_err(|error| match error {
                BoundedReadError::Timeout => EmbeddingError::Timeout,
                BoundedReadError::Transport | BoundedReadError::TooLarge => {
                    EmbeddingError::Transport
                }
            })?;
        decode_embeddings_response(&raw, texts.len(), self.cfg.dimensions)
    }
}

#[cfg(test)]
mod tests {
    use super::{append_bounded, decode_embeddings_response, EmbeddingError};

    #[test]
    fn accepts_google_compatible_embeddings_without_indexes_in_response_order() {
        let raw = br#"{"data":[{"object":"embedding","embedding":[0.1,0.2]},{"object":"embedding","embedding":[0.3,0.4]}]}"#;
        assert_eq!(
            decode_embeddings_response(raw, 2, 2).unwrap(),
            vec![vec![0.1, 0.2], vec![0.3, 0.4]],
        );
    }

    #[test]
    fn preserves_indexed_embedding_order_and_rejects_mixed_indexes() {
        let indexed =
            br#"{"data":[{"index":1,"embedding":[0.3,0.4]},{"index":0,"embedding":[0.1,0.2]}]}"#;
        assert_eq!(
            decode_embeddings_response(indexed, 2, 2).unwrap(),
            vec![vec![0.1, 0.2], vec![0.3, 0.4]],
        );

        let mixed = br#"{"data":[{"index":0,"embedding":[0.1,0.2]},{"embedding":[0.3,0.4]}]}"#;
        assert!(matches!(
            decode_embeddings_response(mixed, 2, 2),
            Err(EmbeddingError::BadJson)
        ));

        let explicit_null = br#"{"data":[{"index":null,"embedding":[0.1,0.2]}]}"#;
        assert!(matches!(
            decode_embeddings_response(explicit_null, 1, 2),
            Err(EmbeddingError::BadJson)
        ));
    }

    #[test]
    fn rejects_values_that_overflow_f32() {
        let raw = br#"{"data":[{"embedding":[1e39]}]}"#;
        assert!(matches!(
            decode_embeddings_response(raw, 1, 1),
            Err(EmbeddingError::BadVector)
        ));
    }

    #[test]
    fn bounded_body_append_rejects_oversized_chunks_without_appending_them() {
        let mut body = vec![1, 2, 3];
        assert!(append_bounded(&mut body, &[4, 5], 4).is_err());
        assert_eq!(body, vec![1, 2, 3]);
    }

    #[test]
    fn safe_error_codes_preserve_http_class_without_sensitive_details() {
        assert_eq!(
            EmbeddingError::HttpStatus(401).safe_error_code(),
            "EMBED_HTTP_401"
        );
        assert_eq!(
            EmbeddingError::RateLimited.safe_error_code(),
            "EMBED_HTTP_429"
        );
        assert_eq!(
            EmbeddingError::Transport.safe_error_code(),
            "EMBED_TRANSPORT"
        );
        assert_eq!(
            EmbeddingError::Transport.to_string(),
            "embedding 端点传输失败"
        );
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
        let raw = read_bounded_response(resp)
            .await
            .map_err(|error| match error {
                BoundedReadError::Timeout => "rerank 响应读取超时".to_string(),
                BoundedReadError::Transport => "读取 rerank 响应失败".to_string(),
                BoundedReadError::TooLarge => {
                    format!("rerank 响应超过 {MAX_RESPONSE_BYTES} 字节上限")
                }
            })?;
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
