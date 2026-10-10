//! V2-D1 验收测试（doc7/06 §7，全部确定性、无模型调用）。

use crate::derived::{
    COMPACT_MAX_ITEMS, DOC_COMPACT, FACET_EXPERIENCE, FACET_OPINIONS, FACET_REFLECTIONS,
    FACET_WORLD,
};
use crate::{Store, StoreError};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2d1-test-{}-{tag}", std::process::id()));
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

/// 写一条记忆（证据与记忆同域），返回 memory_id。
fn write(
    store: &mut Store,
    scope: &ScopeKey,
    session: &str,
    seq: i64,
    text: &str,
    kind: MemoryKind,
    dom: &DomainScope,
) -> String {
    let o = origin(session);
    let t = chrono::Utc::now();
    let ev = match store
        .record_evidence(scope, &o, seq, "user", "user", &t, text, dom)
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store.remember(scope, &o, &ev, text, kind, dom).unwrap() {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

fn bodies(store: &Store, scope: &ScopeKey, facet: &str) -> Vec<String> {
    store
        .facet_view(scope, &dom(), facet)
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.body)
        .collect()
}

/// 直接改规范行的 occurred_at（\`remember\` 本身不写这一列）。
fn set_occurred(store: &Store, scope: &ScopeKey, memory_id: &str, at: &str) {
    store
        .conn()
        .execute(
            "UPDATE memories SET occurred_at=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id, at],
        )
        .unwrap();
}

fn set_version(store: &Store, scope: &ScopeKey, memory_id: &str, version: i64) {
    store
        .conn()
        .execute(
            "UPDATE memories SET version=?4 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id, version],
        )
        .unwrap();
}

// ---- 1. 跨 kind 正确归面 ----

#[test]
fn facets_assign_across_kinds() {
    // doc7/06 §7.1。
    let (mut store, scope) = setup("facets");
    let m_episode = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户上周去了成都出差",
        MemoryKind::Episode,
        &dom(),
    );
    let m_fact_time = write(
        &mut store,
        &scope,
        "s2",
        1,
        "用户 2026 年搬到成都",
        MemoryKind::Fact,
        &dom(),
    );
    set_occurred(&store, &scope, &m_fact_time, "2026-03-01T00:00:00Z");
    let m_fact_plain = write(
        &mut store,
        &scope,
        "s3",
        1,
        "用户的猫叫咪咪",
        MemoryKind::Fact,
        &dom(),
    );
    let m_pref = write(
        &mut store,
        &scope,
        "s4",
        1,
        "用户喜欢黑咖啡",
        MemoryKind::Preference,
        &dom(),
    );
    let m_instr_pref_marker = write(
        &mut store,
        &scope,
        "s5",
        1,
        "回答我时更喜欢简短一点",
        MemoryKind::Instruction,
        &dom(),
    );
    let m_instr_plain = write(
        &mut store,
        &scope,
        "s6",
        1,
        "以后用中文回答",
        MemoryKind::Instruction,
        &dom(),
    );

    store.derived_refresh(&scope, &dom()).unwrap();

    let experience = bodies(&store, &scope, FACET_EXPERIENCE);
    assert!(
        experience.contains(&"用户上周去了成都出差".to_string()),
        "episode 进 experience"
    );
    assert!(
        experience.contains(&"用户 2026 年搬到成都".to_string()),
        "有时间的事实也进 experience（不是 kind 一对一映射）"
    );
    assert!(!experience.contains(&"用户的猫叫咪咪".to_string()));

    let world = bodies(&store, &scope, FACET_WORLD);
    assert!(world.contains(&"用户的猫叫咪咪".to_string()));
    assert!(
        world.contains(&"用户 2026 年搬到成都".to_string()),
        "同一条可进多个分面"
    );
    assert!(world.contains(&"以后用中文回答".to_string()));

    let opinions = bodies(&store, &scope, FACET_OPINIONS);
    assert!(opinions.contains(&"用户喜欢黑咖啡".to_string()));
    assert!(
        opinions.contains(&"回答我时更喜欢简短一点".to_string()),
        "instruction 也能表达偏好"
    );
    assert!(
        !opinions.contains(&"以后用中文回答".to_string()),
        "普通 instruction 不是偏好"
    );

    // 每条都带来源，且来源指向真实记忆。
    let view = store.facet_view(&scope, &dom(), FACET_WORLD).unwrap();
    for item in &view.items {
        assert!(!item.sources.is_empty(), "派生条目必须至少一个来源");
        assert_eq!(
            item.observed_or_inferred, "observed",
            "V2-D1 只产生确定性摘录"
        );
        assert_eq!(item.generator_version, "facet_v1");
    }
    let _ = (
        m_episode,
        m_fact_plain,
        m_pref,
        m_instr_pref_marker,
        m_instr_plain,
    );
}

