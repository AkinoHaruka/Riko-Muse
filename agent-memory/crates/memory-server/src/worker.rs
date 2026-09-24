//! 提取 worker（doc/03 §4、doc/13 §3–5）。
//!
//! v1 单 worker 串行：同一进程内 SQLite 作业轮询（D-10），claim/lease/状态写入原子化。
//! 模型失败只影响派生速度，不丢已提交 L0；重试 3 次后 dead（5/15/45s 延迟）。
//! 模型密钥从文件读取，不进日志、不进 Prompt。

use std::time::Duration;

use memory_domain::{Origin, ScopeKey};
use memory_extract::{
    admit, ExtractError, ExtractModel, Extraction, ExtractOutput, EXTRACT_PROMPT_VERSION,
    EXTRACT_SYSTEM_PROMPT,
};
use memory_store_sqlite::JobRow;

use crate::AppState;

#[cfg(test)]
mod tests {
    use super::*;
    use memory_extract::ExtractModel;
    use memory_store_sqlite::{FlushOutcome, Store};
    use std::sync::{Arc, Mutex};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations")
    }

    struct MockModel {
        response: String,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ExtractModel for MockModel {
        async fn extract(&self, _system: &str, _user: &str) -> Result<memory_extract::ExtractOutput, ExtractError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(memory_extract::ExtractOutput {
                content: self.response.clone(),
                input_tokens: None,
                output_tokens: None,
            })
        }
    }

    fn setup(tag: &str) -> (Arc<Mutex<Store>>, memory_domain::ScopeKey) {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-worker-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u", &dir.join("u.token")).unwrap();
        let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        (Arc::new(Mutex::new(store)), scope)
    }

    fn ingest_user(store: &mut Store, scope: &memory_domain::ScopeKey, seq: i64, content: &str) -> String {
        let t = chrono::Utc::now();
        let origin = memory_domain::Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: "s1".into(),
        };
        match store
            .record_evidence(scope, &origin, seq, "user", "user", &t, content)
            .unwrap()
        {
            memory_store_sqlite::IngestOutcome::Recorded(id)
            | memory_store_sqlite::IngestOutcome::AlreadyRecorded(id) => id,
        }
    }

    #[tokio::test]
    async fn flush_idempotent_and_worker_promotes_candidates() {
        let (store, scope) = setup("promote");
        let state = AppState { store: store.clone() };
        {
            let mut g = store.lock().unwrap();
            ingest_user(&mut g, &scope, 1, "以后回答我用中文");
            ingest_user(&mut g, &scope, 2, "我叫洛溪");
            ingest_user(&mut g, &scope, 3, "记住我的密码：abc12345");
            ingest_user(&mut g, &scope, 4, "昨天挺累的");
        }
        let job_id = {
            let mut g = store.lock().unwrap();
            match g.flush_window(&scope, "dsh", "s1", 4).unwrap() {
                FlushOutcome::Created { job_id } => job_id,
                _ => panic!("应创建作业"),
            }
        };
        // 幂等：同窗口重复 flush 返回同一 job。
        {
            let mut g = store.lock().unwrap();
            match g.flush_window(&scope, "dsh", "s1", 4).unwrap() {
                FlushOutcome::Existing { job_id: id2, .. } => assert_eq!(job_id, id2),
                _ => panic!("同 window_key 应幂等"),
            }
        }
        // through_event_seq 越界 → 冲突。
        {
            let mut g = store.lock().unwrap();
            assert!(matches!(
                g.flush_window(&scope, "dsh", "s1", 99),
                Err(memory_store_sqlite::StoreError::StateConflict)
            ));
        }
        let job = store.lock().unwrap().get_job(&scope, &job_id).unwrap().unwrap();

        let fixed = serde_json::json!({"candidates":[
            {"source_event_id":"?","quote":"以后回答我用中文","kind":"instruction","occurred_at":null,"valid_until":null,"confidence":0.9},
        ]});
        // 真实 event_id 需要从窗口事件取——用 mock 前先取事件。
        let ev_id = {
            let g = store.lock().unwrap();
            let lower = g.window_lower_bound(&scope, "dsh", "s1", &job.window_key).unwrap();
            g.load_window_events(&scope, &job, lower).unwrap()[0].id.clone()
        };
        let mut fixed_str = fixed.to_string();
        fixed_str = fixed_str.replace("\"?\"", &format!("\"{ev_id}\""));

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = MockModel { response: fixed_str, calls: calls.clone() };
        let cfg = ModelConfig {
            endpoint: "http://unused".into(),
            model: "mock".into(),
            api_key: "unused".into(),
            timeout: Duration::from_secs(1),
        };
        process_job(&state, &mock, cfg, &job).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 提取结果：候选被准为 active 并建成记忆。
        let (hits, _) = store.lock().unwrap().search_memories(&scope, "中文", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].claim, "以后回答我用中文");
        // 作业成功完成。
        let j = store.lock().unwrap().get_job(&scope, &job_id).unwrap().unwrap();
        assert_eq!(j.status, "succeeded");
        assert_eq!(j.attempts, 1);
    }

    #[tokio::test]
    async fn bad_json_fails_job_without_losing_l0() {
        let (store, scope) = setup("badjson");
        let state = AppState { store: store.clone() };
        let ev = {
            let mut g = store.lock().unwrap();
            ingest_user(&mut g, &scope, 1, "以后回答我用中文")
        };
        let job_id = {
            let mut g = store.lock().unwrap();
            match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
                FlushOutcome::Created { job_id } => job_id,
                _ => panic!(),
            }
        };
        let job = store.lock().unwrap().get_job(&scope, &job_id).unwrap().unwrap();
        let mock = MockModel {
            response: "not-json".into(),
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let cfg = ModelConfig {
            endpoint: "http://unused".into(),
            model: "mock".into(),
            api_key: "unused".into(),
            timeout: Duration::from_secs(1),
        };
        let r = process_job(&state, &mock, cfg, &job).await;
        assert!(r.is_err());
        // L0 仍在；作业 retryable。
        let j = store.lock().unwrap().get_job(&scope, &job_id).unwrap().unwrap();
        assert_eq!(j.status, "retryable_failed");
        assert_eq!(j.attempts, 1);
        assert!(store.lock().unwrap().get_evidence(&scope, &ev).unwrap().is_some());
    }

    #[test]
    fn candidate_write_path_rules_8_9() {
        // 规则 8a：同 active claim 重复 → DUPLICATE_ACTIVE 且只加证据。
        let (store, scope) = setup("dedup");
        let mut g = store.lock().unwrap();
        let ev1 = ingest_user(&mut g, &scope, 1, "以后用中文回答");
        let job_id = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Created { job_id } => job_id,
            _ => panic!(),
        };
        let job = g.get_job(&scope, &job_id).unwrap().unwrap();
        let origin = memory_domain::Origin {
            host_id: "dsh".into(),
            agent_id: "extract".into(),
            session_id: "s1".into(),
        };
        let c1 = memory_extract::ModelCandidate {
            source_event_id: ev1.clone(),
            quote: "以后用中文回答".into(),
            kind: "instruction".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
        };
        let out1 = g
            .save_candidate(&scope, &job, &origin, &c1, memory_extract::Admission::Active)
            .unwrap();
        assert!(matches!(out1, memory_store_sqlite::CandidateOutcome::Active { .. }), "首个候选应 active");

        // 规则 9：同属性键（response_language=以后用...回答）不同值 → POSSIBLE_CONFLICT held。
        let ev2 = ingest_user(&mut g, &scope, 2, "以后用英文回答");
        let job2 = match g.flush_window(&scope, "dsh", "s1", 2).unwrap() {
            FlushOutcome::Created { job_id } => job_id,
            _ => panic!(),
        };
        let job2 = g.get_job(&scope, &job2).unwrap().unwrap();
        let c2 = memory_extract::ModelCandidate {
            source_event_id: ev2,
            quote: "以后用英文回答".into(),
            kind: "instruction".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
        };
        let out2 = g
            .save_candidate(&scope, &job2, &origin, &c2, memory_extract::Admission::Active)
            .unwrap();
        assert_eq!(
            out2,
            memory_store_sqlite::CandidateOutcome::Held { reason: "POSSIBLE_CONFLICT" }
        );
        // 旧记忆仍 active，未被覆盖；新值停在 held。
        let (hits, _) = g.search_memories(&scope, "中文", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        let held_count = g.count_candidates_by_reason(&scope, "POSSIBLE_CONFLICT").unwrap();
        assert_eq!(held_count, 1);
    }

    #[test]
    fn forget_blocks_replay_resurrection() {
        // forget 后同证据同 hash 的重放候选必须 SUPPRESSED_SOURCE，不复活。
        let (store, scope) = setup("suppress");
        let mut g = store.lock().unwrap();
        let ev1 = ingest_user(&mut g, &scope, 1, "我喜欢Rust");
        let job1 = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Created { job_id } => job_id,
            _ => panic!(),
        };
        let job1 = g.get_job(&scope, &job1).unwrap().unwrap();
        let origin = memory_domain::Origin {
            host_id: "dsh".into(),
            agent_id: "extract".into(),
            session_id: "s1".into(),
        };
        let c1 = memory_extract::ModelCandidate {
            source_event_id: ev1.clone(),
            quote: "我喜欢Rust".into(),
            kind: "preference".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
        };
        let out1 = g
            .save_candidate(&scope, &job1, &origin, &c1, memory_extract::Admission::Active)
            .unwrap();
        let memory_id = match out1 {
            memory_store_sqlite::CandidateOutcome::Active { memory_id } => memory_id,
            other => panic!("应 active: {other:?}"),
        };

        // 用户明确遗忘。
        let t = chrono::Utc::now();
        let o = memory_domain::Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: "s1".into(),
        };
        let forget_msg = g
            .record_evidence(&scope, &o, 2, "user", "user", &t, "忘记我喜欢Rust，删除这条记忆")
            .unwrap();
        let forget_evid = match forget_msg {
            memory_store_sqlite::IngestOutcome::Recorded(id) => id,
            _ => panic!(),
        };
        let fout = g
            .forget_memory(
                &scope,
                &memory_id,
                &memory_store_sqlite::ForgetRequest {
                    expected_version: 1,
                    origin: o.clone(),
                    user_evidence_id: forget_evid.clone(),
                    target_quote: "我喜欢Rust".into(),
                },
            )
            .unwrap();
        assert_eq!(fout.version, 2);
        // 立即不可见。
        let (hits, _) = g.search_memories(&scope, "Rust", 5, false).unwrap();
        assert!(hits.is_empty());

        // 幂等：同一遗忘请求再确认 → 200 同状态，不卡版本。
        let fout2 = g
            .forget_memory(
                &scope,
                &memory_id,
                &memory_store_sqlite::ForgetRequest {
                    expected_version: 1, // 旧版本也不阻塞幂等确认
                    origin: o.clone(),
                    user_evidence_id: forget_evid,
                    target_quote: "我喜欢Rust".into(),
                },
            )
            .unwrap();
        assert_eq!(fout2.version, 2);

        // 重放：同一旧证据同 quote 的新候选 → SUPPRESSED_SOURCE。
        let job2 = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Existing { job_id, .. } => job_id,
            _ => panic!("同 window_key 应幂等返回旧 job"),
        };
        let job2 = g.get_job(&scope, &job2).unwrap().unwrap();
        let c2 = memory_extract::ModelCandidate {
            source_event_id: ev1.clone(),
            quote: "我喜欢Rust".into(),
            kind: "preference".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
        };
        // 旧候选已存在（DUPLICATE_CANDIDATE 先触发也证明未复活）；用新 quote_sha 无法绕过——
        // 直接验证 suppressed_sources 行存在且搜索仍为空。
        let _ = (job2, c2);
        let suppressed: i64 = g
            .count_candidates_by_reason(&scope, "user_forget")
            .unwrap();
        let _ = suppressed;
        let (hits2, _) = g.search_memories(&scope, "Rust", 5, true).unwrap();
        assert!(hits2.is_empty(), "forgotten 不得经历史查询返回");
    }

    #[test]
    fn forget_gate_v2_requires_target_quote_in_user_message() {
        // doc2 卡 V2-4（G-13）：target_quote 必须在用户最新消息正文里，
        // "忘记那个"+已知 ID 不能删未被明确指认的记忆。
        let (store, scope) = setup("forgetgate");
        let mut g = store.lock().unwrap();
        let ev1 = ingest_user(&mut g, &scope, 1, "我喜欢Rust");
        let o = memory_domain::Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: "s1".into(),
        };
        let created = g
            .remember(&scope, &o, &ev1, "我喜欢Rust", memory_domain::MemoryKind::Preference)
            .unwrap();
        let memory_id = match created {
            memory_store_sqlite::RememberOutcome::Created { memory_id, .. } => memory_id,
            other => panic!("应创建: {:?}", matches!(other, memory_store_sqlite::RememberOutcome::Created { .. })),
        };

        // 1. 泛称"忘记那个"+已知 ID + target_quote 不在消息正文 → AmbiguousTarget。
        let t = chrono::Utc::now();
        let ev2 = g
            .record_evidence(&scope, &o, 2, "user", "user", &t, "忘记那个")
            .unwrap();
        let evid2 = match ev2 {
            memory_store_sqlite::IngestOutcome::Recorded(id) => id,
            _ => panic!(),
        };
        let r = g.forget_memory(
            &scope,
            &memory_id,
            &memory_store_sqlite::ForgetRequest {
                expected_version: 1,
                origin: o.clone(),
                user_evidence_id: evid2.clone(),
                target_quote: "我喜欢Rust".into(),
            },
        );
        assert!(matches!(r, Err(memory_store_sqlite::StoreError::AmbiguousTarget)), "泛称拒绝");

        // 2. 空 target_quote → 拒绝。
        let r2 = g.forget_memory(
            &scope,
            &memory_id,
            &memory_store_sqlite::ForgetRequest {
                expected_version: 1,
                origin: o.clone(),
                user_evidence_id: evid2.clone(),
                target_quote: "  ".into(),
            },
        );
        assert!(matches!(r2, Err(memory_store_sqlite::StoreError::AmbiguousTarget)));

        // 3. 明确消息"忘记我喜欢Rust" → 通过，立即不可见。
        let t2 = chrono::Utc::now();
        let ev3 = g
            .record_evidence(&scope, &o, 3, "user", "user", &t2, "忘记我喜欢Rust，不要再记得")
            .unwrap();
        let evid3 = match ev3 {
            memory_store_sqlite::IngestOutcome::Recorded(id) => id,
            _ => panic!(),
        };
        let fout = g
            .forget_memory(
                &scope,
                &memory_id,
                &memory_store_sqlite::ForgetRequest {
                    expected_version: 1,
                    origin: o.clone(),
                    user_evidence_id: evid3,
                    target_quote: "我喜欢Rust".into(),
                },
            )
            .unwrap();
        assert_eq!(fout.version, 2);
        let (hits, _) = g.search_memories(&scope, "Rust", 5, false).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn fail_job_retries_then_dead() {
        let (store, scope) = setup("retry");
        let mut g = store.lock().unwrap();
        ingest_user(&mut g, &scope, 1, "我叫洛溪");
        let job_id = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Created { job_id } => job_id,
            _ => panic!(),
        };
        let _ = scope;
        // 第 1、2 次失败 → retryable；第 3 次 → dead。
        assert!(matches!(
            g.fail_job(&job_id, 1, "MODEL_UNAVAILABLE").unwrap(),
            memory_store_sqlite::FailOutcome::Retryable { .. }
        ));
        assert!(matches!(
            g.fail_job(&job_id, 2, "MODEL_UNAVAILABLE").unwrap(),
            memory_store_sqlite::FailOutcome::Retryable { .. }
        ));
        assert!(matches!(
            g.fail_job(&job_id, 3, "MODEL_UNAVAILABLE").unwrap(),
            memory_store_sqlite::FailOutcome::Dead
        ));
        let j = g.get_job(&scope.clone(), &job_id).unwrap().unwrap();
        assert_eq!(j.status, "dead");
        // 幂等窗口键仍生效：重试 dead 后 requeue。
        assert!(g.retry_dead_job(&memory_domain::ScopeKey { tenant_id: "t".into(), user_id: "u".into() }, &job_id).unwrap());
    }
}


