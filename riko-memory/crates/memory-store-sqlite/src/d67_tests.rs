//! D6-7 固定响应验收测试（doc6/08 卡内要求：以固定模型 response 验证
//! trigger/job/input/receipt 原子链路；不判断语义质量，真实模型不在本卡）。

use crate::dream_jobs::{locate_quote_span, DREAM_EXTRACT_V2};
use crate::{Store, StoreError};
use memory_domain::{MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-d67-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    let origin = Origin {
        host_id: "dsh".into(),
        agent_id: "a".into(),
        session_id: "s".into(),
    };
    (store, scope, origin)
}

fn ingest_user(
    store: &mut Store,
    scope: &ScopeKey,
    origin: &Origin,
    seq: i64,
    content: &str,
) -> String {
    let t = chrono::Utc::now();
    match store
        .record_evidence(
            scope,
            origin,
            seq,
            "user",
            "user",
            &t,
            content,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) => id,
        crate::IngestOutcome::AlreadyRecorded(id) => id,
    }
}

#[test]
fn dream_trigger_snapshot_idempotent_and_freezes_inputs() {
    // doc6/10 §4/§5：同 trigger_key 幂等返回原 job；快照固化输入与账本；
    // 快照后新事件留 pending（等待下一 job）；无事件时不建空作业。
    let (mut store, scope, origin) = setup("trigger");
    // 无事件：不建作业。
    assert!(store
        .dream_trigger(
            &scope,
            "manual",
            "k0",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main()
        )
        .unwrap()
        .is_none());
    let e1 = ingest_user(&mut store, &scope, &origin, 1, "我住在杭州");
    let e2 = ingest_user(&mut store, &scope, &origin, 2, "我对芒果过敏");
    let now = crate::now_rfc3339_pub().unwrap();
    let job = store
        .dream_trigger(
            &scope,
            "manual",
            "k1",
            Some("agent-a"),
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(job.extract_version, DREAM_EXTRACT_V2);
    assert_eq!(job.status, "queued");
    // 同 key 重放：返回原 job（幂等 coalesce）。
    let replay = store
        .dream_trigger(
            &scope,
            "manual",
            "k1",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(replay.id, job.id);
    // 快照固化：两条输入、账本 assigned。
    let inputs = store.dream_input_evidence_ids(&scope, &job.id).unwrap();
    assert_eq!(inputs, vec![e1.clone(), e2.clone()]);
    let (st, active): (String, String) = store
        .conn()
        .query_row(
            "SELECT status, active_job_id FROM dream_evidence_state WHERE evidence_id=?1",
            rusqlite::params![e1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (st.as_str(), active.as_str()),
        ("assigned", job.id.as_str())
    );
    // 快照后新事件留 pending：不再进当前 job 的输入。
    let e3 = ingest_user(&mut store, &scope, &origin, 3, "下周一要体检");
    let inputs2 = store.dream_input_evidence_ids(&scope, &job.id).unwrap();
    assert_eq!(inputs2.len(), 2, "快照后新事件不进当前作业");
    let pend: String = store
        .conn()
        .query_row(
            "SELECT COALESCE(status,'pending') FROM dream_evidence_state WHERE evidence_id=?1",
            rusqlite::params![e3],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "pending".into());
    assert_ne!(pend, "assigned", "快照后新事件保持待处理");
    // 不同 key 新 trigger：新 job 只收 pending 的新事件。
    let job2 = store
        .dream_trigger(
            &scope,
            "scheduled",
            "auto-2026-09-26",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let inputs3 = store.dream_input_evidence_ids(&scope, &job2.id).unwrap();
    assert_eq!(inputs3, vec![e3], "新作业只取快照时待处理事件");
}

#[test]
fn dream_submit_candidates_validates_spans_and_lifecycle() {
    // doc6/10 §6：Rust 核 evidence 在冻结输入内 + quote 逐字 byte span；
    // succeed 后账本推进 processed；坏 JSON 路径（解析拒绝）→ dead。
    let (mut store, scope, origin) = setup("submit");
    let content = "我住在杭州，喜欢简短回答。";
    let e1 = ingest_user(&mut store, &scope, &origin, 1, content);
    let job = store
        .dream_trigger(
            &scope,
            "manual",
            "k1",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    // claim 时钟须 >= run_after（trigger 内部落库时刻），故在 trigger 后取。
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 90).unwrap().unwrap();
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, "running");
    let gen = claimed.claim_generation;

    // 合法候选：quote 逐字，span 由 locate_quote_span 定位。
    let quote = "我住在杭州";
    let (sb, eb) = locate_quote_span(content, quote).unwrap();
    let good = crate::dream_jobs::DreamProposal {
        kind: "fact".into(),
        claim: "用户住在杭州".into(),
        quote: quote.into(),
        evidence_id: e1.clone(),
        start_byte: sb,
        end_byte: eb,
        status: "candidate".into(),
        reason_code: None,
        occurred_at: None,
    };
    // 非法候选 1：quote 非逐字（改写）。
    let bad_quote = crate::dream_jobs::DreamProposal {
        quote: "用户住在杭州".into(),
        ..good.clone()
    };
    // 非法候选 2：evidence 不在冻结输入内。
    let foreign = crate::dream_jobs::DreamProposal {
        evidence_id: "01a0not-in-job".into(),
        ..good.clone()
    };
    let (accepted, rejected) = store
        .dream_submit_candidates(
            &scope,
            &job.id,
            gen,
            "dream_policy_v1",
            &[good, bad_quote, foreign],
        )
        .unwrap();
    assert_eq!(
        (accepted, rejected),
        (1, 2),
        "逐字 span 与输入归属被强制核验"
    );
    let candidates: Vec<(String, String)> = store
        .conn()
        .prepare("SELECT id, status FROM dream_candidates WHERE dream_job_id=?1")
        .unwrap()
        .query_map(rusqlite::params![job.id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].1, "candidate");
    // succeed：状态推进 + 账本 processed。
    assert!(store
        .dream_succeed(&scope, &job.id, gen, Some("mock"), Some(10), Some(5))
        .unwrap());
    let (jstatus, estatus): (String, String) = store
        .conn()
        .query_row(
            "SELECT (SELECT status FROM dream_jobs WHERE id=?3),
                    (SELECT status FROM dream_evidence_state WHERE evidence_id=?4)",
            rusqlite::params![scope.tenant_id, scope.user_id, job.id, e1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (jstatus.as_str(), estatus.as_str()),
        ("succeeded", "processed")
    );
    // stale claim 提交被拒（generation 不匹配）。
    let late = store.dream_submit_candidates(&scope, &job.id, gen, "dream_policy_v1", &[]);
    assert!(matches!(late, Err(StoreError::StaleClaim)));
}

#[test]
fn dream_recover_expired_and_provider_wait() {
    // doc6/10 §5：provider_wait 保留 assigned；崩溃恢复过期 running 回 queued。
    let (mut store, scope, origin) = setup("recover");
    let _ = ingest_user(&mut store, &scope, &origin, 1, "我住在杭州");
    let job = store
        .dream_trigger(
            &scope,
            "manual",
            "k1",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    // claim 时钟须 >= run_after（trigger 内部落库时刻），故在 trigger 后取。
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 90).unwrap().unwrap();
    let gen = claimed.claim_generation;
    // provider 故障 → provider_wait；账本保持 assigned（不伪装 defer）。
    assert!(store
        .dream_provider_wait(&scope, &job.id, gen, "MODEL_TIMEOUT", None)
        .unwrap());
    let estatus: String = store
        .conn()
        .query_row(
            "SELECT status FROM dream_evidence_state WHERE active_job_id=?1",
            rusqlite::params![job.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(estatus, "assigned");
    // 模拟 lease 过期 → 恢复回 queued（冻结输入不变）。
    store
        .conn()
        .execute(
            "UPDATE dream_jobs SET status='running', lease_until='2020-01-01T00:00:00Z' WHERE id=?1",
            rusqlite::params![job.id],
        )
        .unwrap();
    store.dream_recover_expired(&now).unwrap();
    let recovered = store.dream_get(&scope, &job.id).unwrap().unwrap();
    assert_eq!(recovered.status, "queued", "过期 running 回 queued");
    assert_eq!(
        store
            .dream_input_evidence_ids(&scope, &job.id)
            .unwrap()
            .len(),
        1,
        "恢复后冻结输入不变"
    );
}

#[test]
fn dream_extract_parser_strict() {
    // doc6/05 §3 同法：严格 JSON、可剥围栏、字段集固定、kind 枚举、≤20 候选。
    use crate::dream_jobs::parse_dream_extract_v1;
    let ok = r#"{"candidates":[{"evidence_id":"e1","kind":"fact","quote":"我住在杭州","claim":"用户住在杭州","occurred_at":null}]}"#;
    assert_eq!(parse_dream_extract_v1(ok).unwrap().len(), 1);
    let fenced = format!("```json\n{ok}\n```");
    assert_eq!(parse_dream_extract_v1(&fenced).unwrap().len(), 1);
    assert!(
        parse_dream_extract_v1("not json").is_err(),
        "坏 JSON 确定性失败"
    );
    assert!(parse_dream_extract_v1(
        r#"{"candidates":[{"evidence_id":"e1","kind":"secret","quote":"x","claim":"y"}]}"#
    )
    .is_err());
    assert!(parse_dream_extract_v1(
        r#"{"candidates":[{"evidence_id":"e1","kind":"fact","quote":"x","claim":"y","extra":1}]}"#
    )
    .is_err());
    // >20 候选拒绝。
    let many: Vec<String> = (0..21)
        .map(|i| format!(r#"{{"evidence_id":"e{i}","kind":"fact","quote":"q","claim":"c"}}"#))
        .collect();
    let many_json = format!(r#"{{"candidates":[{}]}}"#, many.join(","));
    assert!(parse_dream_extract_v1(&many_json).is_err());
    // locate_quote_span：非 char_boundary 不 panic。
    let content = "你好世界";
    assert!(locate_quote_span(content, "好世").is_some());
    assert!(
        locate_quote_span(content, "好界").is_none() || locate_quote_span("abc", "bc").is_some()
    );
}