// ---- 2. 反思必须有真实取代边 ----

#[test]
fn reflections_require_a_supersede_edge() {
    // doc7/06 §7.2：有 supersedes 链不自动等于反思。
    let (mut store, scope) = setup("reflections");
    let old = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户住在杭州",
        MemoryKind::Fact,
        &dom(),
    );
    // 人工造一条 status=superseded 但**没有** relations 边的行。
    let no_edge = write(
        &mut store,
        &scope,
        "s2",
        1,
        "用户在用旧手机",
        MemoryKind::Fact,
        &dom(),
    );
    store
        .conn()
        .execute(
            "UPDATE memories SET status='superseded' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, no_edge],
        )
        .unwrap();

    // 真更正：产生 memory_relations 取代边。
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
    store
        .correct_memory(
            &scope,
            &old,
            &crate::memories::CorrectRequest {
                expected_version: 1,
                origin: o,
                user_evidence_id: ev_new,
                old_quote: "用户住在杭州".into(),
                replacement_quote: "用户住在成都".into(),
            },
            &dom(),
        )
        .unwrap();

    store.derived_refresh(&scope, &dom()).unwrap();
    let reflections = bodies(&store, &scope, FACET_REFLECTIONS);
    assert!(
        reflections.contains(&"用户住在杭州".to_string()),
        "被真更正且带取代边的旧条目进反思"
    );
    assert!(
        !reflections.contains(&"用户在用旧手机".to_string()),
        "只有 superseded 状态、没有取代边的不算反思"
    );
}

// ---- 3. compact 预算、去重与可重建 ----

#[test]
fn compact_respects_budget_dedups_resident_and_rebuilds() {
    // doc7/06 §7.3。
    let (mut store, scope) = setup("compact");
    let mut ids = Vec::new();
    for i in 0..30 {
        let text = format!("用户偏好条目编号 {i:02} 的内容");
        ids.push(write(
            &mut store,
            &scope,
            &format!("s{i}"),
            1,
            &text,
            MemoryKind::Preference,
            &dom(),
        ));
    }
    // pin 第一条：compact 必须去重掉它。
    store
        .resident_pin(&scope, &ids[0], None, None, None)
        .unwrap();

    let first = store.derived_refresh(&scope, &dom()).unwrap();
    assert_eq!(first.batch_version, 1);
    let view = store.compact_view(&scope, &dom()).unwrap();
    assert!(view.items.len() <= COMPACT_MAX_ITEMS, "条数预算生效");
    assert!(
        view.items
            .iter()
            .all(|i| i.body.chars().count() <= crate::derived::COMPACT_MAX_CHARS),
        "不截断单条正文（超预算即停而不是切正文）"
    );
    assert!(
        !view.items.iter().any(|i| i.sources[0].0 == ids[0]),
        "Resident 已 pin 的条目在 compact 里去重"
    );
    assert!(view.total_chars <= view.budget_chars);

    // 同输入重建：内容相同、版本递增、不产生重复行。
    let bodies_before: Vec<String> = view.items.iter().map(|i| i.body.clone()).collect();
    let second = store.derived_refresh(&scope, &dom()).unwrap();
    assert_eq!(second.batch_version, 2);
    let view2 = store.compact_view(&scope, &dom()).unwrap();
    let bodies_after: Vec<String> = view2.items.iter().map(|i| i.body.clone()).collect();
    assert_eq!(bodies_before, bodies_after, "同冻结输入重建内容稳定");
    let rows: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM derived_items WHERE tenant_id=?1 AND user_id=?2 AND document_kind=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, DOC_COMPACT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows as usize,
        bodies_after.len(),
        "整体替换，不留上一批残留行"
    );
}

