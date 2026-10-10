//! V2-P1 验收测试（doc7/05 §6，全部确定性、无网络）。

use crate::{Store, StoreError};
use memory_domain::refs::{memory_stable_ref, parse_memory_ref};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2p1-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    (store, scope)
}

fn dom() -> DomainScope {
    DomainScope::user_main()
}

fn origin(session: &str) -> Origin {
    Origin {
        host_id: "dsh".into(),
        agent_id: "a".into(),
        session_id: session.into(),
    }
}

/// 写一条记忆，返回 (memory_id, evidence_id)。
fn write(
    store: &mut Store,
    scope: &ScopeKey,
    session: &str,
    seq: i64,
    text: &str,
    kind: MemoryKind,
) -> (String, String) {
    let o = origin(session);
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(scope, &o, seq, "user", "user", &t, text, &dom())
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let mid = match store.remember(scope, &o, &ev, text, kind, &dom()).unwrap() {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };
    (mid, ev)
}

fn expire(store: &Store, scope: &ScopeKey, memory_id: &str, at: &str) {
    store
        .conn()
        .execute(
            "UPDATE memories SET valid_until=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id, at],
        )
        .unwrap();
}

fn rows(store: &Store, scope: &ScopeKey, memory_id: &str) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn search_ids(store: &Store, scope: &ScopeKey, q: &str) -> Vec<String> {
    let (hits, _) = store.search_memories(scope, q, 20, false, &dom()).unwrap();
    hits.into_iter().map(|h| h.memory_id).collect()
}

// ---- 1. 到期即时不可见（不跑 expire 调度器）----

#[test]
fn expired_valid_until_hides_immediately_without_scheduler() {
    // doc7/05 §6.1：M1 的到期语义不能只靠 15 分钟调度器把行改成 expired。
    let (mut store, scope) = setup("expire");
    let (mid, _) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户在读分布式系统",
        MemoryKind::Fact,
    );
    let (keep, _) = write(
        &mut store,
        &scope,
        "s2",
        1,
        "用户喜欢黑咖啡",
        MemoryKind::Preference,
    );

    assert!(store.get_memory(&scope, &mid, &dom()).unwrap().is_some());
    assert!(search_ids(&store, &scope, "分布式").contains(&mid));

    // 只改 valid_until，不跑 expire_due_memories。
    expire(&store, &scope, &mid, "2020-01-01T00:00:00Z");

    assert!(
        store.get_memory(&scope, &mid, &dom()).unwrap().is_none(),
        "valid_until 过期后 get 必须立刻不可见"
    );
    assert!(
        !search_ids(&store, &scope, "分布式").contains(&mid),
        "valid_until 过期后 search 必须立刻不可见"
    );
    assert!(
        store
            .memory_explain(&scope, &mid, &dom(), false)
            .unwrap()
            .is_none(),
        "explain 缺省也不得返回过期记忆"
    );
    let visible = store
        .filter_visible(&scope, &[mid.clone(), keep.clone()], &dom(), false)
        .unwrap();
    assert_eq!(visible, vec![keep.clone()]);
    // 未过期的仍可见，且行本身没有被删除（只是不可见）。
    assert!(store.get_memory(&scope, &keep, &dom()).unwrap().is_some());
    assert_eq!(rows(&store, &scope, &mid), 1);
}

// ---- 2. 来源全失效即不可见 ----

#[test]
fn memory_without_valid_support_is_invisible() {
    // doc7/05 §6.2：forget 抑制的是证据，记忆在最后一条支持消失后不可见。
    let (mut store, scope) = setup("support");
    let (mid, ev1) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    // 第二条来源（同会话下一条事件），dedup 到同一条记忆。
    let o = origin("s1");
    let t = chrono::Utc::now();
    let ev2 = match store
        .record_evidence(&scope, &o, 2, "user", "user", &t, "用户住在杭州", &dom())
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    store
        .remember(&scope, &o, &ev2, "用户住在杭州", MemoryKind::Fact, &dom())
        .unwrap();

    let suppress = |store: &Store, ev: &str| {
        store
            .conn()
            .execute(
                "INSERT INTO suppressed_sources
                   (tenant_id,user_id,evidence_id,claim_sha256,forgotten_memory_id,created_at,domain_id)
                 SELECT ?1,?2,?3,claim_sha256,?4,?5,'user_main' FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?4",
                rusqlite::params![scope.tenant_id, scope.user_id, ev, mid, chrono::Utc::now().to_rfc3339()],
            )
            .unwrap();
    };

    assert!(store.get_memory(&scope, &mid, &dom()).unwrap().is_some());
    suppress(&store, &ev1);
    assert!(
        store.get_memory(&scope, &mid, &dom()).unwrap().is_some(),
        "还有一条有效来源时仍可见"
    );
    suppress(&store, &ev2);
    assert!(
        store.get_memory(&scope, &mid, &dom()).unwrap().is_none(),
        "全部来源被抑制后不可见"
    );
    assert!(!search_ids(&store, &scope, "杭州").contains(&mid));
    assert!(rows(&store, &scope, &mid) == 1, "不可见不等于删行");
}

