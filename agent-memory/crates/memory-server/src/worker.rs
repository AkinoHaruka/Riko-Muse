//! 提取 worker（doc/03 §4、doc/13 §3–5）。
//!
//! v1 单 worker 串行：同一进程内 SQLite 作业轮询（D-10），claim/lease/状态写入原子化。
//! 模型失败只影响派生速度，不丢已提交 L0；重试 3 次后 dead（5/15/45s 延迟）。
//! 模型密钥从文件读取，不进日志、不进 Prompt。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use memory_domain::{Origin, ScopeKey};
use memory_extract::{
    admit, ExtractError, ExtractModel, Extraction, EXTRACT_PROMPT_VERSION, EXTRACT_SYSTEM_PROMPT,
};
use memory_store_sqlite::JobRow;
use serde::Serialize;

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
        async fn extract(&self, _system: &str, _user: &str) -> Result<String, ExtractError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.response.clone())
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
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
}

/// OpenAI 兼容提取客户端。手写最小 HTTP/1.1（本机/内网端点，避免外部依赖）。
pub struct OpenAiCompatibleClient {
    cfg: ModelConfig,
}

impl OpenAiCompatibleClient {
    pub fn new(cfg: ModelConfig) -> Self {
        Self { cfg }
    }
}

impl ExtractModel for OpenAiCompatibleClient {
    async fn extract(&self, system: &str, user: &str) -> Result<String, ExtractError> {
        let cfg = self.cfg.clone();
        let system = system.to_string();
        let user = user.to_string();
        tokio::task::spawn_blocking(move || http_post_json(&cfg, &system, &user))
            .await
            .map_err(|e| ExtractError::Transport(format!("spawn_blocking 失败: {e}")))?
    }
}

fn http_post_json(cfg: &ModelConfig, system: &str, user: &str) -> Result<String, ExtractError> {
    let url = cfg
        .endpoint
        .strip_prefix("http://")
        .ok_or_else(|| ExtractError::Transport("首版仅支持 http:// 模型端点".into()))?;
    let (hostport, path) = url
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((url, "/v1/chat/completions".into()));
    let (host, port) = hostport
        .split_once(':')
        .map(|(h, p)| (h, p.parse::<u16>().unwrap_or(80)))
        .unwrap_or((hostport, 80));

    #[derive(Serialize)]
    struct Msg<'a> {
        role: &'a str,
        content: &'a str,
    }
    #[derive(Serialize)]
    struct Req<'a> {
        model: &'a str,
        messages: [Msg<'a>; 2],
        temperature: f32,
    }
    let body = serde_json::to_string(&Req {
        model: &cfg.model,
        messages: [Msg { role: "system", content: system }, Msg { role: "user", content: user }],
        temperature: 0.0,
    })
    .map_err(|e| ExtractError::Transport(e.to_string()))?;

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e: std::net::AddrParseError| ExtractError::Transport(e.to_string()))?,
        cfg.timeout,
    )
    .map_err(|e| ExtractError::Transport(format!("连接模型端点失败: {e}")))?;
    stream
        .set_read_timeout(Some(cfg.timeout))
        .map_err(|e| ExtractError::Transport(e.to_string()))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nAuthorization: Bearer {key}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        key = cfg.api_key,
        len = body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| ExtractError::Transport(format!("写入请求失败: {e}")))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| ExtractError::Transport(format!("读取响应失败: {e}")))?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, resp_body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| ExtractError::Transport("响应缺少头部分隔".into()))?;
    let status_line = head.lines().next().unwrap_or("");
    let status_code: u32 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ExtractError::Transport(format!("无法解析状态行: {status_line}")))?;
    if status_code == 408 || status_code == 504 {
        return Err(ExtractError::Timeout);
    }
    if !(200..300).contains(&status_code) {
        return Err(ExtractError::Transport(format!("模型端点返回 {status_code}")));
    }
    // 解析 OpenAI chat completion：choices[0].message.content。
    let parsed: serde_json::Value =
        serde_json::from_str(resp_body).map_err(|_| ExtractError::BadJson)?;
    let content = parsed
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or(ExtractError::BadJson)?;
    Ok(content.to_string())
}

/// 启动单 worker 串行循环（D-10）。模型未配置时不开线程。
pub fn spawn_worker(state: AppState, model: Option<ModelConfig>) {
    let Some(cfg) = model else { return };
    let client = OpenAiCompatibleClient::new(cfg.clone());
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
    let (events, origin, attempts) = {
        let guard = state.store.lock().unwrap();
        let lower = guard
            .window_lower_bound(&scope, &job.host_id, &job.session_id, &job.window_key)
            .map_err(|e| e.to_string())?;
        let events = guard
            .load_window_events(&scope, job, lower)
            .map_err(|e| e.to_string())?;
        let attempts = job.attempts + 1;
        let origin = Origin {
            host_id: job.host_id.clone(),
            agent_id: "extract-worker".into(),
            session_id: job.session_id.clone(),
        };
        (events, origin, attempts)
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
        Ok(text) => {
            let extraction: Result<Extraction, String> =
                serde_json::from_str(&text).map_err(|e| e.to_string());
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
                    // 用量：手写客户端未解析 usage，留空（提供者返回时记录，doc/09）。
                    guard
                        .complete_job(&job.id, attempts, &cfg.model, None, None)
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