// ---- 4. 来源变化即时屏蔽 ----

#[test]
fn source_change_hides_item_without_rebuild() {
    // doc7/06 §7.4：不等下一次重建。
    let (mut store, scope) = setup("stale");
    let mid = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户住在成都",
        MemoryKind::Fact,
        &dom(),
    );
    store.derived_refresh(&scope, &dom()).unwrap();
    let before = bodies(&store, &scope, FACET_WORLD);
    assert!(before.contains(&"用户住在成都".to_string()));
    assert_eq!(
        store
            .facet_view(&scope, &dom(), FACET_WORLD)
            .unwrap()
            .skipped_stale,
        0
    );

    // 来源版本前移（模拟被更正/强化）：**不重建**。
    set_version(&store, &scope, &mid, 2);
    let after = store.facet_view(&scope, &dom(), FACET_WORLD).unwrap();
    assert!(
        !after.items.iter().any(|i| i.body == "用户住在成都"),
        "来源变化后读路径立刻不再返回该条目"
    );
    assert!(after.skipped_stale >= 1, "被屏蔽的条目要计数，便于诊断");

    // 重建：来源仍然有效（只是版本前移），条目按新来源版本重新派生出来。
    store.derived_refresh(&scope, &dom()).unwrap();
    let rebuilt = store.facet_view(&scope, &dom(), FACET_WORLD).unwrap();
    let item = rebuilt
        .items
        .iter()
        .find(|i| i.body == "用户住在成都")
        .expect("来源仍有效，重建后应重新派生");
    assert_eq!(
        item.sources[0].1, 2,
        "重建记录的来源版本必须是来源当前版本，不能沿用旧版本"
    );
    assert_eq!(rebuilt.skipped_stale, 0);

    // 来源真正失效（到期）后重建：条目从候选集消失。
    store
        .conn()
        .execute(
            "UPDATE memories SET valid_until='2020-01-01T00:00:00Z' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
        )
        .unwrap();
    store.derived_refresh(&scope, &dom()).unwrap();
    assert!(
        !bodies(&store, &scope, FACET_WORLD).contains(&"用户住在成都".to_string()),
        "来源到期后重建不得再产生该条目"
    );
}

// ---- 5. purge 闭包 ----

#[test]
fn purge_removes_items_and_leaves_no_orphans() {
    // doc7/06 §7.5。
    let (mut store, scope) = setup("purge-derived");
    let mid = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户曾住在广州",
        MemoryKind::Fact,
        &dom(),
    );
    store.derived_refresh(&scope, &dom()).unwrap();
    assert!(bodies(&store, &scope, FACET_WORLD).contains(&"用户曾住在广州".to_string()));

    let (token, _) = store
        .purge_preview(&scope, &mid, &"idem-derived".to_string(), &dom())
        .unwrap();
    store
        .purge_confirm(&scope, &token, &"idem-derived".to_string(), &dom())
        .unwrap();

    // purge 闭包内已经清掉来源行与零来源条目。
    let sources: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM derived_item_sources WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sources, 0);
    let orphans: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM derived_items d WHERE d.tenant_id=?1 AND d.user_id=?2
               AND NOT EXISTS (SELECT 1 FROM derived_item_sources s
                   WHERE s.tenant_id=d.tenant_id AND s.user_id=d.user_id AND s.item_id=d.id)",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0, "不允许没有来源的孤立派生条目");
    assert!(!bodies(&store, &scope, FACET_WORLD).contains(&"用户曾住在广州".to_string()));
}

// ---- 6. 域隔离 ----