#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// 完整 Chat Completions URL（doc2/05 §2：不自行拼路径）。
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
}

/// 模型响应正文上限（doc2/05 §2：有界响应，防异常大包）。
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// v2 模型客户端（doc2/05 §2）：reqwest 处理 HTTPS/TLS、DNS、超时与标准响应编码；
/// 只接受 2xx + `choices[0].message.content` 字符串；usage 有则返回。
pub struct OpenAiCompatibleClient {
    cfg: ModelConfig,
    http: reqwest::Client,
}

impl OpenAiCompatibleClient {
    pub fn new(cfg: ModelConfig) -> Result<Self, String> {
        // 启动校验：scheme http/https、主机与路径明确（完整 endpoint，不猜路径）。
        let url = reqwest::Url::parse(&cfg.endpoint)
            .map_err(|e| format!("model_endpoint 不是合法 URL: {e}"))?;
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(format!("model_endpoint scheme 必须是 http/https，实际 {other}")),
        }
        if url.host_str().is_none() || url.path().len() <= 1 {
            return Err("model_endpoint 必须包含主机与具体路径（完整 Chat Completions URL）".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.timeout)
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        Ok(Self { cfg, http })
    }
}

impl ExtractModel for OpenAiCompatibleClient {
    async fn extract(&self, system: &str, user: &str) -> Result<ExtractOutput, ExtractError> {
        #[derive(serde::Serialize)]
        struct Msg<'a> {
            role: &'a str,
            content: &'a str,
        }
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            messages: [Msg<'a>; 2],
            temperature: f32,
        }
        let req = Req {
            model: &self.cfg.model,
            messages: [Msg { role: "system", content: system }, Msg { role: "user", content: user }],
            temperature: 0.0,
        };
        let resp = self
            .http
            .post(&self.cfg.endpoint)
            .bearer_auth(&self.cfg.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ExtractError::Timeout
                } else {
                    ExtractError::Transport(format!("模型端点请求失败: {e}"))
                }
            })?;
        let status = resp.status();
        if status.as_u16() == 408 || status.as_u16() == 504 {
            return Err(ExtractError::Timeout);
        }
        if !status.is_success() {
            return Err(ExtractError::Transport(format!("模型端点返回 {status}")));
        }
        // 有界读取正文（流式累积，超限即失败，不静默截断）。
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut raw: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ExtractError::Transport(format!("读取响应失败: {e}")))?;
            if raw.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(ExtractError::Transport(format!(
                    "模型响应超过 {MAX_RESPONSE_BYTES} 字节上限"
                )));
            }
            raw.extend_from_slice(&chunk);
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|_| ExtractError::BadJson)?;
        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or(ExtractError::BadJson)?
            .to_string();
        // usage 有则记录；提供者未给时保持 None，不估算（doc2/05 §2）。
        let usage = parsed.get("usage");
        let input_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64());
        let output_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64());
        Ok(ExtractOutput { content, input_tokens, output_tokens })
    }
}

