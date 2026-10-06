//! V2-B1/A1 验收测试（doc7/08 §6，全部确定性、**不调用任何模型**）。

use crate::background::ProposeAction;
use crate::{Store, StoreError};
use memory_domain::schedule::{
    nightly_due, quiet_due, relationships_due, upkeep_due, ScheduleConfig,
};
use memory_domain::{DomainScope, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-v2bg-test-{}-{tag}", std::process::id()));
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

fn evidence(store: &mut Store, scope: &ScopeKey, session: &str, seq: i64, text: &str) -> String {
    let o = Origin {
        host_id: "dsh".into(),
        agent_id: "a".into(),
        session_id: session.into(),
    };
    let t = chrono::Utc::now();
    match store
        .record_evidence(scope, &o, seq, "user", "user", &t, text, &dom())
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    }
}

// ---- 1—4：调度判定（纯函数；开关互不影响）----

#[test]
fn schedule_never_runs_without_new_signal() {
    let cfg = ScheduleConfig::default();
    assert!(!upkeep_due(&cfg, 100_000, None, 0));
    assert!(!relationships_due(&cfg, 100_000, None, 0));
    assert!(!quiet_due(&cfg, 100_000, 0, 0, 0));
    assert!(upkeep_due(&cfg, 100_000, None, 1));
}

#[test]
fn schedule_switches_do_not_leak_across_tasks() {
    let cfg = ScheduleConfig {
        quiet_enabled: false,
        ..ScheduleConfig::default()
    };
    // 关掉 quiet 不影响 upkeep / relationships / nightly。
    assert!(upkeep_due(&cfg, 100_000, Some(0), 1));
    assert!(relationships_due(&cfg, 100_000, Some(0), 1));
    assert!(nightly_due(&cfg, "2026-10-07", Some("2026-10-06")));
    assert!(!quiet_due(&cfg, 100_000, 0, 1, 0));
}

// ---- 5：处理账本幂等 ----

#[test]
fn ledger_dedupes_the_same_signal_per_task() {
    // doc7/08 §6.5：同一信号只消化一次；不同任务/不同信号各留一行。
    let (store, scope) = setup("ledger");
    assert!(store
        .ledger_record(&scope, &dom(), "upkeep", "ev-1", 1)
        .unwrap());
    assert!(
        !store
            .ledger_record(&scope, &dom(), "upkeep", "ev-1", 1)
            .unwrap(),
        "同 (域, 任务, 信号) 第二次记账必须返回 false"
    );
    assert!(store
        .ledger_record(&scope, &dom(), "nightly", "ev-1", 1)
        .unwrap());
    assert!(store
        .ledger_record(&scope, &dom(), "upkeep", "ev-2", 1)
        .unwrap());
    assert_eq!(store.ledger_count(&scope, &dom(), "upkeep").unwrap(), 2);
    assert_eq!(store.ledger_count(&scope, &dom(), "nightly").unwrap(), 1);
    assert!(store.ledger_has(&scope, &dom(), "upkeep", "ev-1").unwrap());
    assert!(!store.ledger_has(&scope, &dom(), "upkeep", "ev-9").unwrap());
    // 非法 task_kind 直接拒绝，不静默吞。
    assert!(matches!(
        store.ledger_record(&scope, &dom(), "bogus", "ev-1", 1),
        Err(StoreError::StateConflict)
    ));
}

#[test]
fn task_runs_are_recorded_and_countable() {
    let (mut store, scope) = setup("runs");
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(
            store
                .task_run_record(&scope, &dom(), "quiet", "ok", 1, "")
                .unwrap(),
        );
    }
    store
        .task_run_record(&scope, &dom(), "quiet", "error", 0, "SAFE_ERR")
        .unwrap();
    let runs = store
        .task_runs_since(&scope, &dom(), "quiet", "1970-01-01T00:00:00Z")
        .unwrap();
    assert_eq!(runs.len(), 4, "失败也要留痕，不吞失败当成功");
    assert_eq!(
        runs.iter().filter(|r| r.outcome == "ok").count(),
        3,
        "当日计数据此判定 quiet 上限"
    );
    assert!(ids.iter().all(|i| !i.is_empty()));
    // 非法 outcome 拒绝。
    assert!(matches!(
        store.task_run_record(&scope, &dom(), "quiet", "fine", 1, ""),
        Err(StoreError::StateConflict)
    ));
}

// ---- 6：rupture_v2 分类端到端 ----

#[test]
fn rupture_scan_classifies_and_only_agent_corrections_open_threads() {
    // doc7/08 §6.6：自我纠正 FP 不开线；真实抱怨 FN 开线；引语与中性只留痕。
    let (mut store, scope) = setup("rupture-v2");
    evidence(
        &mut store,
        &scope,
        "s-self",
        1,
        "不对，是我记错了，应该是十点。",
    );
    evidence(
        &mut store,
        &scope,
        "s-neutral",
        1,
        "这个方案不对，我们再想想。",
    );
    evidence(&mut store, &scope, "s-quote", 1, "他说你记错了，其实没错。");
    evidence(&mut store, &scope, "s-agent", 1, "上次你说错导致损失");

    let outcome = store.rupture_scan(&scope, &dom()).unwrap();
    assert_eq!(outcome.opened_threads, 1, "只有对 Agent 的纠正才开线");

    let rows: Vec<(String, String, bool)> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT target, detector_version, thread_id IS NULL FROM rupture_events
                 WHERE tenant_id=?1 AND user_id=?2 ORDER BY detected_at, target",
            )
            .unwrap();
        let mapped = stmt
            .query_map(rusqlite::params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        mapped.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(rows.len(), 4, "四种语境都要落库留痕：{rows:?}");
    assert!(rows.iter().all(|(_, v, _)| v == "rupture_v2"));
    let by_target: std::collections::BTreeMap<&str, bool> = rows
        .iter()
        .map(|(t, _, no_thread)| (t.as_str(), *no_thread))
        .collect();
    assert_eq!(
        by_target.get("agent_correction"),
        Some(&false),
        "对 Agent 的纠正挂线程"
    );
    assert_eq!(
        by_target.get("self_correction"),
        Some(&true),
        "自我纠正不挂线程"
    );
    assert_eq!(by_target.get("third_party"), Some(&true), "引语不挂线程");
    assert_eq!(by_target.get("neutral"), Some(&true), "中性讨论不挂线程");

    // 重扫幂等：不重复插入。
    let again = store.rupture_scan(&scope, &dom()).unwrap();
    assert_eq!(again.inserted_ruptures, 0);
}