#[test]
fn derived_views_are_domain_scoped() {
    // doc7/06 §7.6。
    let (mut store, scope) = setup("derived-domain");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let side = DomainScope::resolve("side_a", true, &[], "side_a");

    write(
        &mut store,
        &scope,
        "s-main",
        1,
        "用户在准备年度汇报",
        MemoryKind::Fact,
        &dom(),
    );
    write(
        &mut store,
        &scope,
        "s-side",
        1,
        "用户在策划生日惊喜",
        MemoryKind::Fact,
        &side,
    );

    store.derived_refresh(&scope, &dom()).unwrap();
    store.derived_refresh(&scope, &side).unwrap();

    let main_world = bodies(&store, &scope, FACET_WORLD);
    assert!(main_world.contains(&"用户在准备年度汇报".to_string()));
    assert!(
        !main_world.contains(&"用户在策划生日惊喜".to_string()),
        "主域读不到 side 派生条目"
    );

    // side 读域集 = {side_a, user_main}：两个域的条目都可见，但按域各自成文。
    let side_world = store.facet_view(&scope, &side, FACET_WORLD).unwrap();
    let side_bodies: Vec<String> = side_world.items.iter().map(|i| i.body.clone()).collect();
    assert!(side_bodies.contains(&"用户在策划生日惊喜".to_string()));
    assert!(side_bodies.contains(&"用户在准备年度汇报".to_string()));
    // 条目自身仍归属各自域（不跨域合并成一条）。
    let domains: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT DISTINCT domain_id FROM derived_items
                 WHERE tenant_id=?1 AND user_id=?2 AND document_kind=?3 ORDER BY domain_id",
            )
            .unwrap();
        let rows = stmt
            .query_map(
                rusqlite::params![scope.tenant_id, scope.user_id, FACET_WORLD],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(domains, vec!["side_a".to_string(), "user_main".to_string()]);
}

// ---- 7. 只读投影的 manifest ----

#[test]
fn export_entries_match_readable_items() {
    // doc7/06 §7.7。
    let (mut store, scope) = setup("export");
    let mid = write(
        &mut store,
        &scope,
        "s1",
        1,
        "用户喜欢喝手冲咖啡",
        MemoryKind::Preference,
        &dom(),
    );
    write(
        &mut store,
        &scope,
        "s2",
        1,
        "用户住在南京",
        MemoryKind::Fact,
        &dom(),
    );
    store.derived_refresh(&scope, &dom()).unwrap();

    let (entries, batch) = store.derived_export_entries(&scope, &dom()).unwrap();
    assert!(batch >= 1);
    let compact_entry = entries
        .iter()
        .find(|e| e.document_kind == DOC_COMPACT)
        .unwrap();
    assert_eq!(
        compact_entry.item_count,
        store.compact_view(&scope, &dom()).unwrap().items.len()
    );
    let world_entry = entries
        .iter()
        .find(|e| e.document_kind == FACET_WORLD)
        .unwrap();
    assert_eq!(
        world_entry.item_count,
        store
            .facet_view(&scope, &dom(), FACET_WORLD)
            .unwrap()
            .items
            .len()
    );

    let id = store
        .derived_write_manifest(&scope, &dom(), batch, &entries, "complete")
        .unwrap();
    let (outcome, manifest): (String, String) = store
        .conn()
        .query_row(
            "SELECT outcome, manifest_json FROM derived_exports WHERE id=?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(outcome, "complete");
    assert!(manifest.contains("COMPACT.md"));
    assert!(manifest.contains("bank/world.md"));

    // 来源失效后重算：manifest 的条目数必须跟着下降，导出不含失效条目正文。
    set_version(&store, &scope, &mid, 9);
    let (entries2, batch2) = store.derived_export_entries(&scope, &dom()).unwrap();
    let world2 = entries2
        .iter()
        .find(|e| e.document_kind == FACET_WORLD)
        .unwrap();
    assert_eq!(
        world2.item_count,
        store
            .facet_view(&scope, &dom(), FACET_WORLD)
            .unwrap()
            .items
            .len()
    );
    assert!(batch2 >= batch);
    let view = store.facet_view(&scope, &dom(), FACET_WORLD).unwrap();
    assert!(!view.items.iter().any(|i| i.body == "用户喜欢喝手冲咖啡"));
    // 未知分面名必须被拒绝，不能当成合法查询。
    assert!(matches!(
        store.facet_view(&scope, &dom(), "facet_bogus"),
        Err(StoreError::InvalidPageField)
    ));
}
