//! doc7（Riko-Muse）M1/M2/M3 验收测试（全部确定性，无 LLM）：
//! valid_until 到期转换与审计、rupture 扫描幂等与线程归组、synthesis 指标与
//! 再生成策略、purge 闭包对 rupture/线程/synthesis 的清理。真实模型/真实 DSH 不在本卡范围。

use crate::{alignment, Store, StoreError};
use memory_domain::{MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-muse-test-{}-{tag}", std::process::id()));
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

/// 记录一条 user 证据，返回 evidence_id。
fn ingest_user(
    store: &mut Store,
    scope: &ScopeKey,
    origin: &Origin,
    seq: i64,
    content: &str,
) -> String {
    use crate::IngestOutcome;
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
        IngestOutcome::Recorded(id) => id,
        IngestOutcome::AlreadyRecorded(id) => id,
    }
}

/// 直改事件 received_at（仅测试用：构造 7 天归组窗口）。
fn set_received_at(store: &Store, scope: &ScopeKey, evidence_id: &str, rfc3339: &str) {
    store
        .conn()
        .execute(
            "UPDATE evidence_events SET received_at=?1
             WHERE tenant_id=?2 AND user_id=?3 AND id=?4",
            rusqlite::params![rfc3339, scope.tenant_id, scope.user_id, evidence_id],
        )
        .unwrap();
}

fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

// ---- M1 ----

