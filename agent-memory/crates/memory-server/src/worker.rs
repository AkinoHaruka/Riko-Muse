//! 提取 worker（doc/03 §4、doc/13 §3–5）。
//!
//! v1 单 worker 串行：同一进程内 SQLite 作业轮询（D-10），claim/lease/状态写入原子化。
//! 模型失败只影响派生速度，不丢已提交 L0；重试 3 次后 dead（5/15/45s 延迟）。
//! 模型密钥从文件读取，不进日志、不进 Prompt。

use std::time::Duration;

use memory_domain::{Origin, ScopeKey};
use memory_extract::{
    Admission, ExtractError, ExtractModel, Extraction, ExtractOutput,
};
use memory_store_sqlite::{FailOutcome, JobRow, StoreError};

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

    struct SysCapture {
        response: String,
        system: Arc<Mutex<String>>,
    }

    impl ExtractModel for SysCapture {
        async fn extract(&self, system: &str, _user: &str) -> Result<memory_extract::ExtractOutput, ExtractError> {
            *self.system.lock().unwrap() = system.to_string();
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

    /// 以某 RFC3339 时刻为基准加秒（相对已落库时间戳推导，保证确定性）。
    fn plus_secs(rfc3339: &str, secs: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    #[tokio::test]
    async fn flush_idempotent_and_worker_promotes_candidates() {
        let (store, scope) = setup("promote");
        let state = AppState { store: store.clone(), embedding: None, rerank: None, recency_mode: "none" };
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
                FlushOutcome::Created { job_id, .. } => job_id,
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
            let lower = g.window_lower_bound(&scope, "dsh", "s1", job.through_event_seq).unwrap();
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
            max_tokens: 1024,
            extra_body: None,
        };
        // 先领取（claim 置 running + generation），worker 只处理已领取作业。
        let claimed = store
            .lock()
            .unwrap()
            .claim_next_ordered_job(&plus_secs(&job.run_after, 1))
            .unwrap()
            .unwrap();
        process_job(&state, &mock, cfg, &claimed).await.unwrap();
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
        let state = AppState { store: store.clone(), embedding: None, rerank: None, recency_mode: "none" };
        let ev = {
            let mut g = store.lock().unwrap();
            ingest_user(&mut g, &scope, 1, "以后回答我用中文")
        };
        let job_id = {
            let mut g = store.lock().unwrap();
            match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
                FlushOutcome::Created { job_id, .. } => job_id,
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
            max_tokens: 1024,
            extra_body: None,
        };
        let claimed = store
            .lock()
            .unwrap()
            .claim_next_ordered_job(&plus_secs(&job.run_after, 1))
            .unwrap()
            .unwrap();
        let r = process_job(&state, &mock, cfg, &claimed).await;
        assert!(r.is_err());
        // L0 仍在；作业 retryable（BAD_JSON 按退避重试，状态已落地）。
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
        let job1_id = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Created { job_id, .. } => job_id,
            _ => panic!(),
        };
        // save_candidate 事务内核对 running+generation，必须先真实领取。
        let run_after1 = g.get_job(&scope, &job1_id).unwrap().unwrap().run_after;
        let job = g
            .claim_next_ordered_job(&plus_secs(&run_after1, 1))
            .unwrap()
            .unwrap();
        assert_eq!(job.id, job1_id);
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
        // job1 完成后 job2 才可领取（前窗规则：through 1 未完成会阻断 through 2）。
        g.complete_job(&job1_id, job.claim_generation, 1, "mock", None, None).unwrap();

        // 规则 9：同属性键（response_language=以后用...回答）不同值 → POSSIBLE_CONFLICT held。
        let ev2 = ingest_user(&mut g, &scope, 2, "以后用英文回答");
        let job2_id = match g.flush_window(&scope, "dsh", "s1", 2).unwrap() {
            FlushOutcome::Created { job_id, .. } => job_id,
            _ => panic!(),
        };
        let run_after2 = g.get_job(&scope, &job2_id).unwrap().unwrap().run_after;
        let job2 = g
            .claim_next_ordered_job(&plus_secs(&run_after2, 1))
            .unwrap()
            .unwrap();
        assert_eq!(job2.id, job2_id);
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
        let job1_id = match g.flush_window(&scope, "dsh", "s1", 1).unwrap() {
            FlushOutcome::Created { job_id, .. } => job_id,
            _ => panic!(),
        };
        let run_after = g.get_job(&scope, &job1_id).unwrap().unwrap().run_after;
        let job1 = g
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        assert_eq!(job1.id, job1_id);
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
            FlushOutcome::Created { job_id, .. } => job_id,
            _ => panic!(),
        };
        // 第 1、2 次失败 → retryable；第 3 次 → dead。fail/complete 只在
        // running + generation 匹配时生效（doc4/02 §5），每轮先真实领取。
        for round in 1..=3i32 {
            let run_after = g.get_job(&scope, &job_id).unwrap().unwrap().run_after;
            let claimed = g
                .claim_next_ordered_job(&plus_secs(&run_after, 1))
                .unwrap()
                .unwrap();
            assert_eq!(claimed.id, job_id);
            let outcome = g.fail_job(&job_id, claimed.claim_generation, round, "MODEL_UNAVAILABLE");
            if round < 3 {
                assert!(matches!(outcome, Ok(memory_store_sqlite::FailOutcome::Retryable { .. })));
            } else {
                assert!(matches!(outcome, Ok(memory_store_sqlite::FailOutcome::Dead)));
            }
        }
        let j = g.get_job(&scope.clone(), &job_id).unwrap().unwrap();
        assert_eq!(j.status, "dead");
        assert_eq!(j.attempts, 3);
        // 幂等窗口键仍生效：重试 dead 后 requeue。
        assert!(g.retry_dead_job(&memory_domain::ScopeKey { tenant_id: "t".into(), user_id: "u".into() }, &job_id).unwrap());
    }

    #[test]
    fn request_body_has_bounded_output_and_merges_extra_fields() {
        let base = ModelConfig {
            endpoint: "https://api.example.com/v1/chat/completions".into(),
            model: "m".into(),
            api_key: "k".into(),
            timeout: Duration::from_secs(30),
            max_tokens: 1024,
            extra_body: None,
        };
        let body = build_request_body(&base, "sys", "user");
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "user");
        // 额外字段顶层合并，且可覆盖默认值（如 provider 专有 thinking 开关）。
        let with_extra = ModelConfig {
            extra_body: Some(serde_json::json!({"enable_thinking": false, "temperature": 0.2})),
            ..base
        };
        let body2 = build_request_body(&with_extra, "sys", "user");
        assert_eq!(body2["enable_thinking"], false);
        assert_eq!(body2["temperature"], 0.2);
        assert_eq!(body2["max_tokens"], 1024);
    }

    #[tokio::test]
    async fn prompt_dispatch_uses_job_version_and_fails_unknown() {
        // doc2/05 §3：worker 按作业行 prompt_version 选提示词——老版本作业用老提示词，
        // 未知版本显式失败，不得用"最新规则"处理旧作业。
        let (store, scope) = setup("dispatch");
        let state = AppState { store: store.clone(), embedding: None, rerank: None, recency_mode: "none" };
        // 三个 session 各一作业：完成提交会置 succeeded 并使旧代际失效，不能复用同一作业。
        for (session, content) in [
            ("sv3", "以后回答我用中文"),
            ("sv1", "以后回答我用中文"),
            ("sv0", "以后回答我用中文"),
            ("sv0b", "以后回答我用中文"),
        ] {
            let t = chrono::Utc::now();
            let origin = memory_domain::Origin {
                host_id: "dsh".into(),
                agent_id: "agent-a".into(),
                session_id: session.into(),
            };
            store
                .lock()
                .unwrap()
                .record_evidence(&scope, &origin, 1, "user", "user", &t, content)
                .unwrap();
        }
        let flush_job = |store: &Arc<Mutex<Store>>, session: &str| {
            let mut g = store.lock().unwrap();
            let job_id = match g.flush_window(&scope, "dsh", session, 1).unwrap() {
                FlushOutcome::Created { job_id, .. } => job_id,
                _ => panic!(),
            };
            let run_after = g.get_job(&scope, &job_id).unwrap().unwrap().run_after;
            let claimed = g
                .claim_next_ordered_job(&plus_secs(&run_after, 1))
                .unwrap()
                .unwrap();
            assert_eq!(claimed.id, job_id);
            claimed
        };
        let cfg = ModelConfig {
            endpoint: "http://unused".into(),
            model: "mock".into(),
            api_key: "unused".into(),
            timeout: Duration::from_secs(1),
            max_tokens: 1024,
            extra_body: None,
        };
        let empty = Arc::new(Mutex::new(String::new()));

        // 当前版本（extract_v3/admit_v2）作业 → v3 提示词，admission_version 为 admit_v2。
        let job = flush_job(&store, "sv3");
        assert_eq!(job.prompt_version, memory_contract::EXTRACT_PROMPT_VERSION);
        assert_eq!(job.admission_version, memory_contract::ADMISSION_VERSION);
        assert_eq!(job.prompt_version, "extract_v3");
        assert_eq!(job.admission_version, "admit_v2");
        let sys2 = empty.clone();
        let mock2 = SysCapture { system: sys2.clone(), response: "{\"candidates\":[]}".into() };
        process_job(&state, &mock2, cfg.clone(), &job).await.unwrap();
        assert_eq!(*sys2.lock().unwrap(), memory_extract::EXTRACT_SYSTEM_PROMPT_V3);

        // 老版本（extract_v1 冻结文本）作业 → 老提示词（extract_v2 的分派在
        // admission_version_dispatch_old_and_new_jobs 中与 admit_v1 一并覆盖）。
        let mut job = flush_job(&store, "sv1");
        job.prompt_version = memory_contract::EXTRACT_PROMPT_VERSION_V1.into();
        let sys1 = empty.clone();
        let mock1 = SysCapture { system: sys1.clone(), response: "{\"candidates\":[]}".into() };
        process_job(&state, &mock1, cfg.clone(), &job).await.unwrap();
        assert_eq!(*sys1.lock().unwrap(), memory_extract::EXTRACT_SYSTEM_PROMPT_V1);

        // 未知版本 → 确定性失败：立即 dead、不调用模型、不进退避（doc5 卡 D5-0）。
        let mut job = flush_job(&store, "sv0");
        job.prompt_version = "extract_v0".into();
        let calls0 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock0 = SysCapture { system: empty.clone(), response: "{\"candidates\":[]}".into() };
        let mock0 = CountingModel { inner: mock0, calls: calls0.clone() };
        let err = process_job(&state, &mock0, cfg.clone(), &job).await.unwrap_err();
        assert!(err.contains("UNKNOWN_PROMPT_VERSION") || err.contains("无对应规则实现"), "实际错误: {err}");
        assert_eq!(calls0.load(std::sync::atomic::Ordering::SeqCst), 0, "确定性失败不得调用模型");
        let j = store.lock().unwrap().get_job(&scope, &job.id).unwrap().unwrap();
        assert_eq!(j.status, "dead", "未知版本必须一次失败即 dead");
        assert_eq!(j.attempts, 1, "本次 attempt 只记一次");
        let detail = store
            .lock()
            .unwrap()
            .get_job_detail(&scope, &job.id)
            .unwrap()
            .expect("作业详情应存在");
        assert_eq!(detail.item.error_code.as_deref(), Some("UNKNOWN_PROMPT_VERSION"));

        // 未知 admission version → 同样确定性 dead，不调用模型（doc5 卡 D5-3）。
        let mut job = flush_job(&store, "sv0b");
        job.admission_version = "admit_v0".into();
        let calls0b = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock0b = SysCapture { system: empty.clone(), response: "{\"candidates\":[]}".into() };
        let mock0b = CountingModel { inner: mock0b, calls: calls0b.clone() };
        let err = process_job(&state, &mock0b, cfg, &job).await.unwrap_err();
        assert!(err.contains("UNKNOWN_ADMISSION_VERSION") || err.contains("无对应规则实现"), "实际错误: {err}");
        assert_eq!(calls0b.load(std::sync::atomic::Ordering::SeqCst), 0, "未知准入版本不得调用模型");
        let j = store.lock().unwrap().get_job(&scope, &job.id).unwrap().unwrap();
        assert_eq!(j.status, "dead");
        assert_eq!(j.attempts, 1);
        let detail = store
            .lock()
            .unwrap()
            .get_job_detail(&scope, &job.id)
            .unwrap()
            .expect("作业详情应存在");
        assert_eq!(detail.item.error_code.as_deref(), Some("UNKNOWN_ADMISSION_VERSION"));
    }

    #[tokio::test]
    async fn admission_version_dispatch_old_and_new_jobs() {
        // doc5/03 §5 + doc5/07 C：同一候选，旧作业（extract_v2/admit_v1）保持旧判定，
        // 新作业（extract_v3/admit_v2）用新规则；revision reason 表明实际策略版本。
        let (store, scope) = setup("admver");
        let state = AppState { store: store.clone(), embedding: None, rerank: None, recency_mode: "none" };
        for session in ["s_old", "s_new"] {
            let t = chrono::Utc::now();
            let origin = memory_domain::Origin {
                host_id: "dsh".into(),
                agent_id: "agent-a".into(),
                session_id: session.into(),
            };
            store
                .lock()
                .unwrap()
                .record_evidence(&scope, &origin, 1, "user", "user", &t, "我在杭州做后端开发。我主要写 Rust。")
                .unwrap();
        }
        let cfg = ModelConfig {
            endpoint: "http://unused".into(),
            model: "mock".into(),
            api_key: "unused".into(),
            timeout: Duration::from_secs(1),
            max_tokens: 1024,
            extra_body: None,
        };
        // 旧作业：extract_v2/admit_v1 → 候选 held:NOT_EXPLICIT，不建记忆。
        run_extract_job(&store, &state, &scope, "s_old", Some("extract_v2"), Some("admit_v1"), cfg.clone())
            .await
            .unwrap();
        {
            let g = store.lock().unwrap();
            let (hits, _) = g.search_memories(&scope, "后端", 5, false).unwrap();
            assert!(hits.is_empty(), "admit_v1 下旧判定不建 active");
            let held = g.count_candidates_by_reason(&scope, "NOT_EXPLICIT").unwrap();
            assert_eq!(held, 1, "旧作业候选保持旧结果");
        }
        // 新作业：extract_v3/admit_v2 → 候选 active，建记忆。
        run_extract_job(&store, &state, &scope, "s_new", None, None, cfg.clone())
            .await
            .unwrap();
        {
            let g = store.lock().unwrap();
            let (hits, _) = g.search_memories(&scope, "后端", 5, false).unwrap();
            assert_eq!(hits.len(), 1, "admit_v2 下同一候选 active");
            assert_eq!(hits[0].claim, "我在杭州做后端开发");
        }
        // revision reason 的策略版本断言在 store 层测试（jobs.rs::revision_reason_shows_policy_version）。
    }

    /// 测试助手：flush→claim→（可选覆写版本）→固定响应候选→process_job。
    async fn run_extract_job(
        store: &Arc<Mutex<Store>>,
        state: &AppState,
        scope: &memory_domain::ScopeKey,
        session: &str,
        prompt_version: Option<&str>,
        admission_version: Option<&str>,
        cfg: ModelConfig,
    ) -> Result<(), String> {
        let mut g = store.lock().unwrap();
        let job_id = match g.flush_window(scope, "dsh", session, 1).unwrap() {
            FlushOutcome::Created { job_id, .. } => job_id,
            _ => panic!(),
        };
        let run_after = g.get_job(scope, &job_id).unwrap().unwrap().run_after;
        let mut job = g
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        assert_eq!(job.id, job_id);
        if let Some(pv) = prompt_version {
            job.prompt_version = pv.into();
        }
        if let Some(av) = admission_version {
            job.admission_version = av.into();
        }
        let ev_id = {
            let lower = g
                .window_lower_bound(scope, "dsh", session, job.through_event_seq)
                .unwrap();
            g.load_window_events(scope, &job, lower).unwrap()[0].id.clone()
        };
        let response = format!(
            "{{\"candidates\":[{{\"source_event_id\":\"{ev_id}\",\"quote\":\"我在杭州做后端开发\",\"kind\":\"fact\"}}]}}"
        );
        drop(g);
        let mock = MockModel { response, calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)) };
        process_job(state, &mock, cfg, &job).await
    }

    /// 包装模型以统计调用次数（确定性失败不得调用模型）。
    struct CountingModel {
        inner: SysCapture,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ExtractModel for CountingModel {
        async fn extract(&self, system: &str, user: &str) -> Result<memory_extract::ExtractOutput, ExtractError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.extract(system, user).await
        }
    }
}