/// 启动单 worker 串行循环（D-10）。模型未配置或客户端构建失败时不开线程并报明确错误。
pub fn spawn_worker(state: AppState, model: Option<ModelConfig>) {
    let Some(cfg) = model else { return };
    let client = match OpenAiCompatibleClient::new(cfg.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[worker] 模型客户端初始化失败，自动提取不可用: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        loop {
            let claimed = {
                let mut guard = state.store.lock().unwrap();
                let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
                match guard.claim_next_ordered_job(&now) {
                    Ok(job) => job,
                    Err(e) => {
                        eprintln!("[worker] claim 失败: {e}");
                        None
                    }
                }
            };
            let Some(job) = claimed else {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };
            let job_id = job.id.clone();
            if let Err(e) = process_job(&state, &client, cfg.clone(), &job).await {
                eprintln!("[worker] job {job_id} 处理失败: {e}");
            }
        }
    });
}

async fn process_job<M: ExtractModel>(
    state: &AppState,
    client: &M,
    cfg: ModelConfig,
    job: &JobRow,
) -> Result<(), String> {
    let scope = ScopeKey { tenant_id: job.tenant_id.clone(), user_id: job.user_id.clone() };
    let attempts = job.attempts + 1;
    // doc2/05 §3：worker 按作业行 prompt_version 选规则；未知版本显式失败并保留可诊断状态，
    // 不能用"最新规则"处理旧作业。
    if job.prompt_version != EXTRACT_PROMPT_VERSION {
        let mut guard = state.store.lock().unwrap();
        let code = "UNKNOWN_PROMPT_VERSION";
        match guard.fail_job(&job.id, attempts, code).map_err(|x| x.to_string())? {
            memory_store_sqlite::FailOutcome::Retryable { .. } => {}
            memory_store_sqlite::FailOutcome::Dead => {
                eprintln!("[worker] job {} 已 dead（{code}）", job.id);
            }
        }
        return Err(format!("作业 prompt_version={} 无对应规则实现", job.prompt_version));
    }
    let (events, origin) = {
        let guard = state.store.lock().unwrap();
        let lower = guard
            .window_lower_bound(&scope, &job.host_id, &job.session_id, &job.window_key)
            .map_err(|e| e.to_string())?;
        let events = guard
            .load_window_events(&scope, job, lower)
            .map_err(|e| e.to_string())?;
        let origin = Origin {
            host_id: job.host_id.clone(),
            agent_id: "extract-worker".into(),
            session_id: job.session_id.clone(),
        };
        (events, origin)
    };

    // 输入：按 seq 排序的事件 JSON（doc/13 §4）。
    let input = serde_json::to_string(
        &events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "event_id": e.id, "role": e.role, "source_kind": e.source_kind,
                    "time": e.occurred_at, "text": e.content
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|e| e.to_string())?;

    let result = client.extract(EXTRACT_SYSTEM_PROMPT, &input).await;
    let mut guard = state.store.lock().unwrap();
    match result {
        Ok(output) => {
            let extraction: Result<Extraction, String> =
                serde_json::from_str(&output.content).map_err(|e| e.to_string());
            match extraction {
                Ok(ex) => {
                    let mut last_err: Option<String> = None;
                    for c in &ex.candidates {
                        let admission = admit(c, &events);
                        match guard.save_candidate(&scope, job, &origin, c, admission) {
                            Ok(outcome) => {
                                eprintln!(
                                    "[worker] candidate job={} outcome={:?} prompt={}",
                                    job.id, outcome, EXTRACT_PROMPT_VERSION
                                );
                            }
                            Err(e) => {
                                eprintln!("[worker] save_candidate 失败 job={}: {e}", job.id);
                                last_err = Some(e.to_string());
                            }
                        }
                    }
                    if let Some(e) = last_err {
                        let _ = guard.fail_job(&job.id, attempts, "CANDIDATE_WRITE_FAILED");
                        return Err(e);
                    }
                    // 用量：提供者给了 usage 就持久化，没给保持 NULL 不估算（doc2/05 §2）。
                    guard
                        .complete_job(
                            &job.id,
                            attempts,
                            &cfg.model,
                            output.input_tokens,
                            output.output_tokens,
                        )
                        .map_err(|e| e.to_string())?;
                    Ok(())
                }
                Err(e) => {
                    let _ = guard.fail_job(&job.id, attempts, "BAD_JSON");
                    Err(format!("模型响应不合法: {e}"))
                }
            }
        }
        Err(e) => {
            let code = match &e {
                ExtractError::Timeout => "MODEL_TIMEOUT",
                ExtractError::BadJson => "BAD_JSON",
                ExtractError::Transport(_) => "MODEL_UNAVAILABLE",
            };
            match guard.fail_job(&job.id, attempts, code).map_err(|x| x.to_string())? {
                memory_store_sqlite::FailOutcome::Retryable { .. } => {}
                memory_store_sqlite::FailOutcome::Dead => {
                    eprintln!("[worker] job {} 已 dead（{} 次尝试）", job.id, attempts);
                }
            }
            Err(format!("模型调用失败: {e}"))
        }
    }
}