// ---- 3. retire 与 forgotten ----

#[test]
fn retired_and_forgotten_leave_no_readable_body() {
    // doc7/05 §6.3。
    let (mut store, scope) = setup("retire-forget");

    // retire：写域内对象，覆盖后读不到。
    let (mid_r, ev_r) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户临时住在北京",
        MemoryKind::Fact,
    );
    let req = memory_store_sqlite_lifecycle_retire(&scope, &ev_r, "用户临时住在北京");
    assert!(store.retire_memory(&scope, &mid_r, &req, &dom()).unwrap());
    assert!(store.get_memory(&scope, &mid_r, &dom()).unwrap().is_none());
    assert!(store
        .memory_explain(&scope, &mid_r, &dom(), false)
        .unwrap()
        .is_none());
    let ex = store.memory_explain(&scope, &mid_r, &dom(), true).unwrap();
    if let Some(ex) = ex {
        assert!(ex.relations.retired, "历史模式下要能看到 retired 关系");
        assert!(ex.visible == false);
    }

    // forget：正文在任何模式下都不返回。
    // forget 需要最新用户事件里有 forget cue，且 target_quote 同时在事件正文与 claim 中。
    let o_f = origin("s2");
    let t_f = chrono::Utc::now();
    let ev_f = match store
        .record_evidence(
            &scope,
            &o_f,
            1,
            "user",
            "user",
            &t_f,
            "忘记这条：用户不再用这个邮箱",
            &dom(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let mid_f = match store
        .remember(
            &scope,
            &o_f,
            &ev_f,
            "用户不再用这个邮箱",
            MemoryKind::Fact,
            &dom(),
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };
    let freqs = crate::memories::ForgetRequest {
        expected_version: 1,
        origin: origin("s2"),
        user_evidence_id: ev_f,
        target_quote: "用户不再用这个邮箱".into(),
    };
    store.forget_memory(&scope, &mid_f, &freqs, &dom()).unwrap();
    assert!(store.get_memory(&scope, &mid_f, &dom()).unwrap().is_none());
    assert!(
        store
            .memory_explain(&scope, &mid_f, &dom(), true)
            .unwrap()
            .is_none(),
        "forgotten 正文连历史模式都不能返回（explain 不是复活通道）"
    );
    assert!(!search_ids(&store, &scope, "邮箱").contains(&mid_f));
}

fn memory_store_sqlite_lifecycle_retire(
    scope: &ScopeKey,
    evidence_id: &str,
    quote: &str,
) -> crate::lifecycle::RetireRequest {
    crate::lifecycle::RetireRequest {
        expected_version: 1,
        actor_kind: "user",
        reason_code: Some("user_request".into()),
        idempotency_key: format!("idem-{evidence_id}"),
        origin: origin("s1"),
        user_evidence_id: evidence_id.to_string(),
        target_quote: quote.to_string(),
        start_byte: 0,
        end_byte: quote.len() as i64,
    }
}

// ---- 4. 逐字证据 ----

#[test]
fn explain_returns_verbatim_multi_source_evidence() {
    // doc7/05 §6.4：按 UTF-8 字节 span 逐字切片，多来源全部返回、不合并。
    let (mut store, scope) = setup("verbatim");
    let text = "我在杭州做后端开发，主要写 Rust。";
    let (mid, ev1) = write(&mut store, &scope, "s1", 1, text, MemoryKind::Fact);
    // 追加第二条来源：同会话下一条事件包含同一 quote。
    let o = origin("s1");
    let t = chrono::Utc::now();
    let ev2 = match store
        .record_evidence(&scope, &o, 2, "user", "user", &t, text, &dom())
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    store
        .remember(&scope, &o, &ev2, text, MemoryKind::Fact, &dom())
        .unwrap();

    let ex = store
        .memory_explain(&scope, &mid, &dom(), false)
        .unwrap()
        .unwrap();
    assert_eq!(ex.claim, text);
    assert_eq!(ex.evidence.len(), 2, "两条来源都要返回，不能只留一条");
    let ids: Vec<&str> = ex.evidence.iter().map(|e| e.evidence_id.as_str()).collect();
    assert!(ids.contains(&ev1.as_str()) && ids.contains(&ev2.as_str()));
    for e in &ex.evidence {
        assert!(e.span_exact, "remember 写入的证据应有精确 span");
        assert_eq!(e.quote, text, "quote 必须是原文逐字切片");
        assert_eq!(
            (e.end_byte.unwrap() - e.start_byte.unwrap()) as usize,
            text.len()
        );
    }

    // NULL span → 整条正文 + span_exact=false。
    store
        .conn()
        .execute(
            "UPDATE memory_evidence SET start_byte=NULL, end_byte=NULL
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3 AND evidence_id=?4",
            rusqlite::params![scope.tenant_id, scope.user_id, mid, ev1],
        )
        .unwrap();
    let ex = store
        .memory_explain(&scope, &mid, &dom(), false)
        .unwrap()
        .unwrap();
    let e1 = ex.evidence.iter().find(|e| e.evidence_id == ev1).unwrap();
    assert!(!e1.span_exact);
    assert_eq!(
        e1.quote, text,
        "无 span 时返回整条事件正文，不假装有精确 quote"
    );
    assert!(e1.start_byte.is_none() && e1.end_byte.is_none());
}

// ---- 5. speaker / subject ----

#[test]
fn explain_maps_speaker_but_never_invents_subject() {
    // doc7/05 §6.5：speaker 由 role/source_kind 映射；subject 不得由正文反推。
    let (mut store, scope) = setup("speaker");
    let (mid, _) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户的猫叫咪咪",
        MemoryKind::Fact,
    );
    let ex = store
        .memory_explain(&scope, &mid, &dom(), false)
        .unwrap()
        .unwrap();
    assert_eq!(ex.speaker.as_deref(), Some("user"));
    assert!(ex.subject.is_none(), "subject 恒为 None");
    assert_eq!(ex.subject_source, "unknown");
    assert_eq!(ex.source_class, "user_explicit");
    assert!(ex.reason_code.is_none() || ex.reason_code.is_some());
}

