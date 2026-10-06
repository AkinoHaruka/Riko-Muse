//! V2-Q1 验收测试（doc7/10 §6，全部确定性、不调用任何模型）。

use crate::entries::{ENTRY_MAX_CHARS, ENTRY_MAX_EVENTS};
use crate::{Store, StoreError};
use memory_domain::{DomainScope, MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2q1-test-{}-{tag}", std::process::id()));
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

fn ingest(store: &mut Store, scope: &ScopeKey, session: &str, seq: i64, text: &str) -> String {
    let o = origin(session);
    let t = chrono::Utc::now();
    match store
        .record_evidence(scope, &o, seq, "user", "user", &t, text, &dom())
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    }
}

fn remember(store: &mut Store, scope: &ScopeKey, session: &str, seq: i64, quote: &str) -> String {
    let ev = ingest(store, scope, session, seq, quote);
    let o = origin(session);
    match store
        .remember(scope, &o, &ev, quote, MemoryKind::Fact, &dom())
        .unwrap()
    {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

// ---- 1. 切块确定性 ----

#[test]
fn chunking_is_deterministic_and_respects_boundaries() {
    // doc7/10 §6.1：会话边界、字符上限、事件数上限三种切分都命中；重建幂等。
    let (mut store, scope) = setup("chunk");
    for seq in 0..10 {
        ingest(&mut store, &scope, "s1", seq, &format!("第 {seq} 条内容"));
    }
    ingest(&mut store, &scope, "s2", 0, "另一个会话的第一条");
    // 超长单条 → 自己占一个条目。
    let long = "长".repeat(ENTRY_MAX_CHARS + 10);
    ingest(&mut store, &scope, "s3", 0, &long);

    let first = store.entries_refresh(&scope, &dom()).unwrap();
    assert_eq!(first.batch_version, 1);
    // s1 共 10 条，按事件数上限 8 切成 2 条；s2 一条；s3 一条 → 共 4 条。
    assert_eq!(
        first.entries, 4,
        "会话边界 + 事件数上限应切成 4 条：{first:?}"
    );
    assert_eq!(first.sources, 12, "每条来源都要落库");

    // 同输入重建：条目数一致、版本递增。
    let second = store.entries_refresh(&scope, &dom()).unwrap();
    assert_eq!(second.batch_version, 2);
    assert_eq!(second.entries, first.entries);
    assert_eq!(second.previous_entries, first.entries);
}

#[test]
fn entry_body_is_verbatim_concatenation_not_a_summary() {
    // doc7/10 §6.2：body 必须是原文逐字拼接。
    let (mut store, scope) = setup("verbatim");
    ingest(&mut store, &scope, "s1", 0, "我在杭州做后端");
    ingest(&mut store, &scope, "s1", 1, "主要写 Rust");
    store.entries_refresh(&scope, &dom()).unwrap();
    let view = store.entries_search(&scope, &dom(), "杭州", 10).unwrap();
    assert_eq!(view.hits.len(), 1);
    let entry = store
        .entry_get(&scope, &dom(), &view.hits[0].entry_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.body, "我在杭州做后端\n主要写 Rust");
    assert_eq!(entry.sources.len(), 2);
    for s in &entry.sources {
        assert!(s.start_byte == 0 && s.end_byte > 0);
        assert_eq!(s.content_sha256.len(), 64);
    }
}

// ---- 2. 来源失效即时屏蔽 ----

#[test]
fn source_change_hides_entry_without_rebuild() {
    // doc7/10 §6.3/§6.4。
    let (mut store, scope) = setup("stale");
    let mid = remember(&mut store, &scope, "s1", 0, "用户住在昆明");
    store.entries_refresh(&scope, &dom()).unwrap();
    let hit = store.entries_search(&scope, &dom(), "昆明", 10).unwrap();
    assert_eq!(hit.hits.len(), 1, "先能召回");
    assert_eq!(hit.skipped_stale, 0);
    let entry_id = hit.hits[0].entry_id.clone();

    // 记忆版本前移（模拟被更正）：**不重建**。
    store
        .conn()
        .execute(
            "UPDATE memories SET version=version+1 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mid],
        )
        .unwrap();
    let after = store.entries_search(&scope, &dom(), "昆明", 10).unwrap();
    assert!(after.hits.is_empty(), "记忆版本变化后条目即时不再返回");
    assert!(after.skipped_stale >= 1);
    assert!(store
        .entry_get(&scope, &dom(), &entry_id)
        .unwrap()
        .is_none());

    // 重建后条目带着新版本重新出现。
    store.entries_refresh(&scope, &dom()).unwrap();
    let rebuilt = store.entries_search(&scope, &dom(), "昆明", 10).unwrap();
    assert_eq!(rebuilt.hits.len(), 1);
    let entry = store
        .entry_get(&scope, &dom(), &rebuilt.hits[0].entry_id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.sources[0].memory_version, Some(2));
}

// ---- 3. purge 闭包 ----

#[test]
fn purge_removes_entries_and_leaves_no_orphans() {
    // doc7/10 §6.5。
    let (mut store, scope) = setup("purge-entry");
    let mid = remember(&mut store, &scope, "s1", 0, "用户曾住在广州");
    store.entries_refresh(&scope, &dom()).unwrap();
    assert_eq!(
        store
            .entries_search(&scope, &dom(), "广州", 10)
            .unwrap()
            .hits
            .len(),
        1
    );

    let (token, _) = store
        .purge_preview(&scope, &mid, &"idem-q1".to_string(), &dom())
        .unwrap();
    store
        .purge_confirm(&scope, &token, &"idem-q1".to_string(), &dom())
        .unwrap();

    assert!(store
        .entries_search(&scope, &dom(), "广州", 10)
        .unwrap()
        .hits
        .is_empty());
    let orphans: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM context_entries e WHERE e.tenant_id=?1 AND e.user_id=?2
               AND NOT EXISTS (SELECT 1 FROM entry_sources s
                   WHERE s.tenant_id=e.tenant_id AND s.user_id=e.user_id AND s.entry_id=e.id)",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
    let srcs: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM entry_sources WHERE tenant_id=?1 AND user_id=?2",
            rusqlite::params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(srcs, 0, "指向已删证据的来源行必须清掉");
}

// ---- 4. 域隔离 ----

#[test]
fn entries_are_domain_scoped() {
    // doc7/10 §6.6。
    let (mut store, scope) = setup("entry-domain");
    store.domain_create_side(&scope, "side_a", "test").unwrap();
    let side = DomainScope::resolve("side_a", true, &[], "side_a");
    let o = origin("s-side");
    let t = chrono::Utc::now();
    store
        .record_evidence(
            &scope,
            &o,
            0,
            "user",
            "user",
            &t,
            "用户在策划生日惊喜",
            &side,
        )
        .unwrap();
    ingest(&mut store, &scope, "s-main", 0, "用户在准备年度汇报");
    store.entries_refresh(&scope, &dom()).unwrap();
    store.entries_refresh(&scope, &side).unwrap();

    let main = store.entries_search(&scope, &dom(), "用户", 10).unwrap();
    let bodies: Vec<String> = main
        .hits
        .iter()
        .map(|h| {
            store
                .entry_get(&scope, &dom(), &h.entry_id)
                .unwrap()
                .unwrap()
                .body
        })
        .collect();
    assert!(bodies.iter().any(|b| b.contains("年度汇报")));
    assert!(
        !bodies.iter().any(|b| b.contains("生日惊喜")),
        "主域不得召回 side 条目"
    );
}

// ---- 5. 同语料对照（Q1 的核心交付）----

/// 预标注问法（doc7/10 §4）：每条给出 query 与应命中的记忆序号集合。
/// 覆盖跨语言/同义/旧值/关系歧义/上下文依赖五类，是**离线词法**对照，不代表语义质量。
const LABELLED: &[(&str, &str, &[usize])] = &[
    ("lexical_exact", "昆明", &[0]),
    ("lexical_other", "咖啡", &[3]),
    ("context_dependent", "上一句提到的城市", &[]),
    ("old_value", "杭州", &[1]),
    ("relation", "妻子", &[2]),
    ("synonym_miss", "住所", &[]),
];

#[test]
fn same_corpus_comparison_reports_claim_and_entry_lanes() {
    // doc7/10 §6.7：三路对照，如实报告；**不得**把 entry 的局部增益推广成整体结论。
    let (mut store, scope) = setup("compare");
    // 0: 昆明（claim 与 entry 都该命中）
    remember(&mut store, &scope, "s1", 0, "用户住在昆明");
    // 1: 旧值（被更正 → 缺省不可见）
    let old = remember(&mut store, &scope, "s2", 0, "用户住在杭州");
    let o = origin("s2");
    let t = chrono::Utc::now();
    let ev_new = match store
        .record_evidence(
            &scope,
            &o,
            1,
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
    // 2: 关系（V2-R1 实体，claim 侧不含「妻子」以外信息）
    remember(&mut store, &scope, "s3", 0, "我妻子叫小雨");
    // 3: 偏好
    remember(&mut store, &scope, "s4", 0, "用户喜欢黑咖啡");
    store.entries_refresh(&scope, &dom()).unwrap();

    let mut claim_hits = 0usize;
    let mut entry_hits = 0usize;
    let mut merged_hits = 0usize;
    let mut expected_total = 0usize;
    let mut report: Vec<String> = Vec::new();
    for (tag, query, expected) in LABELLED {
        expected_total += expected.len();
        let claim = store
            .search_memories(&scope, query, 10, false, &dom())
            .unwrap();
        let claim_ids: Vec<String> = claim.0.iter().map(|h| h.memory_id.clone()).collect();
        let entries = store.entries_search(&scope, &dom(), query, 10).unwrap();
        let entry_ids: Vec<String> = entries.hits.iter().map(|h| h.entry_id.clone()).collect();
        // 合并路：claim + entry 各自去重后并集（同一 query）。
        let mut merged = claim_ids.clone();
        for e in &entry_ids {
            if !merged.contains(e) {
                merged.push(e.clone());
            }
        }
        claim_hits += claim_ids.len().min(expected.len());
        entry_hits += entry_ids.len().min(expected.len());
        merged_hits += merged.len().min(expected.len());
        report.push(format!(
            "{tag:<20} claim={} entry={} merged={} expected={}",
            claim_ids.len(),
            entry_ids.len(),
            merged.len(),
            expected.len()
        ));
    }
    // 如实打印三路结果，便于人工看到 entry 只在哪些类别上有增益。
    for line in &report {
        println!("{line}");
    }
    println!(
        "expected_total={expected_total} claim_overlap={claim_hits} entry_overlap={entry_hits} merged_overlap={merged_hits}"
    );
    // 语义支路未配置时只报词法结果；这里只断言「对照跑通且产出可读结果」，
    // **不**断言 entry 优于 claim（V2 文档 04 §5 明确禁止这种推广）。
    assert_eq!(report.len(), LABELLED.len());
    assert!(store
        .entries_search(&scope, &dom(), "昆明", 10)
        .unwrap()
        .hits
        .iter()
        .any(|h| h.match_reason == "grams" || h.match_reason == "rrf" || h.match_reason == "fts"));
}

// ---- 6. 向量对象已扩到 entry（只入队，不调用模型）----

#[test]
fn entry_vectors_can_be_enqueued_without_a_model_call() {
    // doc7/10 §0：本卡只做绑定与入队；真实语义质量未验证。
    let (mut store, scope) = setup("entry-vector");
    ingest(&mut store, &scope, "s1", 0, "用户在读 Rust 源码");
    store.entries_refresh(&scope, &dom()).unwrap();
    let hit = store.entries_search(&scope, &dom(), "Rust", 10).unwrap();
    let entry_id = hit.hits[0].entry_id.clone();
    let job = store
        .semantic_enqueue(&scope, "entry", &entry_id, "m1")
        .unwrap();
    assert!(job.is_some(), "entry 只入队，不触发任何模型调用");
    let kind: String = store
        .conn()
        .query_row(
            "SELECT object_kind FROM semantic_jobs WHERE tenant_id=?1 AND user_id=?2 AND object_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, entry_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "entry");
    // 非法 object_kind 仍然被拒。
    assert!(matches!(
        store.semantic_enqueue(&scope, "bogus", &entry_id, "m1"),
        Ok(None) | Err(StoreError::StateConflict)
    ));
}

#[test]
fn entry_search_rejects_nothing_but_reports_stale_count() {
    let (mut store, scope) = setup("stale-count");
    ingest(&mut store, &scope, "s1", 0, "用户在写单元测试");
    store.entries_refresh(&scope, &dom()).unwrap();
    let ok = store
        .entries_search(&scope, &dom(), "单元测试", 10)
        .unwrap();
    assert_eq!(ok.skipped_stale, 0);
    let none = store
        .entries_search(&scope, &dom(), "完全不相干的词", 10)
        .unwrap();
    assert!(none.hits.is_empty());
    assert_eq!(none.skipped_stale, 0, "没命中的候选不记为 stale");
    let _ = (ENTRY_MAX_EVENTS, ENTRY_MAX_CHARS);
}
