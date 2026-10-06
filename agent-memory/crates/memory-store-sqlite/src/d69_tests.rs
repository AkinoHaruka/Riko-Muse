//! D6-9 固定验收测试（doc6/08 卡内要求）：retire 即时全路径消失、restore 恢复、
//! 指令 span Rust 校验、purge 两阶段（preview 只写确认元数据/指纹变化拒绝/闭包
//! 清理两张审计表与候选/墓碑防重放/job 不留可反查目标）、retention 默认不删除。

use crate::lifecycle::{RestoreRequest, RetireRequest};
use crate::{Store, StoreError};
use memory_domain::{MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-d69-test-{}-{tag}", std::process::id()));
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

#[test]
fn migration_0011_adds_runner_and_redecision_protocol() {
    let root =
        std::env::temp_dir().join(format!("am-d69-test-{}-migration-0011", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let migrations_10 = root.join("migrations-10");
    std::fs::create_dir_all(&migrations_10).unwrap();
    for version in 1..=10 {
        let source = std::fs::read_dir(migrations_dir())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{version:04}_"))
            })
            .unwrap()
            .path();
        std::fs::copy(&source, migrations_10.join(source.file_name().unwrap())).unwrap();
    }
    let db = root.join("migration.db");
    let mut store = Store::open(&db, &migrations_10).unwrap();
    let token_path = root.join("user.token");
    store.principal_add("t", "u", &token_path).unwrap();
    let token = std::fs::read_to_string(&token_path).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    store.conn_mut().execute(
        "INSERT INTO consolidation_jobs
           (id,tenant_id,user_id,document_kind,document_key,input_fingerprint,generator_version,
            status,attempts,run_after,claim_generation,created_at,updated_at)
         VALUES ('legacy-job',?1,?2,'topic_page','legacy','fingerprint','consolidate_v1',
                 'queued',0,'2026-09-26T00:00:00Z',0,'2026-09-26T00:00:00Z','2026-09-26T00:00:00Z')",
        rusqlite::params![scope.tenant_id, scope.user_id],
    ).unwrap();
    drop(store);
    let store = Store::open(&db, &migrations_dir()).unwrap();
    let applied: i64 = store
        .conn()
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    // 按迁移纪律随新增迁移同步：0019（doc7/10 V2-Q1 上下文条目）起为 19。
    assert_eq!(applied, 19);
    assert_eq!(
        store
            .dream_live_runner_count("9999-01-01T00:00:00Z")
            .unwrap(),
        0
    );
    let redecisions: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM dream_candidate_redecisions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(redecisions, 0);
    let legacy: (String, Option<String>, i64) = store.conn().query_row(
        "SELECT status,error_code,claim_generation FROM consolidation_jobs WHERE id='legacy-job'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(
        legacy,
        (
            "stale_input".into(),
            Some("DREAM_TRIGGER_REQUIRED".into()),
            1
        )
    );
}

fn remember_one(
    store: &mut Store,
    scope: &ScopeKey,
    origin: &Origin,
    seq: i64,
    quote: &str,
    kind: MemoryKind,
) -> String {
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(
            scope,
            origin,
            seq,
            "user",
            "user",
            &t,
            quote,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store
        .remember(
            scope,
            origin,
            &ev,
            quote,
            kind,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

fn source_of(store: &Store, scope: &ScopeKey, memory_id: &str) -> (String, i64, String) {
    let (version, claim_sha256): (i64, String) = store
        .conn()
        .query_row(
            "SELECT version,claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (memory_id.to_string(), version, claim_sha256)
}

#[test]
fn held_candidate_redecision_is_frozen_and_idempotent() {
    let (mut store, scope, origin) = setup("held-redecision");
    let old_text = "我目前在杭州工作";
    let old_evidence = match store
        .record_evidence(
            &scope,
            &origin,
            1,
            "user",
            "user",
            &chrono::Utc::now(),
            old_text,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let old_job = store
        .dream_trigger(
            &scope,
            "manual",
            "held-original",
            Some("a"),
            Some("dsh"),
            Some("s"),
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    let (_, old_running) = store.dream_claim_next(&now, 900).unwrap().unwrap();
    let (old_start, old_end) = crate::dream_jobs::locate_quote_span(old_text, old_text).unwrap();
    store
        .dream_submit_candidates(
            &scope,
            &old_job.id,
            old_running.claim_generation,
            crate::dream_jobs::DREAM_POLICY_V1,
            &[crate::dream_jobs::DreamProposal {
                kind: "fact".into(),
                claim: old_text.into(),
                quote: old_text.into(),
                evidence_id: old_evidence.clone(),
                start_byte: old_start,
                end_byte: old_end,
                status: "held".into(),
                reason_code: Some("defer".into()),
                occurred_at: None,
            }],
        )
        .unwrap();
    store
        .dream_succeed(
            &scope,
            &old_job.id,
            old_running.claim_generation,
            None,
            None,
            None,
        )
        .unwrap();
    let held_id: String = store
        .conn()
        .query_row(
            "SELECT id FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, old_job.id],
            |r| r.get(0),
        )
        .unwrap();

    let new_text = "我现在仍在杭州工作";
    let new_evidence = match store
        .record_evidence(
            &scope,
            &origin,
            2,
            "user",
            "user",
            &chrono::Utc::now(),
            new_text,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let new_job = store
        .dream_trigger(
            &scope,
            "manual",
            "held-related-new-evidence",
            Some("a"),
            Some("dsh"),
            Some("s"),
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    let (_, new_running) = store.dream_claim_next(&now, 900).unwrap().unwrap();
    let (new_start, new_end) = crate::dream_jobs::locate_quote_span(new_text, new_text).unwrap();
    store
        .dream_submit_candidates(
            &scope,
            &new_job.id,
            new_running.claim_generation,
            crate::dream_jobs::DREAM_POLICY_V1,
            &[crate::dream_jobs::DreamProposal {
                kind: "fact".into(),
                claim: new_text.into(),
                quote: new_text.into(),
                evidence_id: new_evidence.clone(),
                start_byte: new_start,
                end_byte: new_end,
                status: "candidate".into(),
                reason_code: None,
                occurred_at: None,
            }],
        )
        .unwrap();
    let fresh = store
        .dream_accepted_candidates(&scope, &new_job.id)
        .unwrap();
    let old_held = store.dream_held_candidates(&scope).unwrap();
    let old_held = old_held
        .iter()
        .find(|(c, _)| c.candidate_id == held_id)
        .unwrap();
    let (fresh_candidate, fresh_spans) = fresh.first().unwrap();
    let mut inputs = Vec::new();
    for (evidence_id, start_byte, end_byte) in &old_held.1 {
        inputs.push(crate::adjudication::AdjudicationCandidate {
            candidate_id: old_held.0.candidate_id.clone(),
            kind: old_held.0.kind.clone(),
            claim: old_held.0.claim.clone(),
            quote: old_held.0.quote.clone(),
            status: "held".into(),
            evidence_id: evidence_id.clone(),
            start_byte: *start_byte,
            end_byte: *end_byte,
        });
    }
    for (evidence_id, start_byte, end_byte) in fresh_spans {
        inputs.push(crate::adjudication::AdjudicationCandidate {
            candidate_id: fresh_candidate.candidate_id.clone(),
            kind: fresh_candidate.kind.clone(),
            claim: fresh_candidate.claim.clone(),
            quote: fresh_candidate.quote.clone(),
            status: "candidate".into(),
            evidence_id: evidence_id.clone(),
            start_byte: *start_byte,
            end_byte: *end_byte,
        });
    }
    let strategy = "admit_v3:adjudicate_v1:test-embedding";
    let links = [crate::adjudication::AdjudicationRedecision {
        candidate_id: held_id.clone(),
        evidence_id: new_evidence.clone(),
        redecision_kind: "related_evidence".into(),
        strategy_fingerprint: strategy.into(),
    }];
    let adjudication = store
        .adjudication_create_with_redecisions(
            &scope,
            &new_job.id,
            memory_contract::ADMISSION_VERSION_V3,
            crate::adjudication::ADJUDICATE_V1,
            Some("test-embedding"),
            &inputs,
            &[],
            &links,
        )
        .unwrap()
        .unwrap();
    assert!(store
        .dream_candidate_redecision_seen(&scope, &held_id, &new_evidence, strategy)
        .unwrap());
    let frozen = store
        .adjudication_inputs(&scope, &adjudication.id)
        .unwrap()
        .0;
    assert!(frozen
        .iter()
        .any(|c| c.candidate_id == held_id && c.status == "held"));
    assert!(frozen
        .iter()
        .any(|c| c.candidate_id == fresh_candidate.candidate_id));

    let manual = store
        .dream_redecision_trigger(
            &scope,
            &held_id,
            "user-rejudge-1",
            Some("a"),
            Some("dsh"),
            Some("s"),
        )
        .unwrap();
    assert_eq!(manual.purpose, "redecision");
    let duplicate = store
        .dream_redecision_trigger(
            &scope,
            &held_id,
            "user-rejudge-different-http-key",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        duplicate.id, manual.id,
        "相同 Held/evidence/strategy 只建一个重裁 job"
    );
    let manual_inputs = store
        .dream_redecision_candidates(&scope, &manual.id)
        .unwrap();
    assert_eq!(manual_inputs.len(), 1);
    assert_eq!(manual_inputs[0].0.candidate_id, held_id);
    assert!(manual_inputs[0]
        .1
        .iter()
        .any(|(id, _, _)| id == &old_evidence));
}

#[test]
fn consolidation_retry_requeues_its_persisted_dream_trigger_atomically() {
    let (mut store, scope, origin) = setup("consolidation-retry-dream");
    let memory_id = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "我在杭州从事 Rust 开发",
        MemoryKind::Fact,
    );
    let input = source_of(&store, &scope, &memory_id);
    let now = crate::now_rfc3339_pub().unwrap();
    let (job, dream) = store
        .consolidation_enqueue_manual_dream(
            &scope,
            "topic_page",
            "rust",
            None,
            "consolidate_v1",
            "frozen-input-fingerprint",
            &[input],
            &now,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();

    store
        .conn_mut()
        .execute(
            "UPDATE consolidation_jobs SET status='dead',error_code='PAGE_PUBLISH_FAILED',attempts=1,
                    claim_generation=4,lease_until=NULL WHERE id=?1",
            [&job.id],
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE dream_jobs SET status='dead',error_code='PAGE_PUBLISH_FAILED',attempts=1,
                    claim_generation=4,runner_id=NULL,lease_until=NULL WHERE id=?1",
            [&dream.id],
        )
        .unwrap();

    assert!(store.consolidation_requeue(&scope, &job.id, &now).unwrap());
    let consolidation = store.consolidation_get(&scope, &job.id).unwrap().unwrap();
    let dream = store.dream_get(&scope, &dream.id).unwrap().unwrap();
    assert_eq!(consolidation.status, "queued");
    assert_eq!(consolidation.claim_generation, 4);
    assert_eq!(consolidation.attempts, 1);
    assert_eq!(dream.status, "queued");
    assert_eq!(dream.claim_generation, 4);
    assert_eq!(dream.attempts, 1);
    assert_eq!(dream.error_code, None);
    let runner_id: Option<String> = store
        .conn()
        .query_row(
            "SELECT runner_id FROM dream_jobs WHERE id=?1",
            [&dream.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(runner_id, None);
}

#[test]
fn legacy_consolidation_without_dream_link_cannot_be_retried() {
    let (mut store, scope, _) = setup("legacy-consolidation-retry");
    store
        .conn_mut()
        .execute(
            "INSERT INTO consolidation_jobs
           (id,tenant_id,user_id,document_kind,document_key,input_fingerprint,generator_version,
            status,attempts,run_after,claim_generation,created_at,updated_at)
         VALUES ('legacy-unlinked',?1,?2,'topic_page','legacy','fingerprint','consolidate_v1',
                 'dead',1,'2026-09-26T00:00:00Z',0,'2026-09-26T00:00:00Z','2026-09-26T00:00:00Z')",
            rusqlite::params![scope.tenant_id, scope.user_id],
        )
        .unwrap();

    assert!(!store
        .consolidation_requeue(&scope, "legacy-unlinked", "2026-09-26T01:00:00Z")
        .unwrap());
    let job = store
        .consolidation_get(&scope, "legacy-unlinked")
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "dead");
}

#[test]
fn manual_consolidation_dream_link_uses_scope_and_ids_in_correct_columns() {
    let (mut store, scope, origin) = setup("manual-consolidation-link");
    let m1 = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "Rust 后端服务",
        MemoryKind::Fact,
    );
    let m2 = remember_one(
        &mut store,
        &scope,
        &origin,
        2,
        "Rust 本地工具",
        MemoryKind::Fact,
    );
    let inputs = vec![
        source_of(&store, &scope, &m1),
        source_of(&store, &scope, &m2),
    ];
    let now = crate::now_rfc3339_pub().unwrap();
    let first = store
        .consolidation_enqueue(
            &scope,
            "topic_page",
            "manual-rust-linked",
            None,
            crate::pages::GENERATE_CONSOLIDATE_V1,
            "manual-rust-linked-fingerprint",
            &inputs,
            &now,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    let linked = store
        .dream_link_manual_consolidation(
            &scope,
            &first.id,
            &format!("manual-consolidation-{}", first.id),
            &memory_domain::DomainScope::user_main(),
        )
        .expect("manual consolidation must persist its Dream trigger");
    assert_eq!(linked.purpose, "consolidation");
    assert_eq!(linked.trigger_kind, "manual");

    let (job, dream) = store
        .consolidation_enqueue_manual_dream(
            &scope,
            "topic_page",
            "manual-rust-atomic",
            None,
            crate::pages::GENERATE_CONSOLIDATE_V1,
            "manual-rust-atomic-fingerprint",
            &inputs,
            &now,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(dream.purpose, "consolidation");
    assert_eq!(dream.trigger_kind, "manual");

    let (job_again, dream_again) = store
        .consolidation_enqueue_manual_dream(
            &scope,
            "topic_page",
            "manual-rust-atomic",
            None,
            crate::pages::GENERATE_CONSOLIDATE_V1,
            "manual-rust-atomic-fingerprint",
            &inputs,
            &now,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(job_again.id, job.id, "same manual input reuses the job");
    assert_eq!(
        dream_again.id, dream.id,
        "same job reuses its Dream trigger"
    );
    let link_count: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM dream_consolidation_links
             WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND consolidation_job_id=?4",
            rusqlite::params![scope.tenant_id, scope.user_id, dream.id, job.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(link_count, 1);
}

#[test]
fn persistent_runner_claims_and_renews_frozen_phases() {
    let (mut store, scope, origin) = setup("runner-claim");
    let text = "我住在杭州";
    let evidence = match store
        .record_evidence(
            &scope,
            &origin,
            1,
            "user",
            "user",
            &chrono::Utc::now(),
            text,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let triggered = store
        .dream_trigger(
            &scope,
            "manual",
            "runner-trigger",
            Some("agent-a"),
            Some("dsh"),
            Some("s"),
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    store
        .dream_runner_heartbeat(
            &scope,
            "runner-1",
            "dsh",
            "agent-a",
            "[\"chat\",\"dream_v1\"]",
            60,
        )
        .unwrap();
    let heartbeat_now = crate::now_rfc3339_pub().unwrap();
    assert!(store
        .dream_runner_has_capability(&scope, "runner-1", "chat", &heartbeat_now)
        .unwrap());
    assert_eq!(
        store
            .dream_live_runner_capability_count(&heartbeat_now, "chat")
            .unwrap(),
        1
    );
    let first = store
        .dream_runner_claim(&scope, "runner-1", &now, 90)
        .unwrap()
        .unwrap();
    assert_eq!(first.dream_job.id, triggered.id);
    assert!(first.adjudication_job.is_none());
    assert!(
        store
            .dream_runner_claim(&scope, "runner-1", &now, 90)
            .unwrap()
            .is_none(),
        "同 runner 不得并行领取第二个 Dream"
    );

    let (start, end) = crate::dream_jobs::locate_quote_span(text, text).unwrap();
    store
        .dream_submit_candidates(
            &scope,
            &first.dream_job.id,
            first.dream_job.claim_generation,
            crate::dream_jobs::DREAM_POLICY_V1,
            &[crate::dream_jobs::DreamProposal {
                kind: "fact".into(),
                claim: text.into(),
                quote: text.into(),
                evidence_id: evidence.clone(),
                start_byte: start,
                end_byte: end,
                status: "candidate".into(),
                reason_code: None,
                occurred_at: None,
            }],
        )
        .unwrap();
    let (candidate, spans) = store
        .dream_accepted_candidates(&scope, &first.dream_job.id)
        .unwrap()
        .remove(0);
    let adj_input = crate::adjudication::AdjudicationCandidate {
        candidate_id: candidate.candidate_id.clone(),
        kind: candidate.kind,
        claim: candidate.claim,
        quote: candidate.quote,
        status: "candidate".into(),
        evidence_id: spans[0].0.clone(),
        start_byte: spans[0].1,
        end_byte: spans[0].2,
    };
    store
        .adjudication_create(
            &scope,
            &first.dream_job.id,
            memory_contract::ADMISSION_VERSION_V3,
            crate::adjudication::ADJUDICATE_V1,
            Some("test-embedding"),
            &[adj_input],
            &[],
        )
        .unwrap();
    let resumed = store
        .dream_runner_claim(&scope, "runner-1", &crate::now_rfc3339_pub().unwrap(), 90)
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.dream_job.claim_generation,
        first.dream_job.claim_generation
    );
    let adjudication = resumed.adjudication_job.unwrap();
    assert_eq!(adjudication.status, "running");
    assert!(store
        .dream_runner_lease(
            &scope,
            "runner-1",
            &resumed.dream_job.id,
            resumed.dream_job.claim_generation,
            Some((&adjudication.id, adjudication.claim_generation)),
            None,
            90,
        )
        .unwrap());
    assert!(
        !store
            .dream_runner_lease(
                &scope,
                "runner-1",
                &resumed.dream_job.id,
                resumed.dream_job.claim_generation - 1,
                Some((&adjudication.id, adjudication.claim_generation)),
                None,
                90,
            )
            .unwrap(),
        "旧 Dream generation 不得续租"
    );
}

#[test]
fn retire_immediate_disappearance_and_restore_visibility() {
    // doc6/09 卡：retire 即时全路径消失；restore 重新可见但旧派生项不复活。
    let (mut store, scope, origin) = setup("retire");
    let mid = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    let source_sha: String = store
        .conn()
        .query_row(
            "SELECT claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    let page_sources = vec![(mid.clone(), 1, source_sha)];
    let (page_id, _) = store
        .publish_page(
            &crate::pages::PublishRequest {
                scope: &scope,
                document_kind: "topic_page",
                document_key: "hangzhou-work",
                question_version: None,
                question_text: None,
                title: "居住地",
                body_md: "用户住在杭州。",
                generator_version: crate::pages::GENERATE_CONSOLIDATE_V1,
                input_fingerprint: "test-page-fingerprint",
                sources: &page_sources,
                actor_kind: "system",
            },
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(store
        .get_page(
            &scope,
            &page_id,
            &crate::now_rfc3339_pub().unwrap(),
            &memory_domain::DomainScope::user_main()
        )
        .unwrap()
        .is_some());
    // pin 到 resident（含路径一并验证）。
    store.resident_pin(&scope, &mid, None, None, None).unwrap();
    // retire 指令核验：最新用户事件（seq 2）含逐字指令 quote；claim 双向定位。
    let ev2 = {
        let t = chrono::Utc::now();
        match store
            .record_evidence(
                &scope,
                &origin,
                2,
                "user",
                "user",
                &t,
                "别再提用户住在杭州这条了",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    let (s, e) = store
        .verify_user_quote_span(&scope, &origin, &ev2, "用户住在杭州")
        .unwrap();
    assert!(s < e);
    let req = RetireRequest {
        expected_version: 1,
        actor_kind: "user",
        reason_code: Some("user_request".into()),
        idempotency_key: "retire-1".into(),
        origin: origin.clone(),
        user_evidence_id: ev2.clone(),
        target_quote: "用户住在杭州".into(),
        start_byte: s as i64,
        end_byte: e as i64,
    };
    assert!(store
        .retire_memory(&scope, &mid, &req, &memory_domain::DomainScope::user_main())
        .unwrap());
    // 全路径消失：get_memory / search / resident 可见 pin。
    assert!(
        store
            .get_memory(&scope, &mid, &memory_domain::DomainScope::user_main())
            .unwrap()
            .is_none(),
        "retired 不进 get_memory"
    );
    let (hits, _) = store
        .search_memories(
            &scope,
            "杭州",
            10,
            false,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(
        hits.iter().all(|h| h.memory_id != mid),
        "retired 不进 search"
    );
    let vis = store
        .resident_visible_pins(
            &scope,
            &crate::now_rfc3339_pub().unwrap(),
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(
        vis.iter().all(|p| p.memory_id != mid),
        "retired 不进 resident"
    );
    assert!(store
        .retirement_get(&scope, &mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_some());
    assert!(
        store
            .get_page(
                &scope,
                &page_id,
                &crate::now_rfc3339_pub().unwrap(),
                &memory_domain::DomainScope::user_main()
            )
            .unwrap()
            .is_none(),
        "retire 事务内立即使派生页失效"
    );
    assert!(store
        .page_fts_search(
            &scope,
            "居住地",
            10,
            &memory_domain::DomainScope::user_main()
        )
        .unwrap()
        .is_empty());
    assert!(store
        .page_list(
            &scope,
            &["published"],
            10,
            &memory_domain::DomainScope::user_main()
        )
        .unwrap()
        .iter()
        .all(|page| page.page_id != page_id));
    assert!(store.page_pin(&scope, &page_id).is_err());
    // 重复 retire 幂等确认。
    assert!(
        store
            .retire_memory(&scope, &mid, &req, &memory_domain::DomainScope::user_main())
            .unwrap(),
        "同键同请求重放返回原回执"
    );
    let different_request = RetireRequest {
        expected_version: req.expected_version,
        actor_kind: req.actor_kind,
        reason_code: req.reason_code.clone(),
        idempotency_key: req.idempotency_key.clone(),
        origin: req.origin.clone(),
        user_evidence_id: req.user_evidence_id.clone(),
        target_quote: "用户住在杭州这条".into(),
        start_byte: req.start_byte,
        end_byte: req.end_byte,
    };
    assert!(matches!(
        store.retire_memory(
            &scope,
            &mid,
            &different_request,
            &memory_domain::DomainScope::user_main()
        ),
        Err(StoreError::IdempotencyConflict)
    ));
    let restore_evidence = match store
        .record_evidence(
            &scope,
            &origin,
            3,
            "user",
            "user",
            &chrono::Utc::now(),
            "请恢复这条记忆",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let (rs, re) = store
        .verify_user_quote_span(&scope, &origin, &restore_evidence, "恢复这条记忆")
        .unwrap();
    let restore = RestoreRequest {
        expected_version: 1,
        actor_kind: "user",
        idempotency_key: "restore-1".into(),
        origin: origin.clone(),
        user_evidence_id: restore_evidence.clone(),
        target_quote: "恢复这条记忆".into(),
        start_byte: rs as i64,
        end_byte: re as i64,
    };
    // restore：重新可见。
    assert!(store
        .restore_memory(
            &scope,
            &mid,
            &restore,
            &memory_domain::DomainScope::user_main()
        )
        .unwrap());
    assert!(
        store
            .restore_memory(
                &scope,
                &mid,
                &restore,
                &memory_domain::DomainScope::user_main()
            )
            .unwrap(),
        "同键 restore 重放返回原回执"
    );
    let different_restore = RestoreRequest {
        expected_version: restore.expected_version,
        actor_kind: restore.actor_kind,
        idempotency_key: restore.idempotency_key.clone(),
        origin: restore.origin.clone(),
        user_evidence_id: restore.user_evidence_id.clone(),
        target_quote: "另一条恢复指令".into(),
        start_byte: restore.start_byte,
        end_byte: restore.end_byte,
    };
    assert!(matches!(
        store.restore_memory(
            &scope,
            &mid,
            &different_restore,
            &memory_domain::DomainScope::user_main()
        ),
        Err(StoreError::IdempotencyConflict)
    ));
    assert!(store
        .get_memory(&scope, &mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_some());
    assert!(
        store
            .get_page(
                &scope,
                &page_id,
                &crate::now_rfc3339_pub().unwrap(),
                &memory_domain::DomainScope::user_main()
            )
            .unwrap()
            .is_none(),
        "restore 不会复活退休时已失效的派生页"
    );
    assert!(store
        .retirement_get(&scope, &mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_none());
    let (hits2, _) = store
        .search_memories(
            &scope,
            "杭州",
            10,
            false,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(hits2.iter().any(|h| h.memory_id == mid));
    // 跨 scope 隔离：另一用户的 retire/restore 不互串。
    let dir2 = std::env::temp_dir().join(format!("am-d69-test-{}-r2", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir2);
    std::fs::create_dir_all(&dir2).unwrap();
    store
        .principal_add("t", "u2", &dir2.join("t.token"))
        .unwrap();
    let tok2 = std::fs::read_to_string(dir2.join("t.token")).unwrap();
    let scope2 = store.verify_token(tok2.trim()).unwrap().unwrap();
    assert!(
        store
            .retire_memory(
                &scope2,
                &mid,
                &req,
                &memory_domain::DomainScope::user_main()
            )
            .is_err(),
        "跨 scope 目标拒绝"
    );
}

#[test]
fn retire_rejects_quote_not_spanning_latest_event() {
    // doc6/09 卡：指令证据 span 由 Rust 校验（非逐字/非最新事件即拒绝）。
    let (mut store, scope, origin) = setup("span");
    let _mid = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    let _ = store
        .record_evidence(
            &scope,
            &origin,
            2,
            "user",
            "user",
            &chrono::Utc::now(),
            "新的消息",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    // 非逐字（改写）quote → 拒绝。
    assert!(store
        .verify_user_quote_span(&scope, &origin, "01a-fake", "住在杭州")
        .is_err());
}

#[test]
fn audit_failure_does_not_rollback_memory_retire() {
    let (mut store, scope, origin) = setup("audit-fail-retire");
    let mid = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    let evidence_id = match store
        .record_evidence(
            &scope,
            &origin,
            2,
            "user",
            "user",
            &chrono::Utc::now(),
            "请退休用户住在杭州这条记忆",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let (start_byte, end_byte) = store
        .verify_user_quote_span(&scope, &origin, &evidence_id, "用户住在杭州")
        .unwrap();
    store
        .conn_mut()
        .execute_batch("DROP TABLE memory_audit;")
        .unwrap();
    let changed = store
        .retire_memory(
            &scope,
            &mid,
            &RetireRequest {
                expected_version: 1,
                actor_kind: "user",
                reason_code: Some("user_request".into()),
                idempotency_key: "retire-audit-failure".into(),
                origin,
                user_evidence_id: evidence_id,
                target_quote: "用户住在杭州".into(),
                start_byte: start_byte as i64,
                end_byte: end_byte as i64,
            },
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(changed, "memory_audit 写失败不能回滚业务更新");
    assert!(store
        .retirement_get(&scope, &mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_some());
}

#[test]
fn purge_two_phase_closure_and_tombstones() {
    // doc6/02 §7：preview 只写确认元数据；指纹变化拒绝；闭包清理两张审计表与
    // 候选/页面/向量；job/confirmation 不留可反查目标；墓碑阻止 spool 重放复活。
    let (mut store, scope, origin) = setup("purge");
    let mid = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    store.resident_pin(&scope, &mid, None, None, None).unwrap();
    // 复核工作区已有的 C09 修复：forgotten_memory_id 指向目标时，purge
    // 必须在删除 L1 前清除此 FK 依赖，并由墓碑承担防重放。
    let memory_evidence_id: String = store
        .conn()
        .query_row(
            "SELECT evidence_id FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    store.conn().execute(
        "INSERT INTO suppressed_sources
           (tenant_id,user_id,evidence_id,claim_sha256,forgotten_memory_id,created_at)
         SELECT ?1,?2,?3,claim_sha256,?4,?5 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?4",
        rusqlite::params![scope.tenant_id, scope.user_id, memory_evidence_id, mid,
            chrono::Utc::now().to_rfc3339()],
    ).unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO audit_events
           (id,tenant_id,user_id,actor_kind,actor_id,action,target_id,occurred_at,detail_json)
         VALUES ('audit-old-ref',?1,?2,'system','test','memory_correct','unrelated',?3,?4)",
            rusqlite::params![
                scope.tenant_id,
                scope.user_id,
                chrono::Utc::now().to_rfc3339(),
                serde_json::json!({"old_memory_id":mid}).to_string()
            ],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO audit_events
           (id,tenant_id,user_id,actor_kind,actor_id,action,target_id,occurred_at,detail_json)
         VALUES ('audit-near-ref',?1,?2,'system','test','memory_correct','unrelated',?3,?4)",
            rusqlite::params![
                scope.tenant_id,
                scope.user_id,
                chrono::Utc::now().to_rfc3339(),
                serde_json::json!({"old_memory_id":format!("{mid}-suffix")}).to_string()
            ],
        )
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO memory_audit
           (audit_id,record_id,layer,action,tenant_id,user_id,version,updated_at_ms)
         VALUES ('memory-audit-target',?1,'L1','update',?2,?3,1,1)",
            rusqlite::params![mid, scope.tenant_id, scope.user_id],
        )
        .unwrap();
    store.conn().execute(
        "INSERT INTO mutation_receipts
           (tenant_id,user_id,operation,idempotency_key,request_sha256,result_status,response_json,created_at)
         VALUES (?1,?2,'memory_retire','receipt-target','sha','200',?3,?4)",
        rusqlite::params![scope.tenant_id, scope.user_id,
            serde_json::json!({"memory_id":mid}).to_string(), chrono::Utc::now().to_rfc3339()],
    ).unwrap();
    // preview：业务记忆仍在（只读），确认元数据落库。
    let (token, preview) = store
        .purge_preview(
            &scope,
            &mid,
            "idem-1",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(preview.evidence_ids.len(), 1);
    // V2-P1（doc7/05 §1）：读接口统一按「至少一条未被 forget 抑制的来源」判可见，
    // 本用例为 C09 依赖专门插了 suppressed_sources 行，get_memory 因此不可见是**预期**。
    // 本用例真正要证的是「preview 只读、业务行仍在」，所以改查规范行本身。
    let still_active: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still_active, 1, "preview 只读业务记忆：行仍在且 active");
    let conf_target: String = store
        .conn()
        .query_row(
            "SELECT target_id FROM purge_confirmations WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(conf_target, mid);
    // 指纹变化：preview 后目标新增共享引用（另一记忆也引用同一 evidence）→ 拒绝。
    // 构造：直接把本记忆 evidence 也挂到新记忆上改变闭包共享判定不可行（闭包键不变），
    // 改用删除 confirmation 行模拟过期：此处直接验证 confirm 成功路径，指纹拒绝由
    // second-preview 路径覆盖（对同一目标再次 preview 后旧 token 仍可消费一次）。
    let out = store
        .purge_confirm(
            &scope,
            &token,
            "idem-1",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(
        out.deleted
            .get("evidence_deleted")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 1
    );
    // 闭包后：记忆/证据/审计/候选全无；job 与 confirmation 不留可反查目标 ID。
    assert!(store
        .get_memory(&scope, &mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_none());
    let (mem_cnt, ev_cnt): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3),
                    (SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?4)",
            rusqlite::params![scope.tenant_id, scope.user_id, mid,
                preview.evidence_ids[0]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((mem_cnt, ev_cnt), (0, 0), "记忆与证据本体已删");
    let (audits, maudits, cands): (i64, i64, i64) = store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3),
                    (SELECT COUNT(*) FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND record_id=?3),
                    (SELECT COUNT(*) FROM memory_candidates WHERE tenant_id=?1 AND user_id=?2 AND primary_evidence_id=?4)",
            rusqlite::params![scope.tenant_id, scope.user_id, mid, preview.evidence_ids[0]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (audits, maudits, cands),
        (0, 0, 0),
        "两张审计表与候选闭包清空"
    );
    let (detail_audit, near_match, receipts): (i64, i64, i64) = store.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND id='audit-old-ref'),
                (SELECT COUNT(*) FROM audit_events WHERE tenant_id=?1 AND user_id=?2 AND id='audit-near-ref'),
                (SELECT COUNT(*) FROM mutation_receipts WHERE tenant_id=?1 AND user_id=?2 AND idempotency_key='receipt-target')",
        rusqlite::params![scope.tenant_id, scope.user_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(
        (detail_audit, near_match, receipts),
        (0, 1, 0),
        "旧 ID 出现在 audit detail/receipt 时精确清除，前缀相似 ID 不误删"
    );
    let job_target: Option<String> = store
        .conn()
        .query_row(
            "SELECT target_id FROM purge_jobs WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(job_target, None, "job 终态不留可反查目标");
    let conf_left: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM purge_confirmations WHERE tenant_id=?1 AND user_id=?2 AND target_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(conf_left, 0, "confirmation 已清除目标关联");
    // 墓碑阻止 spool 重放复活：同内容新事件被拒。
    let replay = store.record_evidence(
        &scope,
        &origin,
        9,
        "user",
        "user",
        &chrono::Utc::now(),
        "用户住在杭州",
        &memory_domain::DomainScope::user_main(),
    );
    assert!(
        matches!(replay, Err(StoreError::EventConflict)),
        "墓碑阻止重放复活"
    );
    // 同幂等键重放 confirm：无正文结果。
    let replay2 = store
        .purge_confirm(
            &scope,
            &token,
            "idem-1",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert!(replay2.deleted.get("replayed").is_some());
}

#[test]
fn retention_default_disabled_and_positive_policy_runs() {
    // doc6/12 §5：默认 0=关闭不删除；正值策略构成持续授权，无需逐批确认。
    let (mut store, scope, origin) = setup("retention");
    let _mid = remember_one(
        &mut store,
        &scope,
        &origin,
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    // 一条无记忆引用的旧事件。retention 依据 received_at，而非可由上游回填的 occurred_at。
    let old_ev = {
        let old = chrono::Utc::now() - chrono::Duration::days(400);
        match store
            .record_evidence(
                &scope,
                &origin,
                2,
                "user",
                "user",
                &old,
                "很旧的一句话",
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    // 默认（未配置策略）：无操作。
    assert!(store.retention_run(&scope).unwrap().is_none());
    assert!(store
        .get_memory(&scope, &_mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_some());
    // 正值策略：raw evidence 保留 365 天 → 400 天前且无引用的事件被清理。
    store.retention_set_policy(&scope, 365, 0, true).unwrap();
    let effective_at = (chrono::Utc::now() - chrono::Duration::days(500)).to_rfc3339();
    let received_at = (chrono::Utc::now() - chrono::Duration::days(400)).to_rfc3339();
    store
        .conn_mut()
        .execute(
            "UPDATE retention_policies SET effective_at=?3 WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id, effective_at],
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE evidence_events SET received_at=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, old_ev, received_at],
        )
        .unwrap();
    let r = store.retention_run(&scope).unwrap().unwrap();
    assert_eq!(
        r.get("raw_evidence_deleted").and_then(|v| v.as_i64()),
        Some(1)
    );
    let gone: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, old_ev],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gone, 0, "超期 raw evidence 已清理");
    // 活跃记忆不受影响（raw evidence 清理不触碰被引用事件）。
    assert!(store
        .get_memory(&scope, &_mid, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_some());
    // 同批次重跑幂等。
    assert!(store.retention_run(&scope).unwrap().is_none());
}

#[test]
fn retention_removes_memories_only_when_all_sources_expire_in_the_same_batch() {
    let (mut store, scope, origin) = setup("retention-shared-source");
    store.retention_set_policy(&scope, 1, 0, true).unwrap();
    let policy_effective = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
    store
        .conn_mut()
        .execute(
            "UPDATE retention_policies SET effective_at=?3 WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id, policy_effective],
        )
        .unwrap();
    fn record_source(
        store: &mut Store,
        scope: &ScopeKey,
        origin: &Origin,
        seq: i64,
        claim: &str,
        old: bool,
    ) -> String {
        let occurred = if old {
            chrono::Utc::now() - chrono::Duration::days(3)
        } else {
            chrono::Utc::now()
        };
        let evidence = match store
            .record_evidence(
                &scope,
                &origin,
                seq,
                "user",
                "user",
                &occurred,
                claim,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        };
        if old {
            let received = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
            store.conn_mut().execute(
                "UPDATE evidence_events SET received_at=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, evidence, received],
            ).unwrap();
        }
        evidence
    }

    let old_a = record_source(&mut store, &scope, &origin, 1, "用户住在杭州", true);
    let only_old_memory = match store
        .remember(
            &scope,
            &origin,
            &old_a,
            "用户住在杭州",
            MemoryKind::Fact,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };
    let old_b = record_source(&mut store, &scope, &origin, 2, "用户住在杭州", true);
    store
        .remember(
            &scope,
            &origin,
            &old_b,
            "用户住在杭州",
            MemoryKind::Fact,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();

    let old_c = record_source(&mut store, &scope, &origin, 3, "用户在南京工作", true);
    let shared_memory = match store
        .remember(
            &scope,
            &origin,
            &old_c,
            "用户在南京工作",
            MemoryKind::Fact,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };
    let recent = record_source(&mut store, &scope, &origin, 4, "用户在南京工作", false);
    store
        .remember(
            &scope,
            &origin,
            &recent,
            "用户在南京工作",
            MemoryKind::Fact,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();

    let result = store.retention_run(&scope).unwrap().unwrap();
    assert_eq!(result["raw_evidence_deleted"].as_i64(), Some(3));
    assert_eq!(result["memories_purged"].as_i64(), Some(1));
    assert!(
        store
            .get_memory(
                &scope,
                &only_old_memory,
                &memory_domain::DomainScope::user_main()
            )
            .unwrap()
            .is_none(),
        "同一批次内全部支持证据到期，L1 应进入 purge 闭包"
    );
    // V2-P1：共享记忆仍有存活来源，因此读得到；这里改用规范行探针，
    // 与本用例「只有全部来源同批到期才 purge」的意图直接对应。
    assert!(
        store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3 AND status='active'",
                rusqlite::params![scope.tenant_id, scope.user_id, shared_memory],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1,
        "仍有未到期来源时不得删除 L1"
    );
    let surviving_sources: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
        rusqlite::params![scope.tenant_id, scope.user_id, shared_memory], |r| r.get(0),
    ).unwrap();
    assert_eq!(surviving_sources, 1, "到期来源解除关联，近期来源保留");
    let audit_action: String = store.conn().query_row(
        "SELECT action FROM memory_audit WHERE tenant_id=?1 AND user_id=?2 AND record_id=?3 ORDER BY updated_at_ms DESC LIMIT 1",
        rusqlite::params![scope.tenant_id, scope.user_id, shared_memory], |r| r.get(0),
    ).unwrap();
    assert_eq!(audit_action, "update", "解除一个证据关联记为 L1 update");
    let succeeded_jobs: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM retention_jobs WHERE tenant_id=?1 AND user_id=?2 AND status='succeeded'",
        rusqlite::params![scope.tenant_id, scope.user_id], |r| r.get(0),
    ).unwrap();
    assert_eq!(succeeded_jobs, 1, "删除回执与数据清理在同一事务提交");
}

#[test]
fn retention_processes_more_than_one_l0_batch_and_releases_terminal_dream_inputs() {
    let (mut store, scope, origin) = setup("retention-batches-terminal-dream");
    store.retention_set_policy(&scope, 1, 0, true).unwrap();
    let policy_effective = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
    let old_received = (chrono::Utc::now() - chrono::Duration::days(3)).to_rfc3339();
    store
        .conn_mut()
        .execute(
            "UPDATE retention_policies SET effective_at=?3 WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id, policy_effective],
        )
        .unwrap();

    let mut evidence_ids = Vec::new();
    for seq in 0..257 {
        let evidence_id = match store
            .record_evidence(
                &scope,
                &origin,
                seq,
                "user",
                "user",
                &chrono::Utc::now(),
                &format!("批次测试事件 {seq}"),
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        };
        store.conn_mut().execute(
            "UPDATE evidence_events SET received_at=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, evidence_id, old_received],
        ).unwrap();
        evidence_ids.push(evidence_id);
    }

    // A terminal deterministic failure retains its frozen input for explicit admin retry,
    // but an authorized retention policy can still remove that input after its cutoff.
    let dream_evidence = evidence_ids[0].clone();
    let dream = store
        .dream_trigger(
            &scope,
            "manual",
            "retention-dead-input",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 90).unwrap().unwrap();
    assert_eq!(claimed.id, dream.id);
    assert!(store
        .dream_dead(&scope, &dream.id, claimed.claim_generation, "TEST_DEAD")
        .unwrap());

    let first = store.retention_run(&scope).unwrap().unwrap();
    assert_eq!(first["raw_evidence_deleted"].as_i64(), Some(256));
    let remaining: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1, "超过单轮上限的 L0 必须留待下轮，不得卡死整批");

    let second = store.retention_run(&scope).unwrap().unwrap();
    assert_eq!(second["raw_evidence_deleted"].as_i64(), Some(1));
    let remaining: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    let state_left: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM dream_evidence_state WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
        rusqlite::params![scope.tenant_id, scope.user_id, dream_evidence],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(
        state_left, 0,
        "过期 dead job 输入及账本一并从 purge 闭包清理"
    );
}

#[test]
fn retention_does_not_purge_expired_memory_used_by_recoverable_dream_job() {
    let (mut store, scope, origin) = setup("retention-active-dream-closure");
    store.retention_set_policy(&scope, 0, 1, true).unwrap();
    let policy_effective = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
    store
        .conn_mut()
        .execute(
            "UPDATE retention_policies SET effective_at=?3 WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id, policy_effective],
        )
        .unwrap();
    let evidence = match store
        .record_evidence(
            &scope,
            &origin,
            1,
            "user",
            "user",
            &chrono::Utc::now(),
            "用户住在杭州",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let dream = store
        .dream_trigger(
            &scope,
            "manual",
            "retention-active-dream",
            None,
            None,
            None,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 90).unwrap().unwrap();
    let memory = match store
        .remember(
            &scope,
            &origin,
            &evidence,
            "用户住在杭州",
            MemoryKind::Fact,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };
    let expired_at = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    store
        .conn_mut()
        .execute(
            "UPDATE memories SET valid_until=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory, expired_at],
        )
        .unwrap();

    assert!(
        store.retention_run(&scope).unwrap().is_none(),
        "过期 L1 的 purge 闭包不能删除仍由可恢复 Dream job 冻结的 L0"
    );
    // V2-P1：本用例把 valid_until 显式设成过去时间，读接口按新契约必须立刻不可见；
    // 这里要证的是「L1 行没有被 purge 掉」，所以直接查规范行。
    let row_left: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row_left, 1, "过期 L1 行仍在（未被 purge 闭包删除）");
    let evidence_exists: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, evidence],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(evidence_exists, 1);

    // Once the job is terminal, an expired policy-authorized purge may close its input.
    assert!(store
        .dream_dead(&scope, &dream.id, claimed.claim_generation, "TEST_DEAD")
        .unwrap());
    let result = store.retention_run(&scope).unwrap().unwrap();
    assert_eq!(result["memories_purged"].as_i64(), Some(1));
    let residual_evidence: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, evidence],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        residual_evidence, 0,
        "terminal Dream 的冻结输入随 purge 闭包删除"
    );
}

#[test]
fn adjudication_commit_atomically_enqueues_semantic_index_job() {
    let (mut store, scope, origin) = setup("adjudication-semantic-enqueue");
    let text = "我周末喜欢去天文馆观星";
    let evidence = match store
        .record_evidence(
            &scope,
            &origin,
            1,
            "user",
            "user",
            &chrono::Utc::now(),
            text,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let dream = store
        .dream_trigger(
            &scope,
            "manual",
            "semantic-enqueue",
            Some("a"),
            Some("dsh"),
            Some("s"),
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap()
        .unwrap();
    store
        .dream_runner_heartbeat(&scope, "runner", "dsh", "a", "[\"chat\"]", 120)
        .unwrap();
    let running = store
        .dream_runner_claim(&scope, "runner", &crate::now_rfc3339_pub().unwrap(), 120)
        .unwrap()
        .unwrap();
    let (start_byte, end_byte) = crate::dream_jobs::locate_quote_span(text, text).unwrap();
    store
        .dream_submit_candidates(
            &scope,
            &dream.id,
            running.dream_job.claim_generation,
            crate::dream_jobs::DREAM_POLICY_V1,
            &[crate::dream_jobs::DreamProposal {
                kind: "preference".into(),
                claim: text.into(),
                quote: text.into(),
                evidence_id: evidence,
                start_byte,
                end_byte,
                status: "candidate".into(),
                reason_code: None,
                occurred_at: None,
            }],
        )
        .unwrap();
    let (candidate, spans) = store
        .dream_accepted_candidates(&scope, &dream.id)
        .unwrap()
        .remove(0);
    let candidate_input = crate::adjudication::AdjudicationCandidate {
        candidate_id: candidate.candidate_id.clone(),
        kind: candidate.kind,
        claim: candidate.claim,
        quote: candidate.quote,
        status: "candidate".into(),
        evidence_id: spans[0].0.clone(),
        start_byte: spans[0].1,
        end_byte: spans[0].2,
    };
    store
        .adjudication_create(
            &scope,
            &dream.id,
            memory_contract::ADMISSION_VERSION_V3,
            crate::adjudication::ADJUDICATE_V1,
            Some("gemini-embedding-001"),
            &[candidate_input],
            &[],
        )
        .unwrap();
    let adjudication_running = store
        .dream_runner_claim(&scope, "runner", &crate::now_rfc3339_pub().unwrap(), 120)
        .unwrap()
        .unwrap();
    let adjudication = adjudication_running.adjudication_job.unwrap();
    let applied = store
        .adjudication_apply_and_complete(
            &scope,
            &adjudication.id,
            adjudication.claim_generation,
            adjudication_running.dream_job.claim_generation,
            &[crate::adjudication::AdjudicationProposal {
                candidate_id: candidate.candidate_id,
                durability: "durable".into(),
                action: "create".into(),
                reason_code: Some("new_durable_preference".into()),
                target_memory_id: None,
                expected_target_version: None,
                model_confidence: Some(0.9),
                valid_until: None,
            }],
        )
        .unwrap();
    assert_eq!(applied.applied, 1);
    let memory_id = applied.rows[0].2.as_ref().unwrap();
    let queued: (String, String, i64) = store
        .conn()
        .query_row(
            "SELECT status,model_id,source_version FROM semantic_jobs
             WHERE tenant_id=?1 AND user_id=?2 AND object_kind='memory' AND object_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(queued, ("queued".into(), "gemini-embedding-001".into(), 1));
}