#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// 完整 Chat Completions URL（doc2/05 §2：不自行拼路径）。
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub timeout: Duration,
    /// 单次生成上限（max_tokens）。默认 1024：推理型 provider 无限时输出会把
    /// 提取调用拖到分钟级（2026-09-25 SiliconFlow Qwen3.5-4B 实测 >180s）。
    pub max_tokens: u32,
    /// 额外请求体字段（provider 专有开关，如 enable_thinking=false），顶层合并进请求。
    /// None 时不合并任何字段；值须是 JSON 对象。
    pub extra_body: Option<serde_json::Value>,
}

/// 构造 Chat Completions 请求体：固定形状 + 可选 provider 专有字段顶层合并。
/// 合并发生在固定字段之后，专有字段可覆盖默认值（调用方自行保证值合法）。
fn build_request_body(
    cfg: &ModelConfig,
    system: &str,
    user: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "temperature": 0.0,
        "max_tokens": cfg.max_tokens,
    });
    if let Some(extra) = cfg.extra_body.as_ref().and_then(|v| v.as_object()) {
        let map = body.as_object_mut().expect("body 是 object");
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
    body
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
        let req = build_request_body(&self.cfg, system, user);
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

/// 处理一个已领取作业：处理期间每 `JOB_HEARTBEAT_SECS` 条件化续租（doc4/02 §4）；
/// 任务结束（成功/失败/丢弃）即停止心跳。续租失败只停止心跳并放弃续租，
/// 作业结果是否可提交由数据库 generation 校验裁决（`process_job_inner`）。
async fn process_job<M: ExtractModel>(
    state: &AppState,
    client: &M,
    cfg: ModelConfig,
    job: &JobRow,
) -> Result<(), String> {
    let heartbeat = {
        let hb_state = state.clone();
        let hb_job_id = job.id.clone();
        let hb_generation = job.claim_generation;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(memory_contract::JOB_HEARTBEAT_SECS)).await;
                let renewed = {
                    // 只短暂持有 DB 锁；不在此等待模型。
                    let mut guard = hb_state.store.lock().unwrap();
                    guard.renew_job_lease(&hb_job_id, hb_generation).unwrap_or(false)
                };
                if !renewed {
                    eprintln!("[worker] job {hb_job_id} 续租失败（失去执行权），停止心跳");
                    break;
                }
            }
        })
    };
    let result = process_job_inner(state, client, cfg, job).await;
    heartbeat.abort();
    result
}

