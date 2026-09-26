//! D6-5 验收测试（doc6/08 卡内要求，全部固定响应/确定性）：
//! 空目录跳过画像、登记后发布、坏 JSON、崩溃恢复、问题版本/归档即时失效、
//! correct/forget 来源失效。真实模型不在本卡范围（D6-7 接通后启动）。

use crate::{pages, Store, StoreError};
use memory_domain::{MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-d65-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    let origin = Origin { host_id: "dsh".into(), agent_id: "a".into(), session_id: "s".into() };
    (store, scope, origin)
}

fn remember_one(
    store: &mut Store,
    scope: &ScopeKey,
    origin: &Origin,
    seq: i64,
    claim: &str,
    kind: MemoryKind,
) -> String {
    use crate::IngestOutcome;
    let t = chrono::Utc::now();
    let ev = match store.record_evidence(scope, origin, seq, "user", "user", &t, claim).unwrap() {
        crate::IngestOutcome::Recorded(id) => id,
        crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store.remember(scope, origin, &ev, claim, kind).unwrap() {
        crate::RememberOutcome::Created { memory_id, .. } => memory_id,
        crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

/// 来源三元组（memory_id, version, sha）。
fn source_of(store: &Store, scope: &ScopeKey, memory_id: &str) -> (String, i64, String) {
    let (v, s): (i64, String) = store
        .conn()
        .query_row(
            "SELECT version, claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (memory_id.to_string(), v, s)
}

#[test]
fn question_catalog_empty_by_default_and_add_gate_publishing() {
    // doc6/02 §3 / doc6/05 §2：问题目录默认空；登记后才能驱动画像。
    let (mut store, scope, origin) = setup("catalog");
    assert!(store.question_list(&scope, None).unwrap().is_empty(), "首版问题目录默认空");
    // 坏键拒绝。
    assert!(matches!(
        store.question_add(&scope, "Bad-Key!", "x", "user_cli"),
        Err(StoreError::InvalidQuestionKey)
    ));
    let v = store.question_add(&scope, "preferred_work_style", "用户长期偏好的工作方式", "user_cli").unwrap();
    assert_eq!(v, 1);
    // 重复 add 拒绝（key 已存在）。
    assert!(matches!(
        store.question_add(&scope, "preferred_work_style", "别的", "user_cli"),
        Err(StoreError::StateConflict)
    ));
    let _ = origin;
    let rows = store.question_list(&scope, Some("active")).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].question_text, "用户长期偏好的工作方式");
}

#[test]
fn publish_page_and_read_path_source_validation() {
    // doc6/05 §4：发布固化来源；读路径复核当前 active/同版/哈希。
    let (mut store, scope, origin) = setup("publish");
    let m1 = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
    let m2 = remember_one(&mut store, &scope, &origin, 2, "用户偏好简短回答", MemoryKind::Preference);
    let s1 = source_of(&store, &scope, &m1);
    let s2 = source_of(&store, &scope, &m2);
    let req = pages::PublishRequest {
        scope: &scope,
        document_kind: "topic_page",
        document_key: "work-profile",
        question_version: None,
        question_text: None,
        title: "工作画像",
        body_md: "用户住在杭州，偏好简短回答。",
        generator_version: pages::GENERATE_CONSOLIDATE_V1,
        input_fingerprint: "fp-1",
        sources: &[s1.clone(), s2.clone()],
        actor_kind: "system",
    };
    let (page_id, v1) = store.publish_page(&req).unwrap();
    assert_eq!(v1, 1);
    let now = crate::now_rfc3339_pub().unwrap();
    let page = store.get_page(&scope, &page_id, &now).unwrap().unwrap();
    assert_eq!(page.status, "published");
    assert_eq!(page.sources.len(), 2);
    // CAS 版本替换：同 key 再发布 → version+1，旧版留 revisions。
    let (_, v2) = store.publish_page(&req).unwrap();
    assert_eq!(v2, 2);
    let rev_count: i64 = store
        .conn()
        .query_row(
            "SELECT count(*) FROM page_revisions WHERE tenant_id=?1 AND user_id=?2 AND page_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, page_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rev_count, 2, "旧版留 revisions");
    // 同 key 唯一 published：published 行数恒 1。
    let published: i64 = store
        .conn()
        .query_row(
            "SELECT count(*) FROM memory_pages WHERE tenant_id=?1 AND user_id=?2
               AND document_kind='topic_page' AND document_key='work-profile' AND status='published'",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(published, 1);
    // 旧版本快照哈希不同 → 过期来源（版本不符）发布拒绝。
    let stale_source = (s1.0.clone(), s1.1 + 100, s1.2.clone());
    let bad = pages::PublishRequest {
        scope: &scope,
        document_kind: "topic_page",
        document_key: "work-profile-bad",
        question_version: None,
        question_text: None,
        title: "t",
        body_md: "b",
        generator_version: pages::GENERATE_CONSOLIDATE_V1,
        input_fingerprint: "fp-2",
        sources: &[stale_source],
        actor_kind: "system",
    };
    assert!(matches!(store.publish_page(&bad), Err(StoreError::StaleInput)));
}

#[test]
fn source_invalidation_on_forget_and_correct() {
    // doc6/05 §4/§5：forget/correct 后引用页立即 stale 且不可见。
    let (mut store, scope, origin) = setup("invalidate");
    let m1 = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
    let m2 = remember_one(&mut store, &scope, &origin, 2, "用户偏好简短回答", MemoryKind::Preference);
    let req = pages::PublishRequest {
        scope: &scope,
        document_kind: "topic_page",
        document_key: "profile",
        question_version: None,
        question_text: None,
        title: "画像",
        body_md: "摘要。",
        generator_version: pages::GENERATE_CONSOLIDATE_V1,
        input_fingerprint: "fp-1",
        sources: &[source_of(&store, &scope, &m1), source_of(&store, &scope, &m2)],
        actor_kind: "system",
    };
    let (page_id, _) = store.publish_page(&req).unwrap();
    let now = crate::now_rfc3339_pub().unwrap();
    assert!(store.get_page(&scope, &page_id, &now).unwrap().is_some());
    // forget m1（v1 契约终态）+ 同事务语义的 stale_pages_for_memory。
    store
        .conn()
        .execute(
            "UPDATE memories SET status='forgotten' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, m1],
        )
        .unwrap();
    store.stale_pages_for_memory(&scope, &m1).unwrap();
    assert!(
        store.get_page(&scope, &page_id, &now).unwrap().is_none(),
        "来源失效后页立即不可见"
    );
    // FTS 索引同步移除。
    let fts: i64 = store
        .conn()
        .query_row("SELECT count(*) FROM page_fts WHERE page_id=?1", rusqlite::params![page_id], |r| r.get(0))
        .unwrap();
    assert_eq!(fts, 0, "失效页索引已移除");
}

#[test]
fn question_version_change_and_archive_invalidate_pages() {
    // doc6/05 §2：问题更新/归档使旧画像立即 stale。
    // 手工放一张 published 画像（代表此前 Dream 产出）；来源须真实存在（读路径复核）。
    let (mut store, scope, origin) = setup("qversion");
    let m1 = remember_one(&mut store, &scope, &origin, 1, "事实一", MemoryKind::Fact);
    let m2 = remember_one(&mut store, &scope, &origin, 2, "事实二", MemoryKind::Fact);
    let v = store.question_add(&scope, "work_style", "用户长期偏好的工作方式", "user_cli").unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO memory_pages
               (id, tenant_id, user_id, document_kind, document_key, question_version,
                question_text, title, body_md, status, version, generator_version,
                input_fingerprint, created_at, updated_at)
             VALUES ('pg1', ?1, ?2, 'mental_model', 'work_style', ?3, '旧定义', '画像', '旧答案',
                     'published', 1, 'mental_model_v1', 'fp', '2026-09-26T00:00:00Z', '2026-09-26T00:00:00Z')",
            rusqlite::params![scope.tenant_id, scope.user_id, v],
        )
        .unwrap();
    for mid in [&m1, &m2] {
        let (ver, sha): (i64, String) = store
            .conn()
            .query_row(
                "SELECT version, claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, mid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO page_sources (tenant_id, user_id, page_id, memory_id, memory_version, claim_sha256)
                 VALUES (?1, ?2, 'pg1', ?3, ?4, ?5)",
                rusqlite::params![scope.tenant_id, scope.user_id, mid, ver, sha],
            )
            .unwrap();
    }
    let now = crate::now_rfc3339_pub().unwrap();
    assert!(store.get_page(&scope, "pg1", &now).unwrap().is_some());
    // 问题更新 → 画像立即 stale。
    let v2 = store
        .question_update(&scope, "work_style", "新定义", v, "user_cli")
        .unwrap();
    assert_eq!(v2, 2);
    assert!(store.get_page(&scope, "pg1", &now).unwrap().is_none(), "旧定义画像立即失效");
    // 归档 → 同样即时失效；重复归档幂等。
    let v3 = store.question_archive(&scope, "work_style", v2, "user_cli").unwrap();
    assert!(store.get_page(&scope, "pg1", &now).unwrap().is_none());
    let v3b = store.question_archive(&scope, "work_style", v3, "user_cli").unwrap();
    assert_eq!(v3b, v3, "重复归档幂等");
    // CAS 冲突。
    assert!(matches!(
        store.question_update(&scope, "work_style", "x", 1, "user_cli"),
        Err(StoreError::VersionConflict)
    ));
}

#[test]
fn consolidation_job_lifecycle_and_idempotency() {
    // doc6/05 §2/§5：入队固化输入、同 key 幂等、claim/lease、坏 JSON dead、
    // 崩溃恢复（过期 running 回 queued）。
    let (mut store, scope, origin) = setup("jobs");
    let m1 = remember_one(&mut store, &scope, &origin, 1, "事实一", MemoryKind::Fact);
    let m2 = remember_one(&mut store, &scope, &origin, 2, "事实二", MemoryKind::Fact);
    let inputs = vec![source_of(&store, &scope, &m1), source_of(&store, &scope, &m2)];
    let now = crate::now_rfc3339_pub().unwrap();
    let job = store
        .consolidation_enqueue(
            &scope, "topic_page", "work-stack", None, pages::GENERATE_CONSOLIDATE_V1,
            "fp-1", &inputs, &now,
        )
        .unwrap();
    assert_eq!(job.status, "queued");
    // 同 key 幂等：返回既有 job。
    let again = store
        .consolidation_enqueue(
            &scope, "topic_page", "work-stack", None, pages::GENERATE_CONSOLIDATE_V1,
            "fp-1", &inputs, &now,
        )
        .unwrap();
    assert_eq!(again.id, job.id, "同指纹返回原 job");
    // claim → running，generation+1；坏 JSON → 确定性 dead。
    let claimed = store.consolidation_claim(&scope, &now, 90).unwrap().unwrap();
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, "running");
    assert_eq!(claimed.claim_generation, 1);
    // 固化输入：重试不漂移。
    let frozen = store.consolidation_inputs(&scope, &job.id).unwrap();
    assert_eq!(frozen.len(), 2);
    let gen = claimed.claim_generation;
    assert!(store.consolidation_dead(&scope, &job.id, gen, "BAD_JSON").unwrap());
    assert_eq!(store.consolidation_get(&scope, &job.id).unwrap().unwrap().status, "dead");
    // 崩溃恢复：过期 running 回 queued（另一个 job）。
    let job2 = store
        .consolidation_enqueue(
            &scope, "topic_page", "other-stack", None, pages::GENERATE_CONSOLIDATE_V1,
            "fp-2", &inputs, &now,
        )
        .unwrap();
    let now2 = crate::now_rfc3339_pub().unwrap();
    let c2 = store.consolidation_claim(&scope, &now2, 90).unwrap().unwrap();
    assert_eq!(c2.id, job2.id);
    // 模拟 lease 过期：直接把 lease_until 置为过去。
    store
        .conn()
        .execute(
            "UPDATE consolidation_jobs SET lease_until='2020-01-01T00:00:00Z' WHERE id=?1",
            rusqlite::params![job2.id],
        )
        .unwrap();
    let recovered = store.consolidation_recover_expired(&now2).unwrap();
    assert_eq!(recovered, 1);
    assert_eq!(store.consolidation_get(&scope, &job2.id).unwrap().unwrap().status, "queued");
    // 心跳续租对非 running 无效。
    assert!(!store.consolidation_heartbeat(&scope, &job2.id, 1, 90).unwrap());
}

#[test]
fn prompt_output_parsers_strict() {
    // doc6/05 §3：严格 JSON（可剥围栏）、字段集固定、范围校验、坏 JSON 确定性失败。
    use pages::{parse_consolidate_output, parse_mental_model_output};
    let mm = r#"{"question_key":"work_style","answer_md":"先结论","source_memory_ids":["m1"]}"#;
    assert_eq!(parse_mental_model_output(mm, "work_style").unwrap().answer_md, "先结论");
    // 围栏可剥。
    let fenced = format!("```json\n{mm}\n```");
    assert_eq!(parse_mental_model_output(&fenced, "work_style").unwrap().answer_md, "先结论");
    // question_key 不匹配 → 拒绝。
    assert!(parse_mental_model_output(mm, "other").is_err());
    // 坏 JSON / 额外字段 / 空来源 → 拒绝。
    assert!(parse_mental_model_output("not json", "work_style").is_err());
    assert!(parse_mental_model_output(
        r#"{"question_key":"work_style","answer_md":"x","source_memory_ids":[],"extra":1}"#,
        "work_style",
    )
    .is_err());
    assert!(parse_mental_model_output(
        r#"{"question_key":"work_style","answer_md":"","source_memory_ids":["m1"]}"#,
        "work_style",
    )
    .is_err());
    let topic = r#"{"pages":[{"topic_key":"work-stack","title":"工作","body_md":"摘要","source_memory_ids":["m1","m2"]}]}"#;
    assert_eq!(parse_consolidate_output(topic).unwrap().pages.len(), 1);
    // 单来源主题页（须 2—20）拒绝。
    assert!(parse_consolidate_output(
        r#"{"pages":[{"topic_key":"k","title":"t","body_md":"b","source_memory_ids":["m1"]}]}"#
    )
    .is_err());
    // >4 页拒绝。
    let five = format!(
        "{{\"pages\":[{}]}}",
        (0..5)
            .map(|i| format!(
                r#"{{"topic_key":"k{i}","title":"t","body_md":"b","source_memory_ids":["m1","m2"]}}"#
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(parse_consolidate_output(&five).is_err());
}