#[test]
fn expire_transitions_due_memory_with_audit_and_idempotent_rerun() {
    let (mut store, scope, origin) = setup("expire");
    let memory_id = {
        use crate::RememberOutcome;
        let ev = ingest_user(&mut store, &scope, &origin, 0, "我下周三要出差");
        match store
            .remember(
                &scope,
                &origin,
                &ev,
                "下周三要出差",
                MemoryKind::Fact,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            RememberOutcome::Created { memory_id, .. } => memory_id,
            RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    };
    // 构造已到期：valid_until = 昨天。
    store
        .conn()
        .execute(
            "UPDATE memories SET valid_until=?1 WHERE tenant_id=?2 AND user_id=?3 AND id=?4",
            rusqlite::params![days_ago(1), scope.tenant_id, scope.user_id, memory_id],
        )
        .unwrap();

    let expired = store.expire_due_memories().unwrap();
    assert_eq!(expired, 1);
    let (status, version): (String, i64) = store
        .conn()
        .query_row(
            "SELECT status, version FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "expired");
    assert_eq!(version, 2);
    // 审计行：system/memoryd/valid_until_expired，previous_status=active。
    let (actor_kind, actor_id, reason, prev, new): (String, String, String, String, String) =
        store
            .conn()
            .query_row(
                "SELECT actor_kind, actor_id, reason_code, previous_status, new_status
                 FROM memory_revisions WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3 AND version=2",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
    assert_eq!(
        (actor_kind.as_str(), actor_id.as_str()),
        ("system", "memoryd")
    );
    assert_eq!(reason, "valid_until_expired");
    assert_eq!((prev.as_str(), new.as_str()), ("active", "expired"));

    // 幂等：第二批无到期目标。
    assert_eq!(store.expire_due_memories().unwrap(), 0);
}

#[test]
fn expire_leaves_not_due_memory_active() {
    let (mut store, scope, origin) = setup("expire-future");
    let memory_id = {
        use crate::RememberOutcome;
        let ev = ingest_user(&mut store, &scope, &origin, 0, "我明年在筹备婚礼");
        match store
            .remember(
                &scope,
                &origin,
                &ev,
                "明年在筹备婚礼",
                MemoryKind::Fact,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            RememberOutcome::Created { memory_id, .. } => memory_id,
            RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    };
    store
        .conn()
        .execute(
            "UPDATE memories SET valid_until=?1 WHERE tenant_id=?2 AND user_id=?3 AND id=?4",
            rusqlite::params![
                (chrono::Utc::now() + chrono::Duration::days(30))
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                scope.tenant_id,
                scope.user_id,
                memory_id
            ],
        )
        .unwrap();
    assert_eq!(store.expire_due_memories().unwrap(), 0);
    let status: String = store
        .conn()
        .query_row(
            "SELECT status FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "active");
}

// ---- M2 ----

#[test]
fn rupture_scan_detects_groups_and_is_idempotent() {
    let (mut store, scope, origin) = setup("rupture-scan");
    let e1 = ingest_user(&mut store, &scope, &origin, 0, "不对，我说的是十点不是九点");
    let _e2 = ingest_user(&mut store, &scope, &origin, 1, "好的，我明白了");
    let _e3 = ingest_user(&mut store, &scope, &origin, 2, "你又这样，别这样了。");
    // 同一事件多信号：e1 命中 correction（"不对"，同信号取最早）；
    // e3 命中 recurrence + boundary 两个信号。

    let out = store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(out.scanned_events, 3);
    assert_eq!(out.inserted_ruptures, 3, "e1×1 + e3×2");
    assert_eq!(out.opened_threads, 1, "同一窗口内全部归入一个线程");

    // 幂等：重扫不新增（游标已推进 + 唯一键兜底）。
    let again = store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(again.inserted_ruptures, 0);
    assert_eq!(again.opened_threads, 0);

    let ruptures = store
        .ruptures_list(&scope, 50, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(ruptures.len(), 3);
    let thread_ids: std::collections::HashSet<_> = ruptures
        .iter()
        .map(|r| r.thread_id.clone().unwrap())
        .collect();
    assert_eq!(thread_ids.len(), 1);
    let threads = store
        .repair_threads_list(
            &scope,
            Some("open"),
            50,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].rupture_count, 3);
    assert_eq!(threads[0].status, "open");
    let _ = e1;
}

#[test]
fn rupture_regroup_window_opens_new_thread_after_seven_days() {
    let (mut store, scope, origin) = setup("rupture-regroup");
    // 30 天前的一次纠正。
    let old = ingest_user(&mut store, &scope, &origin, 0, "不对，这不是我要的");
    set_received_at(&store, &scope, &old, &days_ago(30));
    store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    // 现在的一次纠正：超出 7 天归组窗口 → 新线程。
    let _recent = ingest_user(&mut store, &scope, &origin, 1, "你又理解错了");
    store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();

    let threads = store
        .repair_threads_list(
            &scope,
            Some("open"),
            50,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(threads.len(), 2, "窗口外 rupture 开新线程");
}

#[test]
fn repair_thread_close_is_explicit_and_idempotent() {
    let (mut store, scope, origin) = setup("thread-close");
    let _ = ingest_user(&mut store, &scope, &origin, 0, "不对，别这样回复");
    store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    let threads = store
        .repair_threads_list(
            &scope,
            Some("open"),
            50,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(threads.len(), 1);
    let thread_id = threads[0].id.clone();

    assert!(store
        .repair_thread_close(
            &scope,
            &thread_id,
            "已当面说清",
            "api",
            &memory_domain::DomainScope::user_main()
        )
        .unwrap());
    // closed 幂等返回 false。
    assert!(!store
        .repair_thread_close(
            &scope,
            &thread_id,
            "已当面说清",
            "api",
            &memory_domain::DomainScope::user_main()
        )
        .unwrap());
    let closed = store
        .repair_threads_list(
            &scope,
            Some("closed"),
            50,
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].close_reason.as_deref(), Some("已当面说清"));
    // 不存在的线程。
    let err = store
        .repair_thread_close(
            &scope,
            "no-such-thread",
            "x",
            "api",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::ThreadNotFound));
}

// ---- M3 ----

#[test]
fn synthesis_metrics_and_regenerate_policy() {
    let (mut store, scope, origin) = setup("synthesis");
    let _ = ingest_user(&mut store, &scope, &origin, 0, "不对，顺序反了");
    for seq in 1..4 {
        let _ = ingest_user(&mut store, &scope, &origin, seq, "这条先记着");
    }
    // 先扫描落 rupture，再派生 synthesis。
    store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    // 首版：rupture 1 / user 4 → rate 0.75；1 个 open 线程。
    let v1 = store
        .alignment_synthesis_refresh(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.rupture_turns, 1);
    assert_eq!(v1.user_turns, 4);
    assert!((v1.correction_free_rate - 0.75).abs() < 1e-9);
    assert_eq!(v1.open_repair_threads, 1);
    // body 含线程 ID（来源可溯）。
    let threads = store
        .repair_threads_list(&scope, None, 50, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert!(v1.body.contains(&threads[0].id));
    // source_refs_json 记录了 rupture 与线程 ID。
    let refs: serde_json::Value = serde_json::from_str(&v1.source_refs_json).unwrap();
    assert_eq!(refs["rupture_event_ids"].as_array().unwrap().len(), 1);
    assert_eq!(refs["thread_ids"].as_array().unwrap().len(), 1);

    // 幂等：无新 rupture、线程数未变、窗口未超 24h → 不生成新版本。
    let same = store
        .alignment_synthesis_refresh(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(same.version, 1);

    // 关线 → open 线程数变化 → 新版本。
    store
        .repair_thread_close(
            &scope,
            &threads[0].id,
            "已解释",
            "api",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    let v2 = store
        .alignment_synthesis_refresh(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.open_repair_threads, 0);
    // 新版本不计旧窗口 rupture（窗口推进）。
    assert_eq!(v2.rupture_turns, 0);
    assert_eq!(v2.user_turns, 0);
    assert!((v2.correction_free_rate - 1.0).abs() < 1e-9);

    let latest = store
        .alignment_synthesis_latest(&scope, &memory_domain::DomainScope::user_main())
        .unwrap()
        .unwrap();
    assert_eq!(latest.version, 2);
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM alignment_synthesis",),
        2
    );
}

#[test]
fn synthesis_empty_scope_returns_empty_window() {
    let (mut store, scope, _origin) = setup("synthesis-empty");
    let v1 = store
        .alignment_synthesis_refresh(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(v1.rupture_turns, 0);
    assert_eq!(v1.user_turns, 0);
    assert!((v1.correction_free_rate - 1.0).abs() < 1e-9);
    assert!(v1.body.contains("无待修复线程"));
}

// ---- purge 闭包扩展 ----

#[test]
fn purge_closure_removes_ruptures_empty_threads_and_referencing_synthesis() {
    let (mut store, scope, origin) = setup("purge-closure");
    // 记忆 + 其 user 证据（含纠正 cue）。
    let ev = ingest_user(&mut store, &scope, &origin, 0, "不对，我不想聊这个话题了");
    let memory_id = {
        use crate::RememberOutcome;
        match store
            .remember(
                &scope,
                &origin,
                &ev,
                "我不想聊这个话题了",
                MemoryKind::Fact,
                &memory_domain::DomainScope::user_main(),
            )
            .unwrap()
        {
            RememberOutcome::Created { memory_id, .. } => memory_id,
            RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    };
    store
        .rupture_scan(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    let v1 = store
        .alignment_synthesis_refresh(&scope, &memory_domain::DomainScope::user_main())
        .unwrap();
    assert_eq!(v1.rupture_turns, 1);

    // purge 该记忆（preview → confirm 两阶段）。
    let (token, _preview) = store
        .purge_preview(
            &scope,
            &memory_id,
            "purge-key-1",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();
    let _result = store
        .purge_confirm(
            &scope,
            &token,
            "purge-key-1",
            &memory_domain::DomainScope::user_main(),
        )
        .unwrap();

    // rupture / 线程 / synthesis 引用版本随 L0 闭包清理。
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM rupture_events"),
        0,
        "rupture 随 evidence 删除"
    );
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM repair_threads"),
        0,
        "失去全部 rupture 的线程删除"
    );
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM alignment_synthesis"),
        0,
        "引用被删 rupture/thread 的 synthesis 版本删除"
    );
    assert!(store
        .alignment_synthesis_latest(&scope, &memory_domain::DomainScope::user_main())
        .unwrap()
        .is_none());
}

#[test]
fn all_scopes_lists_principals() {
    let (store, _scope, _origin) = setup("all-scopes");
    let scopes = store.all_scopes().unwrap();
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].tenant_id, "t");
    assert_eq!(scopes[0].user_id, "u");
}

// 常量护栏：doc7/01 冻结值不被静默改动。
#[test]
fn frozen_constants_match_doc7() {
    assert_eq!(alignment::REPAIR_THREAD_REGROUP_DAYS, 7);
    assert_eq!(alignment::EXPIRE_BATCH_LIMIT, 500);
    assert_eq!(alignment::RUPTURE_SCAN_BATCH, 2000);
    assert_eq!(alignment::SYNTHESIS_REGEN_MIN_SECS, 24 * 60 * 60);
    assert_eq!(alignment::THREAD_TITLE_MAX_CHARS, 80);
}