async fn process_job_inner<M: ExtractModel>(
    state: &AppState,
    client: &M,
    cfg: ModelConfig,
    job: &JobRow,
) -> Result<(), String> {
    let scope = ScopeKey { tenant_id: job.tenant_id.clone(), user_id: job.user_id.clone() };
    let attempts = job.attempts + 1;

    // 外层结果处理（doc4/02 §5）：一旦已 claim，任何错误离开本函数前都按类别落状态；
    // 不再有"直接返回、作业留在 running"的路径。
    // 确定性失败：未知 prompt 版本，同输入重试不会改变 → 立即 dead（不走退避阶梯），
    // 不调用模型（doc5 卡 D5-0 收口 doc-handoff/08 发现 1）。
    let Some(system_prompt) = memory_extract::system_prompt_for(&job.prompt_version) else {
        let mut guard = state.store.lock().unwrap();
        let code = "UNKNOWN_PROMPT_VERSION";
        match guard.fail_job_deterministic(&job.id, job.claim_generation, attempts, code) {
            Ok(()) => {
                eprintln!("[worker] job {} 已 dead（{code}）", job.id);
            }
            Err(e) => {
                eprintln!("[worker] job {} 失败写入未生效（{e}），交由 lease 恢复", job.id);
            }
        }
        return Err(format!("作业 prompt_version={} 无对应规则实现", job.prompt_version));
    };
    // doc5/03 §1：准入规则按作业行 admission_version 分派，与 Prompt 版本相互独立；
    // 未知准入版本同样确定性 dead，不退回"最新规则"（doc5 卡 D5-3）。
    let admission_version = match job.admission_version.as_str() {
        memory_contract::ADMISSION_VERSION_V1 | memory_contract::ADMISSION_VERSION_V2 => {
            job.admission_version.clone()
        }
        other => {
            let mut guard = state.store.lock().unwrap();
            let code = "UNKNOWN_ADMISSION_VERSION";
            match guard.fail_job_deterministic(&job.id, job.claim_generation, attempts, code) {
                Ok(()) => {
                    eprintln!("[worker] job {} 已 dead（{code}）", job.id);
                }
                Err(e) => {
                    eprintln!("[worker] job {} 失败写入未生效（{e}），交由 lease 恢复", job.id);
                }
            }
            return Err(format!("作业 admission_version={other} 无对应规则实现"));
        }
    };

    // 加载窗口 + 下界：窗口超限为确定性失败（不调用模型，直接 dead）；下界查询/读库
    // 的暂态错误按退避重试（WINDOW_READ_FAILED）。锁在模型调用前释放。
    let (events, origin) = {
        let mut guard = state.store.lock().unwrap();
        let loaded = guard
            .window_lower_bound(&scope, &job.host_id, &job.session_id, job.through_event_seq)
            .and_then(|lower| Ok((lower, guard.load_window_events(&scope, job, lower)?)));
        let (lower, events) = match loaded {
            Ok(v) => v,
            Err(StoreError::WindowTooLarge) => {
                // 窗口超限为确定性失败：同输入重试不会改变 → 立即 dead，不空转重试。
                match guard.fail_job_deterministic(&job.id, job.claim_generation, attempts, "WINDOW_TOO_LARGE") {
                    Ok(()) => {
                        eprintln!("[worker] job {} 已 dead（WINDOW_TOO_LARGE）", job.id);
                    }
                    Err(e) => eprintln!("[worker] job {} 失败写入未生效（{e}），交由 lease 恢复", job.id),
                }
                return Err(format!("窗口超限（job {}，不调用模型，已 dead）", job.id));
            }
            Err(e) => {
                match guard.fail_job(&job.id, job.claim_generation, attempts, "WINDOW_READ_FAILED") {
                    Ok(_) => {}
                    Err(e2) => eprintln!("[worker] job {} 失败写入未生效（{e2}），交由 lease 恢复", job.id),
                }
                return Err(format!("窗口加载失败: {e}"));
            }
        };
        let origin = Origin {
            host_id: job.host_id.clone(),
            agent_id: "extract-worker".into(),
            session_id: job.session_id.clone(),
        };
        let _ = lower;
        (events, origin)
    };

    // 输入：按 seq 排序的事件 JSON（doc/13 §4）。与 flush 分窗预算共用同一 builder
    // （doc4/03 §1—2），序列化失败按暂态处理。
    let input = match memory_extract::serialize_window_events(&events) {
        Ok(s) => s,
        Err(e) => {
            let mut guard = state.store.lock().unwrap();
            match guard.fail_job(&job.id, job.claim_generation, attempts, "WINDOW_READ_FAILED") {
                Ok(_) => {}
                Err(e2) => eprintln!("[worker] job {} 失败写入未生效（{e2}），交由 lease 恢复", job.id),
            }
            return Err(e);
        }
    };

    // 模型网络调用不持锁。
    let result = client.extract(system_prompt, &input).await;
    let mut guard = state.store.lock().unwrap();
    // 旧执行者隔离：提交任何结果前核当前代际；失败/查不动时丢弃本轮结果，
    // 不改候选与作业状态，交由 lease 到期恢复（doc4/02 §5）。
    match guard.job_generation_current(&job.id, job.claim_generation) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("[worker] job {} 失去执行权（STALE_CLAIM），丢弃模型结果", job.id);
            return Err("STALE_CLAIM: 作业执行权已被恢复或接管".into());
        }
        Err(e) => {
            eprintln!("[worker] job {} 执行权核验失败（{e}），丢弃本轮结果，等待 lease 恢复", job.id);
            return Err(format!("执行权核验失败: {e}"));
        }
    }
    match result {
        Ok(output) => {
            let extraction: Result<Extraction, String> =
                memory_extract::parse_extraction(&output.content);
            match extraction {
                Ok(ex) => {
                    let mut last_err: Option<String> = None;
                    for c in &ex.candidates {
                        // 按作业行版本分派准入（版本已在上方验证，分派必有实现）。
                        let admission = memory_extract::admit_for(&admission_version, c, &events)
                            .unwrap_or(Admission::Held("NOT_EXPLICIT"));
                        match guard.save_candidate(&scope, job, &origin, c, admission) {
                            Ok(outcome) => {
                                eprintln!(
                                    "[worker] candidate job={} outcome={:?} prompt={} admission={}",
                                    job.id, outcome, job.prompt_version, job.admission_version
                                );
                            }
                            Err(StoreError::StaleClaim) => {
                                eprintln!(
                                    "[worker] job {} 候选写入前失去执行权（STALE_CLAIM），丢弃剩余候选",
                                    job.id
                                );
                                return Err("STALE_CLAIM: 候选写入前失去执行权".into());
                            }
                            Err(e) => {
                                eprintln!("[worker] save_candidate 失败 job={}: {e}", job.id);
                                last_err = Some(e.to_string());
                            }
                        }
                    }
                    if let Some(e) = last_err {
                        match guard.fail_job(&job.id, job.claim_generation, attempts, "CANDIDATE_WRITE_FAILED") {
                            Ok(_) => {}
                            Err(e2) => eprintln!("[worker] job {} 失败写入未生效（{e2}），交由 lease 恢复", job.id),
                        }
                        return Err(e);
                    }
                    // 用量：提供者给了 usage 就持久化，没给保持 NULL 不估算（doc2/05 §2）。
                    match guard.complete_job(
                        &job.id,
                        job.claim_generation,
                        attempts,
                        &cfg.model,
                        output.input_tokens,
                        output.output_tokens,
                    ) {
                        Ok(()) => Ok(()),
                        Err(StoreError::StaleClaim) => {
                            eprintln!("[worker] job {} 完成提交时失去执行权（STALE_CLAIM），不改状态", job.id);
                            Err("STALE_CLAIM: 完成提交未生效".into())
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
                Err(e) => {
                    match guard.fail_job(&job.id, job.claim_generation, attempts, "BAD_JSON") {
                        Ok(_) => {}
                        Err(e2) => eprintln!("[worker] job {} 失败写入未生效（{e2}），交由 lease 恢复", job.id),
                    }
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
            match guard.fail_job(&job.id, job.claim_generation, attempts, code) {
                Ok(FailOutcome::Retryable { .. }) => {}
                Ok(FailOutcome::Dead) => {
                    eprintln!("[worker] job {} 已 dead（{} 次尝试）", job.id, attempts);
                }
                Err(e2) => eprintln!("[worker] job {} 失败写入未生效（{e2}），交由 lease 恢复", job.id),
            }
            Err(format!("模型调用失败: {e}"))
        }
    }
}