// ---- 7：行动授权 ----

#[test]
fn repair_actions_require_authorization_to_activate_or_close() {
    // doc7/08 §6.7。
    let (mut store, scope) = setup("actions");
    evidence(&mut store, &scope, "s1", 1, "你记错了，我说的是十点。");
    store.rupture_scan(&scope, &dom()).unwrap();
    let thread_id = store
        .repair_threads_list(&scope, Some("open"), 10, &dom())
        .unwrap()
        .first()
        .map(|t| t.id.clone())
        .expect("应已开线");

    // 模型只能提议。
    let action_id = store
        .repair_action_propose(
            &scope,
            &ProposeAction {
                thread_id: &thread_id,
                action: "在确认时间前先复述用户给的时间",
                expected_behavior: "回复前先引用用户原话里的时间点",
                conditions: "涉及时间承诺时",
                counterexamples: "用户只是随口提到时间",
                proposed_by: "model",
                source_memory_id: None,
                generator_version: Some("action_v1"),
            },
        )
        .unwrap();
    let listed = store.repair_action_list(&scope, Some(&thread_id)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, "proposed");
    assert!(listed[0].authorized_by.is_none(), "模型提议不得自带授权");
    // 空话不算行动。
    assert!(matches!(
        store.repair_action_propose(
            &scope,
            &ProposeAction {
                thread_id: &thread_id,
                action: "注意",
                expected_behavior: "小心",
                conditions: "",
                counterexamples: "",
                proposed_by: "model",
                source_memory_id: None,
                generator_version: None,
            },
        ),
        Err(StoreError::InvalidRepairAction)
    ));
    // 未授权不能激活。
    assert!(matches!(
        store.repair_action_activate(&scope, &action_id, "  "),
        Err(StoreError::StateConflict)
    ));
    assert!(store
        .repair_action_activate(&scope, &action_id, "user")
        .unwrap());
    assert!(
        !store
            .repair_action_activate(&scope, &action_id, "user")
            .unwrap(),
        "已激活的不能重复激活"
    );
    // 关闭必须有显式理由。
    assert!(matches!(
        store.repair_action_close(&scope, &action_id, "", "user"),
        Err(StoreError::StateConflict)
    ));
    assert!(store
        .repair_action_close(&scope, &action_id, "用户确认这条已经处理", "user")
        .unwrap());
    let after = store.repair_action_list(&scope, Some(&thread_id)).unwrap();
    assert_eq!(after[0].status, "done");
    assert_eq!(
        after[0].close_reason.as_deref(),
        Some("用户确认这条已经处理")
    );
}

// ---- 8：复发计数幂等 ----

#[test]
fn recurrence_counting_is_idempotent_per_rupture() {
    // doc7/08 §6.8。
    let (mut store, scope) = setup("recurrence");
    evidence(&mut store, &scope, "s1", 1, "你记错了，我说的是十点。");
    store.rupture_scan(&scope, &dom()).unwrap();
    let thread_id = store
        .repair_threads_list(&scope, Some("open"), 10, &dom())
        .unwrap()
        .first()
        .unwrap()
        .id
        .clone();
    let action_id = store
        .repair_action_propose(
            &scope,
            &ProposeAction {
                thread_id: &thread_id,
                action: "先复述用户给的时间再回复",
                expected_behavior: "回复里引用用户原话的时间点",
                conditions: "",
                counterexamples: "",
                proposed_by: "cli",
                source_memory_id: None,
                generator_version: None,
            },
        )
        .unwrap();
    store
        .repair_action_activate(&scope, &action_id, "cli")
        .unwrap();

    let now = crate::now_rfc3339_pub().unwrap();
    assert_eq!(
        store
            .repair_action_record_recurrence(&scope, &thread_id, "rup-1", &now)
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .repair_action_record_recurrence(&scope, &thread_id, "rup-1", &now)
            .unwrap(),
        0,
        "同一 rupture 重复记录必须幂等"
    );
    assert_eq!(
        store
            .repair_action_record_recurrence(&scope, &thread_id, "rup-2", &now)
            .unwrap(),
        1
    );
    let actions = store.repair_action_list(&scope, Some(&thread_id)).unwrap();
    assert_eq!(
        actions[0].recurrence_count, 2,
        "两条不同 rupture 算两次复发"
    );
}

#[test]
fn schedule_state_is_read_only_and_reports_ledger_counts() {
    let (mut store, scope) = setup("state");
    store
        .ledger_record(&scope, &dom(), "upkeep", "ev-1", 1)
        .unwrap();
    store
        .task_run_record(&scope, &dom(), "upkeep", "ok", 1, "")
        .unwrap();
    let state = store.schedule_state(&scope, &dom()).unwrap();
    assert_eq!(state.domain_id, "user_main");
    assert_eq!(state.ledger_counts.get("upkeep"), Some(&1));
    assert!(state.last_run_at.is_some());
}
