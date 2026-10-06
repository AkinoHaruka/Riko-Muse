//! V2-R1 验收测试（doc7/07 §6，全部确定性、无模型调用）。
//!
//! 政策边界：本卡只验证「合法可见记忆 → 关系投影」这条路径；
//! 人物事实的**完整覆盖仍受 THIRD_PARTY 门限制**，那不是本卡能测出来的东西。

use crate::relationships::{EntityRefreshOutcome, Resolution};
use crate::{Store, StoreError};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2r1-test-{}-{tag}", std::process::id()));
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

fn write(
    store: &mut Store,
    scope: &ScopeKey,
    session: &str,
    seq: i64,
    text: &str,
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
    match store
        .remember(scope, &o, &ev, text, MemoryKind::Fact, dom)
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

fn refresh(store: &mut Store, scope: &ScopeKey, dom: &DomainScope) -> EntityRefreshOutcome {
    store.relationship_refresh(scope, dom).unwrap()
}

fn names(store: &Store, scope: &ScopeKey, dom: &DomainScope) -> Vec<String> {
    store
        .relationship_index(scope, dom, 100)
        .unwrap()
        .entities
        .into_iter()
        .map(|e| e.display_name)
        .collect()
}

// ---- 1. 两种句式 ----

#[test]
fn projects_entities_from_both_supported_forms() {
    // doc7/07 §6.1。
    let (mut store, scope) = setup("forms");
    write(
        &mut store,
        &scope,
        "s1",
        1,
        "我妻子叫小雨，她特别喜欢园艺",
        &dom(),
    );
    write(
        &mut store,
        &scope,
        "s2",
        1,
        "老王是我的同事，做后端",
        &dom(),
    );
    let out = refresh(&mut store, &scope, &dom());
    assert_eq!(out.entities, 2, "两种句式各产出一个实体：{out:?}");

    let index = store.relationship_index(&scope, &dom(), 100).unwrap();
    let xiaoyu = index
        .entities
        .iter()
        .find(|e| e.display_name == "小雨")
        .expect("句式 A 应产出小雨");
    assert_eq!(xiaoyu.relation.as_deref(), Some("妻子"));
    assert_eq!(xiaoyu.rank_source, "verified_role");
    assert!(xiaoyu.aliases.contains(&"小雨".to_string()));
    assert!(
        xiaoyu.aliases.contains(&"妻子".to_string()),
        "关系词也作为 role 别名"
    );
    assert!(xiaoyu.detail_ref.starts_with("riko://entity/"));

    // 条目与来源：relationship 分节 + 逐条来源。
    let detail = store
        .relationship_get(&scope, &dom(), &xiaoyu.entity_id, None)
        .unwrap()
        .unwrap();
    assert!(detail.skipped_stale == 0);
    let rel = detail
        .items
        .iter()
        .find(|i| i.section == "relationship")
        .expect("必须有承载关系词的条目");
    assert_eq!(rel.body, "我妻子叫小雨，她特别喜欢园艺");
    assert_eq!(rel.observed_or_inferred, "observed");
    assert!(!rel.sources.is_empty());
    assert_eq!(rel.sources[0].0.is_empty(), false);
}

// ---- 2. 宁缺勿滥 ----

#[test]
fn skips_pronouns_and_unsupported_phrasings() {
    // doc7/07 §6.2。
    let (mut store, scope) = setup("skip");
    write(&mut store, &scope, "s1", 1, "他是我同事", &dom());
    write(&mut store, &scope, "s2", 1, "我房东叫张叔", &dom()); // 房东不在词表
    write(&mut store, &scope, "s3", 1, "我同事是", &dom()); // 没有名字
    write(
        &mut store,
        &scope,
        "s4",
        1,
        "我妻子叫小雨，我同事叫老王",
        &dom(),
    ); // 一句多候选
    let out = refresh(&mut store, &scope, &dom());
    assert_eq!(
        out.entities, 0,
        "代词/未知关系词/无名字/多候选都不建实体：{out:?}"
    );
    assert!(names(&store, &scope, &dom()).is_empty());
}

// ---- 3. 不误并 ----

#[test]
fn similar_names_are_not_merged_and_duplicates_collapse() {
    // doc7/07 §6.3。
    let (mut store, scope) = setup("alias");
    write(&mut store, &scope, "s1", 1, "老王是我的同事", &dom());
    write(&mut store, &scope, "s2", 1, "王老师是我的导师", &dom());
    // 同一实体的第二次出现（同会话下一条）。
    write(
        &mut store,
        &scope,
        "s1",
        2,
        "老王是我的同事，做数据库",
        &dom(),
    );
    let out = refresh(&mut store, &scope, &dom());
    assert_eq!(out.entities, 2, "相似称呼是两个实体：{out:?}");
    let list = names(&store, &scope, &dom());
    assert!(list.contains(&"老王".to_string()) && list.contains(&"王老师".to_string()));
    assert_eq!(
        list.iter().filter(|n| n.as_str() == "老王").count(),
        1,
        "同一 display_name 归并到一个实体，不重复建"
    );
    let index = store.relationship_index(&scope, &dom(), 100).unwrap();
    let laowang = index
        .entities
        .iter()
        .find(|e| e.display_name == "老王")
        .unwrap();
    let aliases: Vec<&String> = laowang.aliases.iter().collect();
    assert_eq!(
        aliases.iter().filter(|a| a.as_str() == "老王").count(),
        1,
        "别名去重"
    );
}

// ---- 4. resolve 三态 ----

#[test]
fn resolve_reports_none_one_and_ambiguous() {
    // doc7/07 §6.4。
    let (mut store, scope) = setup("resolve");
    write(&mut store, &scope, "s1", 1, "老王是我的同事", &dom());
    write(&mut store, &scope, "s2", 1, "王老师是我的导师", &dom());
    refresh(&mut store, &scope, &dom());

    assert!(matches!(
        store
            .relationship_resolve(&scope, &dom(), "查无此人")
            .unwrap(),
        Resolution::None
    ));
    match store.relationship_resolve(&scope, &dom(), "老王").unwrap() {
        Resolution::One(e) => assert_eq!(e.display_name, "老王"),
        other => panic!("期望 one，得到 {other:?}"),
    }
    // 别名词表里的关系词「同事」同时属于**所有**同事实体 → 歧义。
    match store.relationship_resolve(&scope, &dom(), "同事").unwrap() {
        Resolution::Ambiguous(list) => {
            assert!(list.len() >= 1);
        }
        Resolution::One(e) => assert_eq!(e.display_name, "老王"),
        Resolution::None => panic!("同事应至少命中老王"),
    }
    // 解析不做模糊猜测：部分匹配不算命中。
    assert!(matches!(
        store.relationship_resolve(&scope, &dom(), "老").unwrap(),
        Resolution::None
    ));
}

// ---- 5. 来源失效即时屏蔽 ----

#[test]
fn source_change_hides_relationship_items_without_rebuild() {
    // doc7/07 §6.5。
    let (mut store, scope) = setup("stale");
    let mid = write(&mut store, &scope, "s1", 1, "我妻子叫小雨", &dom());
    refresh(&mut store, &scope, &dom());
    let index = store.relationship_index(&scope, &dom(), 100).unwrap();
    let id = index.entities[0].entity_id.clone();
    assert_eq!(
        store
            .relationship_get(&scope, &dom(), &id, None)
            .unwrap()
            .unwrap()
            .skipped_stale,
        0
    );

    store
        .conn()
        .execute(
            "UPDATE memories SET version=2 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
        )
        .unwrap();
    let detail = store
        .relationship_get(&scope, &dom(), &id, None)
        .unwrap()
        .unwrap();
    assert!(detail.skipped_stale >= 1, "来源版本变化后条目即时被屏蔽");
    assert!(detail.items.is_empty());

    // 重建后按新版本重新投影。
    refresh(&mut store, &scope, &dom());
    let index2 = store.relationship_index(&scope, &dom(), 100).unwrap();
    let id2 = index2.entities[0].entity_id.clone();
    let detail2 = store
        .relationship_get(&scope, &dom(), &id2, None)
        .unwrap()
        .unwrap();
    assert_eq!(detail2.skipped_stale, 0);
    assert!(!detail2.items.is_empty());
    assert_eq!(detail2.items[0].sources[0].1, 2, "来源版本必须是当前版本");
}

// ---- 6. purge 闭包 ----

#[test]
fn purge_removes_entities_and_leaves_no_orphans() {
    // doc7/07 §6.6。
    let (mut store, scope) = setup("purge-rel");
    let mid = write(&mut store, &scope, "s1", 1, "我妻子叫小雨", &dom());
    refresh(&mut store, &scope, &dom());
    assert_eq!(names(&store, &scope, &dom()).len(), 1);

    let (token, _) = store
        .purge_preview(&scope, &mid, &"idem-rel".to_string(), &dom())
        .unwrap();
    store
        .purge_confirm(&scope, &token, &"idem-rel".to_string(), &dom())
        .unwrap();

    assert!(
        names(&store, &scope, &dom()).is_empty(),
        "实体不得无来源存活"
    );
    let orphans: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relationship_entities e WHERE e.tenant_id=?1 AND e.user_id=?2
               AND NOT EXISTS (SELECT 1 FROM relationship_items i
                   WHERE i.tenant_id=e.tenant_id AND i.user_id=e.user_id AND i.entity_id=e.id)",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
    let srcs: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relationship_sources WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(srcs, 0);
}

// ---- 7. 域隔离 ----

#[test]
fn entities_are_domain_scoped() {
    // doc7/07 §6.7：side 私有实体不进主索引。
    let (mut store, scope) = setup("rel-domain");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let side = DomainScope::resolve("side_a", true, &[], "side_a");

    write(&mut store, &scope, "s-main", 1, "我妻子叫小雨", &dom());
    write(&mut store, &scope, "s-side", 1, "我同事叫阿泽", &side);
    refresh(&mut store, &scope, &dom());
    refresh(&mut store, &scope, &side);

    let main_names = names(&store, &scope, &dom());
    assert!(main_names.contains(&"小雨".to_string()));
    assert!(
        !main_names.contains(&"阿泽".to_string()),
        "主域索引不得出现 side 私有实体：{main_names:?}"
    );

    // side 读域集含主域，所以两边都看得到；但各自成实体、不合并。
    let side_names = names(&store, &scope, &side);
    assert!(side_names.contains(&"阿泽".to_string()));
    assert!(side_names.contains(&"小雨".to_string()));
    let domains: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT DISTINCT domain_id FROM relationship_entities
                 WHERE tenant_id=?1 AND user_id=?2 ORDER BY domain_id",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![scope.tenant_id, scope.user_id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(domains, vec!["side_a".to_string(), "user_main".to_string()]);
}

// ---- 8. 分级排序 ----

#[test]
fn index_ranks_verified_roles_before_unranked() {
    // doc7/07 §6.8。
    let (mut store, scope) = setup("rank");
    write(&mut store, &scope, "s1", 1, "阿明是我同事", &dom()); // verified_role
    write(&mut store, &scope, "s2", 1, "阿伟最近搬走了", &dom()); // 无关系词 → 不建实体
    refresh(&mut store, &scope, &dom());
    let index = store.relationship_index(&scope, &dom(), 100).unwrap();
    assert!(index
        .entities
        .iter()
        .all(|e| e.rank_source == "verified_role"));
    // 索引预算生效并给出省略计数。
    let small = store.relationship_index(&scope, &dom(), 0).unwrap();
    assert_eq!(small.entities.len(), 0);
    assert_eq!(small.total, index.total);
    assert_eq!(small.omitted, index.total);
    // 版本校验：错的 expected_version 必须冲突而不是静默返回。
    let id = index.entities[0].entity_id.clone();
    assert!(matches!(
        store.relationship_get(&scope, &dom(), &id, Some(99)),
        Err(StoreError::VersionConflict)
    ));
}

// ---- 9. 组表本卡未启用 ----

#[test]
fn group_memberships_stay_empty_in_this_card() {
    // doc7/07 §6.9：如实断言，避免以后把它当成已实现。
    let (mut store, scope) = setup("groups");
    write(
        &mut store,
        &scope,
        "s1",
        1,
        "我妻子叫小雨，我同事叫老王",
        &dom(),
    );
    write(&mut store, &scope, "s2", 1, "周末和朋友们一起爬山", &dom());
    refresh(&mut store, &scope, &dom());
    let n: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM group_memberships WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "V2-R1 不产生群组成员关系（缺证据规则）");
    // 也不产生 group 类实体。
    let groups: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM relationship_entities
             WHERE tenant_id=?1 AND user_id=?2 AND entity_kind='group'",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(groups, 0);
}
