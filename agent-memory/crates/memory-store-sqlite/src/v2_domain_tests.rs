//! V2-S1 记忆域验收测试（doc7/04 §5 七项，全部确定性、无网络）。
//!
//! 覆盖：旧库回填、main/A/B 读写矩阵与授权跨读、同 quote 跨域身份、
//! 域管理错误码、墓碑按域、alignment/页面按域独立版本链、写域 ∈ 读域集。

use crate::{pages, Store, StoreError};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey, USER_MAIN_DOMAIN};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2dom-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    (store, scope)
}

fn origin(session: &str) -> Origin {
    Origin {
        host_id: "dsh".into(),
        agent_id: "a".into(),
        session_id: session.into(),
    }
}

/// 主域上下文：读=写=user_main。
fn dom_main() -> DomainScope {
    DomainScope::user_main()
}

/// side 域上下文（doc7/04 §2.3）：读集 = {D} ∪ {user_main}。
fn dom_side(name: &str) -> DomainScope {
    DomainScope::resolve(name, true, &[], name)
}

/// 在指定域写一条记忆：证据事件与记忆同域（doc7/04 §2.2 一致性闸）。
fn remember_in(
    store: &mut Store,
    scope: &ScopeKey,
    origin: &Origin,
    seq: i64,
    quote: &str,
    kind: MemoryKind,
    dom: &DomainScope,
) -> String {
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(scope, origin, seq, "user", "user", &t, quote, dom)
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store
        .remember(scope, origin, &ev, quote, kind, dom)
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

fn search_ids(store: &Store, scope: &ScopeKey, query: &str, dom: &DomainScope) -> Vec<String> {
    let (hits, _) = store.search_memories(scope, query, 20, false, dom).unwrap();
    hits.into_iter().map(|h| h.memory_id).collect()
}

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

// ---- 1. 旧库升级：主域回填、缺省行为与 14 版本一致 ----

#[test]
fn principal_creation_registers_user_main_domain() {
    // doc7/04 §1.4：旧库由迁移回填；新 principal 由 insert_principal 同事务写入。
    let (store, scope) = setup("backfill");
    let domains = store.domain_list(&scope).unwrap();
    assert_eq!(domains.len(), 1, "新 principal 恰好有一个域注册行");
    assert_eq!(domains[0].domain_id, USER_MAIN_DOMAIN);
    assert_eq!(domains[0].kind, "user_main");
    assert_eq!(domains[0].status, "active");
    assert_eq!(domains[0].policy_version, 1);
    // 幂等：重复 ensure 不产生第二行。
    let mut store = store;
    store.domain_ensure_main(&scope).unwrap();
    assert_eq!(store.domain_list(&scope).unwrap().len(), 1);
}

#[test]
fn legacy_data_is_user_main_and_default_scope_matches_v14() {
    // doc7/04 §5.1：旧数据 domain 全为 user_main；缺省上下文读写正常。
    let (mut store, scope) = setup("legacy");
    let m = remember_in(
        &mut store,
        &scope,
        &origin("s"),
        1,
        "用户住在杭州",
        MemoryKind::Fact,
        &dom_main(),
    );
    let stored: String = store
        .conn()
        .query_row(
            "SELECT domain_id FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, m],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, USER_MAIN_DOMAIN);
    // 缺省读域集只有一个域，越域对象读不到。
    let dom = dom_main();
    assert_eq!(dom.read, vec![USER_MAIN_DOMAIN]);
    assert!(store.get_memory(&scope, &m, &dom).unwrap().is_some());
    assert_eq!(search_ids(&store, &scope, "杭州", &dom), vec![m]);
}

// ---- 2. main / side A / side B 读写矩阵与授权跨读 ----

#[test]
fn main_side_matrix_default_reads_and_authorized_cross_read() {
    // V2 文档 02 §6 / V2 验收 V07：主读不到 side；side 继承主域；授权后才跨读。
    let (mut store, scope) = setup("matrix");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    store.domain_create_side(&scope, "side_b", "test").unwrap();

    let m_main = remember_in(
        &mut store,
        &scope,
        &origin("s-main"),
        1,
        "用户偏好深色主题",
        MemoryKind::Preference,
        &dom_main(),
    );
    let m_a = remember_in(
        &mut store,
        &scope,
        &origin("s-a"),
        1,
        "用户在研究搬家方案",
        MemoryKind::Fact,
        &dom_side("side_a"),
    );
    let m_b = remember_in(
        &mut store,
        &scope,
        &origin("s-b"),
        1,
        "用户在准备生日惊喜",
        MemoryKind::Fact,
        &dom_side("side_b"),
    );

    // 主域：只看得到主域对象（A/B 的内容与存在都不泄露）。
    let main_hits = search_ids(&store, &scope, "用户", &dom_main());
    assert!(main_hits.contains(&m_main));
    assert!(!main_hits.contains(&m_a), "主域不能读到 side A");
    assert!(!main_hits.contains(&m_b), "主域不能读到 side B");
    assert!(
        store
            .get_memory(&scope, &m_a, &dom_main())
            .unwrap()
            .is_none(),
        "猜 ID 直接 get 也不能越域"
    );

    // side A：读到 A + main，读不到 B。
    let a_dom = dom_side("side_a");
    let a_hits = search_ids(&store, &scope, "用户", &a_dom);
    assert!(a_hits.contains(&m_a) && a_hits.contains(&m_main));
    assert!(!a_hits.contains(&m_b), "A 未获授权时读不到 B");

    // 授权 A→B（reader=side_a, granted=side_b）后 A 才能读到 B。
    let (grant, created) = store
        .domain_grant_add(&scope, "side_a", "side_b", "trusted_user", "test")
        .unwrap();
    assert!(created);
    let grants = store.domain_grants_for(&scope, "side_a").unwrap();
    assert_eq!(grants, vec!["side_b".to_string()]);
    let a_dom2 = DomainScope::resolve("side_a", true, &grants, "side_a");
    assert!(search_ids(&store, &scope, "用户", &a_dom2).contains(&m_b));

    // 撤销后立即失效。
    assert!(store.domain_grant_revoke(&scope, &grant.id).unwrap());
    let a_dom3 = DomainScope::resolve(
        "side_a",
        true,
        &store.domain_grants_for(&scope, "side_a").unwrap(),
        "side_a",
    );
    assert!(!search_ids(&store, &scope, "用户", &a_dom3).contains(&m_b));
}

// ---- 3. 同 quote 跨域各自身份；域内更正不污染另一域 ----

#[test]
fn identical_quote_in_main_and_side_keeps_distinct_identity() {
    // doc7/04 §3 / V2 验收 V08：不跨域 dedup、不跨域更新旧版本。
    let (mut store, scope) = setup("dedup");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let quote = "用户喜欢手冲咖啡";

    let m_main = remember_in(
        &mut store,
        &scope,
        &origin("s-main"),
        1,
        quote,
        MemoryKind::Preference,
        &dom_main(),
    );
    let m_a = remember_in(
        &mut store,
        &scope,
        &origin("s-a"),
        1,
        quote,
        MemoryKind::Preference,
        &dom_side("side_a"),
    );
    assert_ne!(m_main, m_a, "同文在两个域是不同的记忆实体");

    // 域内重复写入仍然 dedup（旧行为不变）。
    let m_a2 = remember_in(
        &mut store,
        &scope,
        &origin("s-a"),
        2,
        quote,
        MemoryKind::Preference,
        &dom_side("side_a"),
    );
    assert_eq!(m_a, m_a2, "同域内同文仍走 dedup 路径");

    // A 域内更正：主域同文记忆的 claim/版本必须原样不动。
    let before_main = store
        .get_memory(&scope, &m_main, &dom_main())
        .unwrap()
        .unwrap();
    // correct 要求最新用户事件同时含 old_quote 与 replacement_quote（memories.rs:881-890）。
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(
            &scope,
            &origin("s-a"),
            3,
            "user",
            "user",
            &t,
            "更正：用户喜欢手冲咖啡，其实是用户更喜欢速溶咖啡",
            &dom_side("side_a"),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let outcome = store
        .correct_memory(
            &scope,
            &m_a,
            &crate::memories::CorrectRequest {
                expected_version: 1,
                origin: origin("s-a"),
                user_evidence_id: ev,
                old_quote: quote.to_string(),
                replacement_quote: "用户更喜欢速溶咖啡".to_string(),
            },
            &dom_side("side_a"),
        )
        .unwrap();
    assert_eq!(outcome.old_memory_id, m_a);
    let after_main = store
        .get_memory(&scope, &m_main, &dom_main())
        .unwrap()
        .unwrap();
    assert_eq!(
        after_main.claim, before_main.claim,
        "A 的更正不动 main 的正文"
    );
    assert_eq!(
        after_main.version, before_main.version,
        "A 的更正不动 main 的版本"
    );

    // 跨域写：拿 A 的 memory_id 到主域上下文里操作按不存在处理。
    assert!(matches!(
        store.forget_memory(
            &scope,
            &m_a,
            &crate::memories::ForgetRequest {
                expected_version: 1,
                origin: origin("s-main"),
                user_evidence_id: "whatever".into(),
                target_quote: quote.to_string(),
            },
            &dom_main(),
        ),
        Err(StoreError::MemoryNotFound)
    ));
}

// ---- 4. 域管理错误码：未知名 404、closed 409、绑定冲突 409、重放改域拒绝 ----

#[test]
fn domain_admin_rejections_are_explicit() {
    // doc7/04 §5.4。
    let (mut store, scope) = setup("errors");

    // 未知域。
    assert!(matches!(
        store.domain_require_active(&scope, "nope"),
        Err(StoreError::DomainNotFound)
    ));
    // 保留名。
    assert!(matches!(
        store.domain_create_side(&scope, USER_MAIN_DOMAIN, "x"),
        Err(StoreError::DomainReserved)
    ));
    // 非法域名（路径/大写之外的空格与大写放行规则：只允许 [A-Za-z0-9_-]）。
    assert!(matches!(
        store.domain_create_side(&scope, "side a", "x"),
        Err(StoreError::InvalidDomainId)
    ));

    store.domain_create_side(&scope, "side_a", "test").unwrap();
    store.domain_create_side(&scope, "side_b", "test").unwrap();

    // 绑定：同会话改绑别的域 → 409；同目标重复登记幂等。
    assert!(store
        .domain_binding_put(&scope, "dsh", "s1", "side_a", "trusted_user")
        .unwrap());
    assert!(!store
        .domain_binding_put(&scope, "dsh", "s1", "side_a", "trusted_user")
        .unwrap());
    assert!(matches!(
        store.domain_binding_put(&scope, "dsh", "s1", "side_b", "trusted_user"),
        Err(StoreError::DomainBindingConflict)
    ));
    assert_eq!(
        store.resolve_write_domain(&scope, "dsh", "s1").unwrap(),
        "side_a"
    );
    // 未绑定会话回落到 user_main（普通 DSH 会话默认共享主域）。
    assert_eq!(
        store
            .resolve_write_domain(&scope, "dsh", "unbound")
            .unwrap(),
        USER_MAIN_DOMAIN
    );

    // 关闭域：user_main 不可关；side 可关，关闭后不可再作读/写/绑定目标。
    assert!(matches!(
        store.domain_close(&scope, USER_MAIN_DOMAIN),
        Err(StoreError::DomainReserved)
    ));
    assert!(store.domain_close(&scope, "side_b").unwrap());
    assert!(matches!(
        store.domain_require_active(&scope, "side_b"),
        Err(StoreError::DomainClosed)
    ));
    assert!(matches!(
        store.domain_binding_put(&scope, "dsh", "s9", "side_b", "trusted_user"),
        Err(StoreError::DomainClosed)
    ));

    // 证据重放不得改域。
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(
            &scope,
            &origin("s-replay"),
            1,
            "user",
            "user",
            &t,
            "用户住在成都",
            &dom_side("side_a"),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    assert_eq!(
        store.evidence_domain_get(&scope, &ev).unwrap().as_deref(),
        Some("side_a")
    );
    assert!(matches!(
        store.evidence_domain_set(&scope, &ev, USER_MAIN_DOMAIN),
        Err(StoreError::StateConflict)
    ));
    // 一致性闸：映射域与写域不符时 remember 拒绝。
    assert!(matches!(
        store.remember(
            &scope,
            &origin("s-replay"),
            &ev,
            "用户住在成都",
            MemoryKind::Fact,
            &dom_main()
        ),
        Err(StoreError::StateConflict)
    ));
    // 同域重放幂等。
    store.evidence_domain_set(&scope, &ev, "side_a").unwrap();
}

// ---- 5. 墓碑/抑制按域 ----

#[test]
fn purge_tombstones_are_domain_scoped() {
    // doc7/04 §3/§5.5：purge 作用域=写域；一个域的墓碑不拦另一个域的合法写入。
    let (mut store, scope) = setup("tombstone");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let quote = "用户曾住在广州";

    let m_main = remember_in(
        &mut store,
        &scope,
        &origin("s-main"),
        1,
        quote,
        MemoryKind::Fact,
        &dom_main(),
    );
    let m_a = remember_in(
        &mut store,
        &scope,
        &origin("s-a"),
        1,
        quote,
        MemoryKind::Fact,
        &dom_side("side_a"),
    );

    // 跨域 purge 目标按不存在处理。
    assert!(matches!(
        store.purge_preview(&scope, &m_a, &"idem-x".to_string(), &dom_main()),
        Err(StoreError::MemoryNotFound)
    ));

    // 在 A 域 purge：写 A 域墓碑。
    let (token, preview) = store
        .purge_preview(&scope, &m_a, &"idem-a".to_string(), &dom_side("side_a"))
        .unwrap();
    assert!(preview.dependency_fingerprint.len() > 0);
    store
        .purge_confirm(&scope, &token, &"idem-a".to_string(), &dom_side("side_a"))
        .unwrap();

    let tombstones: Vec<(String, String)> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT domain_id, source_kind FROM purge_tombstones
                 WHERE tenant_id=?1 AND user_id=?2 ORDER BY created_at",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert!(
        tombstones.iter().all(|(d, _)| d == "side_a"),
        "墓碑必须只落在被 purge 的域：{tombstones:?}"
    );

    // 主域同文记忆仍然存在且可读（A 的 purge 不波及 main）。
    assert!(store
        .get_memory(&scope, &m_main, &dom_main())
        .unwrap()
        .is_some());
    assert_eq!(
        search_ids(&store, &scope, "广州", &dom_main()),
        vec![m_main]
    );

    // A 的墓碑不得拦住主域对同一文本的重新写入（doc7/04 §5.5：
    // 「main 墓碑不拦 side 重放」，反向同理；墓碑按域匹配，content_sha256 不跨域生效）。
    let m_main2 = remember_in(
        &mut store,
        &scope,
        &origin("s-main-2"),
        1,
        quote,
        MemoryKind::Fact,
        &dom_main(),
    );
    assert!(!m_main2.is_empty(), "主域可重新写入 A 已 purge 的同文内容");
    assert_eq!(
        store
            .get_memory(&scope, &m_main2, &dom_main())
            .unwrap()
            .unwrap()
            .claim,
        quote
    );
    // 而 A 域自己重放同文仍被墓碑拦住。
    let t2 = chrono::Utc::now();
    let blocked = store.record_evidence(
        &scope,
        &origin("s-a-2"),
        1,
        "user",
        "user",
        &t2,
        quote,
        &dom_side("side_a"),
    );
    assert!(
        matches!(blocked, Err(StoreError::EventConflict)),
        "被 purge 的域不能经重放复活同文事件，实际: {blocked:?}"
    );
}

// ---- 6. alignment 与页面按域独立版本链 ----

#[test]
fn alignment_and_pages_have_per_domain_version_chains() {
    // doc7/04 §1.3/§5.6：派生链按域独立版本；同 key 页面允许各域各自 published。
    let (mut store, scope) = setup("derived");
    store.domain_create_side(&scope, "side_a", "test").unwrap();

    let main_first = store
        .alignment_synthesis_refresh(&scope, &dom_main())
        .unwrap();
    assert_eq!(main_first.version, 1);
    // 无新 rupture/线程且窗口未过 —— 不重复生成（确定性幂等，alignment.rs:529-537）。
    let main_again = store
        .alignment_synthesis_refresh(&scope, &dom_main())
        .unwrap();
    assert_eq!(main_again.version, 1, "无新信号时不推进版本");
    assert_eq!(main_again.body, main_first.body);

    // side A 独立从 1 开始（若共用主域版本链这里会是 2）。
    let a_first = store
        .alignment_synthesis_refresh(&scope, &dom_side("side_a"))
        .unwrap();
    assert_eq!(a_first.version, 1, "side 域版本链与主域互不干扰");

    // side A 产生新 rupture 后只有 A 的链推进：A → 2，主域仍是 1。
    let t_rup = chrono::Utc::now();
    let ev_a = match store
        .record_evidence(
            &scope,
            &origin("align-a"),
            1,
            "user",
            "user",
            &t_rup,
            "你上次又搞错了，我说过不要那样",
            &dom_side("side_a"),
        )
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    let detected = crate::now_rfc3339_pub().unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO rupture_events
               (id, tenant_id, user_id, evidence_id, host_id, session_id, event_seq, signal, cue,
                start_byte, end_byte, thread_id, detected_at, domain_id)
             VALUES (?1, ?2, ?3, ?4, 'dsh', 'align-a', 1, 'correction', '又搞错了', 0, 24, NULL, ?5, 'side_a')",
            rusqlite::params![
                uuid::Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                ev_a,
                detected
            ],
        )
        .unwrap();
    let a_second = store
        .alignment_synthesis_refresh(&scope, &dom_side("side_a"))
        .unwrap();
    assert_eq!(a_second.version, 2, "新 rupture 只推进所在域的版本链");
    assert_eq!(
        store
            .alignment_synthesis_latest(&scope, &dom_main())
            .unwrap()
            .unwrap()
            .version,
        1,
        "side 域的复盘不影响主域版本"
    );

    // 页面：同 document_kind/key 两个域各自发布成功（唯一索引含 domain_id）。
    let m_main = remember_in(
        &mut store,
        &scope,
        &origin("p-main"),
        1,
        "用户关注数据库选型",
        MemoryKind::Fact,
        &dom_main(),
    );
    let m_a = remember_in(
        &mut store,
        &scope,
        &origin("p-a"),
        1,
        "用户在比较两种消息队列",
        MemoryKind::Fact,
        &dom_side("side_a"),
    );
    let src_main = source_of(&store, &scope, &m_main);
    let src_a = source_of(&store, &scope, &m_a);

    let publish = |store: &mut Store,
                   key: &str,
                   title: &str,
                   body: &str,
                   src: &(String, i64, String),
                   dom: &DomainScope| {
        store
            .publish_page(
                &pages::PublishRequest {
                    scope: &scope,
                    document_kind: "topic_page",
                    document_key: key,
                    question_version: None,
                    question_text: None,
                    title,
                    body_md: body,
                    generator_version: "v2-test",
                    input_fingerprint: &format!("fp-{key}-{}", dom.write),
                    sources: std::slice::from_ref(src),
                    actor_kind: "system",
                },
                dom,
            )
            .unwrap()
    };

    let (page_main, _) = publish(
        &mut store,
        "topic:db",
        "数据库选型",
        "主域话题页正文。",
        &src_main,
        &dom_main(),
    );
    let (page_a, _) = publish(
        &mut store,
        "topic:db",
        "消息队列",
        "side A 话题页正文。",
        &src_a,
        &dom_side("side_a"),
    );
    assert_ne!(page_main, page_a, "同 key 跨域各自发布独立页面");

    // 主域读页面看不到 side 页面，反之亦然。
    let now = crate::now_rfc3339_pub().unwrap();
    assert!(store
        .get_page(&scope, &page_main, &now, &dom_main())
        .unwrap()
        .is_some());
    assert!(
        store
            .get_page(&scope, &page_a, &now, &dom_main())
            .unwrap()
            .is_none(),
        "主域读不到 side 页面"
    );
    // side A 的读域集 = {side_a, user_main}（doc7/04 §2.3）：两个域的页面都可见，
    // 但主域上下文只看得见自己的那一页。这同时证明页面按域独立成行、不互相覆盖。
    let a_pages = store
        .page_list(&scope, &[], 50, &dom_side("side_a"))
        .unwrap();
    let a_ids: Vec<&str> = a_pages.iter().map(|p| p.page_id.as_str()).collect();
    assert_eq!(a_ids.len(), 2, "side 继承主域读取：{a_ids:?}");
    assert!(a_ids.contains(&page_a.as_str()) && a_ids.contains(&page_main.as_str()));
    let main_pages = store.page_list(&scope, &[], 50, &dom_main()).unwrap();
    assert_eq!(
        main_pages.len(),
        1,
        "主域只能看到主域页面（domain_id 过滤生效）"
    );
    assert_eq!(main_pages[0].page_id, page_main);
    // 同 key 两行各自独立版本，互不覆盖。
    let a_row = store
        .get_page(&scope, &page_a, &now, &dom_side("side_a"))
        .unwrap()
        .unwrap();
    assert_eq!(a_row.version, 1);
    assert_eq!(a_row.body_md, "side A 话题页正文。");
}

// ---- 7. 写域恒 ∈ 读域集；read_json 供 SQL 绑定 ----

#[test]
fn write_domain_is_always_readable_and_read_json_binds() {
    // doc7/04 §3/§5.7：任何解析出的 DomainScope 都必须满足写域 ∈ 读域集。
    let cases = vec![
        dom_main(),
        dom_side("side_a"),
        DomainScope::resolve("side_a", true, &["side_b".into()], "side_a"),
        // 异常组合：读域选择器与写域不同，仍然并入写域。
        DomainScope::resolve("side_a", true, &[], "side_b"),
    ];
    for dom in &cases {
        assert!(dom.allows_read(&dom.write), "写域必须可读: {dom:?}");
    }

    // read_json 作为 SQL 绑定值真实生效（不是把集合拼进 SQL 文本）。
    let (mut store, scope) = setup("readjson");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let m_main = remember_in(
        &mut store,
        &scope,
        &origin("rj-main"),
        1,
        "用户在读 Rust 源码",
        MemoryKind::Fact,
        &dom_main(),
    );
    let m_a = remember_in(
        &mut store,
        &scope,
        &origin("rj-a"),
        1,
        "用户在读 TypeScript 源码",
        MemoryKind::Fact,
        &dom_side("side_a"),
    );
    let dom = dom_side("side_a");
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT id FROM memories WHERE tenant_id=?1 AND user_id=?2
               AND domain_id IN (SELECT value FROM json_each(?3)) ORDER BY id",
        )
        .unwrap();
    let ids: Vec<String> = stmt
        .query_map(
            rusqlite::params![scope.tenant_id, scope.user_id, dom.read_json()],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        ids.contains(&m_main) && ids.contains(&m_a),
        "read_json 绑定含读写域"
    );
    assert_eq!(ids.len(), 2);

    // 主域上下文只绑出一个域。
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT id FROM memories WHERE tenant_id=?1 AND user_id=?2
               AND domain_id IN (SELECT value FROM json_each(?3)) ORDER BY id",
        )
        .unwrap();
    let main_ids: Vec<String> = stmt
        .query_map(
            rusqlite::params![scope.tenant_id, scope.user_id, dom_main().read_json()],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(main_ids, vec![m_main]);
}
