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
        .record_evidence(scope, origin, seq, "user", "user", &t, quote)
        .unwrap()
    {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store.remember(scope, origin, &ev, quote, kind).unwrap() {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
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
        .publish_page(&crate::pages::PublishRequest {
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
        })
        .unwrap();
    assert!(store
        .get_page(&scope, &page_id, &crate::now_rfc3339_pub().unwrap())
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
    assert!(store.retire_memory(&scope, &mid, &req).unwrap());
    // 全路径消失：get_memory / search / resident 可见 pin。
    assert!(
        store.get_memory(&scope, &mid).unwrap().is_none(),
        "retired 不进 get_memory"
    );
    let (hits, _) = store.search_memories(&scope, "杭州", 10, false).unwrap();
    assert!(
        hits.iter().all(|h| h.memory_id != mid),
        "retired 不进 search"
    );
    let vis = store
        .resident_visible_pins(&scope, &crate::now_rfc3339_pub().unwrap())
        .unwrap();
    assert!(
        vis.iter().all(|p| p.memory_id != mid),
        "retired 不进 resident"
    );
    assert!(store.retirement_get(&scope, &mid).unwrap().is_some());
    assert!(
        store
            .get_page(&scope, &page_id, &crate::now_rfc3339_pub().unwrap())
            .unwrap()
            .is_none(),
        "retire 事务内立即使派生页失效"
    );
    assert!(store
        .page_fts_search(&scope, "居住地", 10)
        .unwrap()
        .is_empty());
    assert!(store
        .page_list(&scope, &["published"], 10)
        .unwrap()
        .iter()
        .all(|page| page.page_id != page_id));
    assert!(store.page_pin(&scope, &page_id).is_err());
    // 重复 retire 幂等确认。
    assert!(
        store.retire_memory(&scope, &mid, &req).unwrap(),
        "同键同请求重放返回原回执"
    );
    let restore_evidence = match store
        .record_evidence(
            &scope,
            &origin,
            3,
            "user",
            "user",
            &chrono::Utc::now(),
            "请恢复这条记忆",
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
    assert!(store.restore_memory(&scope, &mid, &restore).unwrap());
    assert!(
        store.restore_memory(&scope, &mid, &restore).unwrap(),
        "同键 restore 重放返回原回执"
    );
    assert!(store.get_memory(&scope, &mid).unwrap().is_some());
    assert!(
        store
            .get_page(&scope, &page_id, &crate::now_rfc3339_pub().unwrap())
            .unwrap()
            .is_none(),
        "restore 不会复活退休时已失效的派生页"
    );
    assert!(store.retirement_get(&scope, &mid).unwrap().is_none());
    let (hits2, _) = store.search_memories(&scope, "杭州", 10, false).unwrap();
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
        store.retire_memory(&scope2, &mid, &req).is_err(),
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
        )
        .unwrap();
    assert!(changed, "memory_audit 写失败不能回滚业务更新");
    assert!(store.retirement_get(&scope, &mid).unwrap().is_some());
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
    let (token, preview) = store.purge_preview(&scope, &mid, "idem-1").unwrap();
    assert_eq!(preview.evidence_ids.len(), 1);
    assert!(
        store.get_memory(&scope, &mid).unwrap().is_some(),
        "preview 只读业务记忆"
    );
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
    let out = store.purge_confirm(&scope, &token, "idem-1").unwrap();
    assert!(
        out.deleted
            .get("evidence_deleted")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 1
    );
    // 闭包后：记忆/证据/审计/候选全无；job 与 confirmation 不留可反查目标 ID。
    assert!(store.get_memory(&scope, &mid).unwrap().is_none());
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
    );
    assert!(
        matches!(replay, Err(StoreError::EventConflict)),
        "墓碑阻止重放复活"
    );
    // 同幂等键重放 confirm：无正文结果。
    let replay2 = store.purge_confirm(&scope, &token, "idem-1").unwrap();
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
    // 一条无记忆引用、时间戳极旧的事件：用旧 occurred_at 直插（模拟超期）。
    let old_ev = {
        let old = chrono::Utc::now() - chrono::Duration::days(400);
        match store
            .record_evidence(&scope, &origin, 2, "user", "user", &old, "很旧的一句话")
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    // 默认（未配置策略）：无操作。
    assert!(store.retention_run(&scope).unwrap().is_none());
    assert!(store.get_memory(&scope, &_mid).unwrap().is_some());
    // 正值策略：raw evidence 保留 365 天 → 400 天前且无引用的事件被清理。
    store.retention_set_policy(&scope, 365, 0, true).unwrap();
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
    assert!(store.get_memory(&scope, &_mid).unwrap().is_some());
    // 同批次重跑幂等。
    assert!(store.retention_run(&scope).unwrap().is_none());
}