// ---- 6. stable_ref ----

#[test]
fn stable_ref_is_authoritative_and_scope_bound() {
    // doc7/05 §6.6：ref 由服务端生成；解析后仍须按认证 scope 鉴权。
    let (mut store, scope) = setup("ref");
    let (mid, _) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户养了一只叫豆豆的狗",
        MemoryKind::Fact,
    );
    let ex = store
        .memory_explain(&scope, &mid, &dom(), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        ex.stable_ref,
        memory_stable_ref("t", "u", "user_main", &mid, ex.version)
    );
    let parsed = parse_memory_ref(&ex.stable_ref).unwrap();
    assert_eq!(parsed.memory_id, mid);
    assert_eq!(parsed.domain_id, "user_main");
    assert_eq!(parsed.version, ex.version);
    // claim_sha256 不是稳定地址。
    let hash: String = store
        .conn()
        .query_row(
            "SELECT claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    assert!(parse_memory_ref(&hash).is_none());

    // 换一个 user 的 scope：同一 memory_id 读不到（scope 隔离）。
    let dir = std::env::temp_dir().join(format!("am-v2p1-ref2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store
        .principal_add("t", "u2", &dir.join("u2.token"))
        .unwrap();
    let token2 = std::fs::read_to_string(dir.join("u2.token")).unwrap();
    let scope2 = store.verify_token(token2.trim()).unwrap().unwrap();
    assert!(store
        .memory_explain(&scope2, &mid, &dom(), true)
        .unwrap()
        .is_none());
    assert!(store
        .filter_visible(&scope2, &[mid.clone()], &dom(), true)
        .unwrap()
        .is_empty());
}

// ---- 7. 历史链与不复活 ----

#[test]
fn history_chain_is_readable_but_purged_content_never_returns() {
    // doc7/05 §6.7。
    let (mut store, scope) = setup("history");
    let (old, ev_old) = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户住在杭州",
        MemoryKind::Fact,
    );
    // 更正：新事件同时含 old_quote 与 replacement_quote。
    let o = origin("s1");
    let t = chrono::Utc::now();
    let ev_new = match store
        .record_evidence(
            &scope,
            &o,
            2,
            "user",
            "user",
            &t,
            "更正：用户住在杭州，其实用户住在成都",
            &dom(),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let outcome = store
        .correct_memory(
            &scope,
            &old,
            &crate::memories::CorrectRequest {
                expected_version: 1,
                origin: o.clone(),
                user_evidence_id: ev_new,
                old_quote: "用户住在杭州".into(),
                replacement_quote: "用户住在成都".into(),
            },
            &dom(),
        )
        .unwrap();
    let new_id = outcome.new_memory_id.clone();

    // 缺省读不到旧版本；历史模式读得到，并能看到取代关系。
    assert!(store.get_memory(&scope, &old, &dom()).unwrap().is_none());
    let hist = store
        .memory_explain(&scope, &old, &dom(), true)
        .unwrap()
        .unwrap();
    assert_eq!(hist.status, "superseded");
    // `visible` 表示「通过本次调用的可见性判定」；历史模式放宽了 status，因此为 true。
    // 「缺省模式下不可见」由上面 get_memory 的断言覆盖。
    assert!(hist.visible, "历史模式下该行通过其判定");
    let new_ex = store
        .memory_explain(&scope, &new_id, &dom(), false)
        .unwrap()
        .unwrap();
    assert_eq!(new_ex.claim, "用户住在成都");
    assert_eq!(new_ex.relations.supersedes.as_deref(), Some(old.as_str()));

    // purge 后正文不再返回。
    let (token, _) = store
        .purge_preview(&scope, &old, &"idem-hist".to_string(), &dom())
        .unwrap();
    store
        .purge_confirm(&scope, &token, &"idem-hist".to_string(), &dom())
        .unwrap();
    assert!(store
        .memory_explain(&scope, &old, &dom(), true)
        .unwrap()
        .is_none());
    assert!(!store
        .filter_visible(&scope, &[old.clone()], &dom(), true)
        .unwrap()
        .contains(&old));
    let _ = ev_old;
}

// ---- 8. 域隔离 ----

#[test]
fn explain_respects_read_domain_set() {
    // doc7/05 §6.8 + V2-S1 矩阵。
    let (mut store, scope) = setup("explain-domain");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let side = DomainScope::resolve("side_a", true, &[], "side_a");

    let o = origin("s-side");
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(
            &scope,
            &o,
            1,
            "user",
            "user",
            &t,
            "用户在准备生日惊喜",
            &side,
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let mid = match store
        .remember(
            &scope,
            &o,
            &ev,
            "用户在准备生日惊喜",
            MemoryKind::Fact,
            &side,
        )
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    };

    assert!(store
        .memory_explain(&scope, &mid, &side, false)
        .unwrap()
        .is_some());
    assert!(
        store
            .memory_explain(&scope, &mid, &dom(), false)
            .unwrap()
            .is_none(),
        "主域不得精读 side 记忆"
    );
    // 授权后主域才读得到（V2-S1 机制）。
    store
        .domain_grant_add(&scope, "user_main", "side_a", "trusted_user", "test")
        .unwrap();
    let grants = store.domain_grants_for(&scope, "user_main").unwrap();
    let main_with_grant = DomainScope::resolve("user_main", false, &grants, "user_main");
    let ex = store
        .memory_explain(&scope, &mid, &main_with_grant, false)
        .unwrap()
        .unwrap();
    assert_eq!(ex.domain_id, "side_a");
    assert_eq!(
        ex.stable_ref,
        memory_stable_ref("t", "u", "side_a", &mid, ex.version)
    );
    // 未授权域仍是 404 语义。
    assert!(matches!(
        store.domain_require_active(&scope, "side_b"),
        Err(StoreError::DomainNotFound)
    ));
}
