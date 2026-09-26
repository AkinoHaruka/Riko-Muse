//! D6-8 固定响应验收测试（doc6/08 卡内要求）：向量生命周期与扫描排名、
//! 裁决 pairwise 固定集（复述/更新/冲突/无匹配）、引用越权拒绝、输入漂移
//! stale、解析严格性。真实 embedding/模型端点不在本卡（D6-10 验收）。

use crate::adjudication::{
    parse_adjudicate_v1, AdjudicationCandidate, AdjudicationProposal, AdjudicationRecall,
};
use crate::{Store, StoreError};
use memory_domain::{MemoryKind, Origin, ScopeKey};

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-d68-test-{}-{tag}", std::process::id()));
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
    quote: &str,
    kind: MemoryKind,
) -> String {
    let t = chrono::Utc::now();
    let ev = match store.record_evidence(scope, origin, seq, "user", "user", &t, quote).unwrap() {
        crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
    };
    match store.remember(scope, origin, &ev, quote, kind).unwrap() {
        crate::RememberOutcome::Created { memory_id, .. }
        | crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
    }
}

#[test]
fn semantic_vector_lifecycle_scan_order_and_stale() {
    // doc6/02 §4 / doc6/04 §2：版本化缓存、model_id+dimensions 隔离、对象失效
    // 同事务 stale、扫描按余弦降序（按 memory ID 断言排名）。
    let (mut store, scope, origin) = setup("vec");
    let m_near = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
    let m_far = remember_one(&mut store, &scope, &origin, 2, "用户对芒果过敏", MemoryKind::Fact);
    // 入队：同对象重复入队幂等（返回同一作业）。
    let j1 = store.semantic_enqueue(&scope, "memory", &m_near, "m1").unwrap().unwrap();
    let j1b = store.semantic_enqueue(&scope, "memory", &m_near, "m1").unwrap().unwrap();
    assert_eq!(j1.id, j1b.id, "同对象同模型待处理作业幂等");
    store.semantic_enqueue(&scope, "memory", &m_far, "m1").unwrap().unwrap();
    // claim 两个作业并写向量：m_near=[1,0]、m_far=[0,1]。
    for mid in [&m_near, &m_far] {
        let now_c = crate::now_rfc3339_pub().unwrap();
        let (scope_j, job) = store.semantic_job_claim(&now_c, 90).unwrap().unwrap();
        assert_eq!(job.model_id, "m1");
        let v: Vec<f32> = if job.object_id == *mid { [1.0f32, 0.0].into() } else { [0.0f32, 1.0].into() };
        store.semantic_vector_save(&scope_j, "memory", &job.object_id, "m1", job.source_version, &job.content_sha256, &v).unwrap();
        store.semantic_job_finish(&scope_j, &job.id, job.claim_generation, "succeeded", None, None).unwrap();
    }
    // 扫描：query=[0.95,0.15] → m_near 余弦更高（按 memory ID 断言排名）。
    let (hits, count) = store.semantic_scan(&scope, "memory", "m1", &[0.95, 0.15], 10).unwrap();
    assert_eq!((hits.len(), count), (2, 2));
    assert_eq!(hits[0].0, m_near, "余弦降序：近邻在前");
    assert_eq!(hits[1].0, m_far);
    // model_id 隔离：不同模型不混算。
    assert_eq!(store.semantic_scan(&scope, "memory", "m2", &[1.0, 0.0], 10).unwrap().0.len(), 0);
    // dimensions 隔离：m3 下存 3 维向量（sha 用真实值），2 维 query 扫不到。
    let (_v, real_sha): (i64, String) = store
        .conn()
        .query_row(
            "SELECT version, claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, m_near],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    store.semantic_vector_save(&scope, "memory", &m_near, "m3", _v, &real_sha, &[1.0, 0.0, 0.0]).unwrap();
    assert_eq!(store.semantic_scan(&scope, "memory", "m3", &[1.0, 0.0], 10).unwrap().0.len(), 0);
    // 版本漂移：save 用旧版本 → StaleInput，不覆盖。
    let drift = store.semantic_vector_save(&scope, "memory", &m_near, "m1", 99, "old", &[0.5, 0.5]);
    assert!(matches!(drift, Err(StoreError::StaleInput)));
    // 对象失效同事务 stale：correct 后旧记忆向量不可见于扫描。
    // correct 要求最新用户事件同时含 old/replacement quote。
    let ev = {
        let t = chrono::Utc::now();
        match store
            .record_evidence(&scope, &origin, 3, "user", "user", &t, "用户住在杭州？不，我搬到上海了")
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    let req = crate::CorrectRequest {
        expected_version: 1,
        origin,
        user_evidence_id: ev,
        old_quote: "用户住在杭州".into(),
        replacement_quote: "我搬到上海了".into(),
    };
    store.correct_memory(&scope, &m_near, &req).unwrap();
    let (hits2, _) = store.semantic_scan(&scope, "memory", "m1", &[1.0, 0.0], 10).unwrap();
    assert!(hits2.iter().all(|(id, _)| *id != m_near), "correct 后旧向量立即 stale");
    // 跨 scope 隔离：另一用户扫描看不到。
    let dir2 = std::env::temp_dir().join(format!("am-d68-test-{}-vec2", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir2);
    std::fs::create_dir_all(&dir2).unwrap();
    store.principal_add("t", "u2", &dir2.join("t.token")).unwrap();
    let tok2 = std::fs::read_to_string(dir2.join("t.token")).unwrap();
    let scope2 = store.verify_token(tok2.trim()).unwrap().unwrap();
    assert_eq!(store.semantic_scan(&scope2, "memory", "m1", &[1.0, 0.0], 10).unwrap().0.len(), 0);
}

#[test]
fn adjudication_apply_pairwise_fixed_set() {
    // doc6/09 §5/§9：create/attach_evidence/update/keep_separate/conflict/defer/
    // not_memory 全动作 + 引用越权拒绝；false merge（keep_separate 两条都在）。
    let (mut store, scope, origin) = setup("adj");
    // 目标记忆（供 attach/update/conflict）。
    let target = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
    let (tver, thash): (i64, String) = store
        .conn()
        .query_row(
            "SELECT version, claim_sha256 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // Dream job + 候选（走真实 submit 校验）。
    let content = "用户住在杭州，用户喜欢简短回答，用户下周去北京，用户养了一只猫，用户会拉小提琴，用户昨天打网球扭伤了脚，用户计划学日语，用户对花粉过敏。";
    let ev = {
        let t = chrono::Utc::now();
        match store.record_evidence(&scope, &origin, 2, "user", "user", &t, content).unwrap() {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    let _job = store.dream_trigger(&scope, "manual", "k1", None, None, None).unwrap().unwrap();
    // claim 时钟须 >= run_after（trigger 内部落库时刻），故在 trigger 后取。
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 900).unwrap().unwrap();
    let gen = claimed.claim_generation;
    let span_of = |q: &str| {
        let sb = content.find(q).unwrap() as i64;
        (sb, sb + q.len() as i64)
    };
    let mk = |kind: &str, claim: &str, quote: &str| {
        let (sb, eb) = span_of(quote);
        crate::dream_jobs::DreamProposal {
            kind: kind.into(),
            claim: claim.into(),
            quote: quote.into(),
            evidence_id: ev.clone(),
            start_byte: sb,
            end_byte: eb,
            status: "candidate".into(),
            reason_code: None,
            occurred_at: None,
        }
    };
    // 8 个候选覆盖 pairwise 集：
    let (a_create, b_attach, c_update, d_keep, e_conflict, f_defer, g_notmem, h_foreign) = (
        mk("fact", "用户会拉小提琴", "用户会拉小提琴"),
        mk("fact", "用户住在杭州", "用户住在杭州"),           // 与 target 精确复述
        mk("fact", "用户搬到上海了", "用户下周去北京"),         // 同方面状态变化（借下一条 quote）
        mk("fact", "用户养了一只猫", "用户养了一只猫"),         // 近义不同方面（keep）
        mk("fact", "用户对花粉过敏", "用户对花粉过敏"),         // 与芒果过敏冲突候选
        mk("fact", "用户计划学日语", "用户计划学日语"),         // 语境不足
        mk("episode", "用户昨天打网球", "用户昨天打网球"),       // 非长期记忆
        mk("fact", "用户喜欢简短回答", "用户喜欢简短回答"),       // 动作有效但目标越权
    );
    let props = vec![a_create, b_attach, c_update, d_keep, e_conflict, f_defer, g_notmem, h_foreign];
    let (accepted, rejected) = store
        .dream_submit_candidates(&scope, &claimed.id, gen, crate::dream_jobs::DREAM_POLICY_V1, &props)
        .unwrap();
    assert_eq!((accepted, rejected), (8, 0));
    // 读取候选 ID（按 claim 对应）。
            let cand_id = |claim: &str| -> String {
        store
            .conn()
            .query_row(
                "SELECT id FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND claim=?4",
                rusqlite::params![scope.tenant_id, scope.user_id, claimed.id, claim],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    let ids: Vec<String> = props.iter().map(|p| cand_id(&p.claim)).collect();
    // 冻结：候选输入 + 召回（exact 复述召回 target；其余无召回）。
    let inputs: Vec<AdjudicationCandidate> = {
        let mut out = Vec::new();
        for (p, cid) in props.iter().zip(&ids) {
            let (sb, eb) = span_of(&p.quote);
            out.push(AdjudicationCandidate {
                candidate_id: cid.clone(),
                kind: p.kind.clone(),
                claim: p.claim.clone(),
                quote: p.quote.clone(),
                status: "candidate".into(),
                evidence_id: ev.clone(),
                start_byte: sb,
                end_byte: eb,
            });
        }
        out
    };
    let recalls = vec![
        AdjudicationRecall {
            candidate_id: ids[1].clone(), // b 复述召回 target（exact）
            target_memory_id: target.clone(),
            target_version: tver,
            channel: "exact".into(),
        },
        AdjudicationRecall {
            candidate_id: ids[2].clone(), // c update 的 target 同样须在冻结召回内
            target_memory_id: target.clone(),
            target_version: tver,
            channel: "exact".into(),
        },
    ];
    let adj = store
        .adjudication_create(
            &scope, &claimed.id,
            memory_contract::ADMISSION_VERSION_V3,
            crate::adjudication::ADJUDICATE_V1,
            Some("m1"), &inputs, &recalls,
        )
        .unwrap()
        .unwrap();
    // 同指纹幂等。
    let adj2 = store
        .adjudication_create(&scope, &claimed.id, memory_contract::ADMISSION_VERSION_V3,
                             crate::adjudication::ADJUDICATE_V1, Some("m1"), &inputs, &recalls)
        .unwrap()
        .unwrap();
    assert_eq!(adj.id, adj2.id);
    // 领取 + 应用固定裁决集（claim 时钟须 >= 冻结时落库的 run_after）。
    let now2 = crate::now_rfc3339_pub().unwrap();
    let (_s, ajob) = store.adjudication_claim(&now2, 900).unwrap().unwrap();
    assert_eq!(ajob.id, adj.id);
    let agen = ajob.claim_generation;
    let p = |cid: &str, durability: &str, action: &str, tgt: Option<&str>, ver: Option<i64>| AdjudicationProposal {
        candidate_id: cid.into(),
        durability: durability.into(),
        action: action.into(),
        reason_code: Some("test".into()),
        target_memory_id: tgt.map(|s| s.into()),
        expected_target_version: ver,
        model_confidence: Some(0.9),
        valid_until: None,
    };
    let outcome = store
        .adjudication_apply(&scope, &adj.id, agen, &[
            p(&ids[0], "durable", "create", None, None),                       // a 新建
            p(&ids[1], "durable", "attach_evidence", Some(&target), Some(tver)), // b 复述→补证据
            p(&ids[2], "durable", "update", Some(&target), Some(tver)),        // c 状态更新
            p(&ids[3], "durable", "keep_separate", None, None),                // d 近义不同方面
            p(&ids[4], "uncertain", "conflict", None, None),                   // e 冲突 → held
            p(&ids[5], "uncertain", "defer", None, None),                      // f 语境不足 → held
            p(&ids[6], "not_memory", "not_memory", None, None),                // g 非记忆
            p(&ids[7], "durable", "attach_evidence", Some(&target), Some(tver)), // h 目标不在其召回集
        ])
        .unwrap();
    // 应用桶= create/attach/update/keep/not_memory 5 条 + rejected(h) 1 条 + held(e,f) 2 条。
    assert_eq!((outcome.applied, outcome.rejected, outcome.held), (5, 1, 2));
    // 逐条断言 application_status（按 memory ID/状态）。
    let status_of = |cid: &str| -> String {
        store
            .conn()
            .query_row(
                "SELECT application_status FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND candidate_id=?4",
                rusqlite::params![scope.tenant_id, scope.user_id, adj.id, cid],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(status_of(&ids[0]), "applied");
    assert_eq!(status_of(&ids[1]), "applied");
    assert_eq!(status_of(&ids[2]), "applied");
    assert_eq!(status_of(&ids[3]), "applied");
    assert_eq!(status_of(&ids[4]), "applied");
    assert_eq!(status_of(&ids[5]), "applied");
    assert_eq!(status_of(&ids[6]), "applied");
    assert_eq!(status_of(&ids[7]), "rejected", "引用越权（目标不在其冻结召回集）拒绝");
    // b 复述：target 证据追加，未产生重复 Active（false merge/duplicate 检查）。
    let n_ev: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, target],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert!(n_ev >= 2, "attach_evidence 追加了新证据");
    // c update：旧版 superseded + 新版 active；exact hash 未变旧行不复用。
    let new_status: String = store
        .conn()
        .query_row(
            "SELECT status FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id,
                store.conn().query_row(
                    "SELECT applied_result_memory_id FROM adjudication_results
                     WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND candidate_id=?4",
                    rusqlite::params![scope.tenant_id, scope.user_id, adj.id, ids[2]],
                    |r| r.get::<_, Option<String>>(0),
                ).unwrap().unwrap()
            ],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(new_status, "active");
    // d keep_separate：独立 L1 与 create 的 L1 并存（false merge 防护）。
    let (d_mem, a_mem) = (
        store.conn().query_row(
            "SELECT applied_result_memory_id FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND candidate_id=?4",
            rusqlite::params![scope.tenant_id, scope.user_id, adj.id, ids[3]],
            |r| r.get::<_, Option<String>>(0),
        ).unwrap().unwrap(),
        store.conn().query_row(
            "SELECT applied_result_memory_id FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3 AND candidate_id=?4",
            rusqlite::params![scope.tenant_id, scope.user_id, adj.id, ids[0]],
            |r| r.get::<_, Option<String>>(0),
        ).unwrap().unwrap(),
    );
    assert_ne!(d_mem, a_mem, "keep_separate 与 create 是两条独立 L1");
    // e/f：候选 held。
    let held: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3 AND status='held'",
            rusqlite::params![scope.tenant_id, scope.user_id, claimed.id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(held, 2, "conflict/defer 候选 held");
    // g：候选 rejected。
    let g_status: String = store
        .conn()
        .query_row(
            "SELECT status FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, ids[6]],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(g_status, "rejected");
    // c update 后 target 已 superseded（旧版保留可审计）。
    let (t_status, t_v2): (String, i64) = store
        .conn()
        .query_row(
            "SELECT status, version FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((t_status.as_str(), t_v2), ("superseded", tver + 1));
    let _ = thash;
}

#[test]
fn adjudication_stale_on_target_drift() {
    // doc6/09 §4.B.4：目标在模型运行中被并发修改/失效 → 整批 StaleInput 不部分提交。
    let (mut store, scope, origin) = setup("drift");
    let target = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
    let content = "用户住在杭州，用户喜欢简短回答。";
    let ev = {
        let t = chrono::Utc::now();
        match store.record_evidence(&scope, &origin, 2, "user", "user", &t, content).unwrap() {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    let _job = store.dream_trigger(&scope, "manual", "k1", None, None, None).unwrap().unwrap();
    // claim 时钟须 >= run_after（trigger 内部落库时刻），故在 trigger 后取。
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 900).unwrap().unwrap();
    let (sb, eb) = {
        let s = content.find("用户喜欢简短回答").unwrap();
        (s as i64, (s + "用户喜欢简短回答".len()) as i64)
    };
    // 真实提交候选（adjudication_job_inputs FK 指向 dream_candidates）。
    let proposal = crate::dream_jobs::DreamProposal {
        kind: "preference".into(),
        claim: "用户喜欢简短回答".into(),
        quote: "用户喜欢简短回答".into(),
        evidence_id: ev.clone(),
        start_byte: sb,
        end_byte: eb,
        status: "candidate".into(),
        reason_code: None,
        occurred_at: None,
    };
    let (accepted, _) = store
        .dream_submit_candidates(&scope, &claimed.id, claimed.claim_generation, crate::dream_jobs::DREAM_POLICY_V1, &[proposal])
        .unwrap();
    assert_eq!(accepted, 1);
    let cid: String = store
        .conn()
        .query_row(
            "SELECT id FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, claimed.id],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    let inputs = vec![AdjudicationCandidate {
        candidate_id: cid.clone(),
        kind: "preference".into(),
        claim: "用户喜欢简短回答".into(),
        quote: "用户喜欢简短回答".into(),
        status: "candidate".into(),
        evidence_id: ev.clone(),
        start_byte: sb,
        end_byte: eb,
    }];
    let recalls = vec![AdjudicationRecall {
        candidate_id: cid.clone(),
        target_memory_id: target.clone(),
        target_version: 1,
        channel: "exact".into(),
    }];
    let adj = store
        .adjudication_create(&scope, &claimed.id, memory_contract::ADMISSION_VERSION_V3,
                             crate::adjudication::ADJUDICATE_V1, Some("m1"), &inputs, &recalls)
        .unwrap()
        .unwrap();
    let now2 = crate::now_rfc3339_pub().unwrap();
    let (_s, ajob) = store.adjudication_claim(&now2, 900).unwrap().unwrap();
    // 模型运行期间 target 被并发 superseded（版本变化）。
    store
        .conn()
        .execute(
            "UPDATE memories SET status='superseded', version=version+1, updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, target, crate::now_rfc3339_pub().unwrap()],
        )
        .unwrap();
    let proposal = AdjudicationProposal {
        candidate_id: cid.into(),
        durability: "durable".into(),
        action: "create".into(),
        reason_code: None,
        target_memory_id: None,
        expected_target_version: None,
        model_confidence: None,
        valid_until: None,
    };
    let r = store.adjudication_apply(&scope, &adj.id, ajob.claim_generation, &[proposal]);
    assert!(matches!(r, Err(StoreError::StaleInput)), "目标版本漂移 → 整批 stale");
    // 无部分提交：结果表无该候选行。
    let n: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM adjudication_results WHERE tenant_id=?1 AND user_id=?2 AND job_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, adj.id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(n, 0, "stale 整批不落任何结果");
}

#[test]
fn adjudicate_v1_parser_strict() {
    // doc6/09 §5：严格 JSON、字段集固定、枚举合法、confidence 仅诊断。
    let ok = r#"{"results":[{"candidate_id":"c1","durability":"durable","action":"create","reason_code":null,"target_memory_id":null,"expected_target_version":null,"model_confidence":0.8,"valid_until":null}]}"#;
    assert_eq!(parse_adjudicate_v1(ok).unwrap().len(), 1);
    let fenced = format!("```json\n{ok}\n```");
    assert_eq!(parse_adjudicate_v1(&fenced).unwrap().len(), 1);
    assert!(parse_adjudicate_v1("not json").is_err());
    // 未知 action / 未知 durability 拒绝。
    assert!(parse_adjudicate_v1(
        r#"{"results":[{"candidate_id":"c1","durability":"durable","action":"merge"}]}"#
    )
    .is_err());
    assert!(parse_adjudicate_v1(
        r#"{"results":[{"candidate_id":"c1","durability":"forever","action":"create"}]}"#
    )
    .is_err());
    // 未知字段拒绝。
    assert!(parse_adjudicate_v1(
        r#"{"results":[{"candidate_id":"c1","durability":"durable","action":"create","extra":1}]}"#
    )
    .is_err());
    // confidence 超 [0,1] 拒绝（schema 违规，不是阈值门）。
    assert!(parse_adjudicate_v1(
        r#"{"results":[{"candidate_id":"c1","durability":"durable","action":"create","model_confidence":1.5}]}"#
    )
    .is_err());
}

#[test]
fn adjudication_claim_lifecycle_and_provider_wait_resume() {
    // doc6/09 §7：provider_wait 保留冻结输入，到期自动续作；attempt 计数。
    let (mut store, scope, origin) = setup("lifecycle");
    let content = "用户住在杭州。";
    let ev = {
        let t = chrono::Utc::now();
        match store.record_evidence(&scope, &origin, 1, "user", "user", &t, content).unwrap() {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    };
    let _job = store.dream_trigger(&scope, "manual", "k1", None, None, None).unwrap().unwrap();
    // claim 时钟须 >= run_after（trigger 内部落库时刻），故在 trigger 后取。
    let now = crate::now_rfc3339_pub().unwrap();
    let claimed = store.dream_claim(&scope, &now, 900).unwrap().unwrap();
    let (sb, eb) = {
        let s = content.find("用户住在杭州").unwrap();
        (s as i64, (s + "用户住在杭州".len()) as i64)
    };
    // 真实提交候选（adjudication_job_inputs FK 指向 dream_candidates）。
    let proposal = crate::dream_jobs::DreamProposal {
        kind: "fact".into(),
        claim: "用户住在杭州".into(),
        quote: "用户住在杭州".into(),
        evidence_id: ev.clone(),
        start_byte: sb,
        end_byte: eb,
        status: "candidate".into(),
        reason_code: None,
        occurred_at: None,
    };
    let (accepted, _) = store
        .dream_submit_candidates(&scope, &claimed.id, claimed.claim_generation, crate::dream_jobs::DREAM_POLICY_V1, &[proposal])
        .unwrap();
    assert_eq!(accepted, 1);
    let cid: String = store
        .conn()
        .query_row(
            "SELECT id FROM dream_candidates WHERE tenant_id=?1 AND user_id=?2 AND dream_job_id=?3",
            rusqlite::params![scope.tenant_id, scope.user_id, claimed.id],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    let inputs = vec![AdjudicationCandidate {
        candidate_id: cid.clone(),
        kind: "fact".into(),
        claim: "用户住在杭州".into(),
        quote: "用户住在杭州".into(),
        status: "candidate".into(),
        evidence_id: ev,
        start_byte: sb,
        end_byte: eb,
    }];
    let adj = store
        .adjudication_create(&scope, &claimed.id, memory_contract::ADMISSION_VERSION_V3,
                             crate::adjudication::ADJUDICATE_V1, Some("m1"), &inputs, &[])
        .unwrap()
        .unwrap();
    let now2 = crate::now_rfc3339_pub().unwrap();
    let (_s, ajob) = store.adjudication_claim(&now2, 900).unwrap().unwrap();
    assert_eq!(ajob.id, adj.id);
    // provider_wait（带退避）→ evidence 保持 assigned → 到期续作。
    assert!(store.adjudication_finish(&scope, &adj.id, ajob.claim_generation, "provider_wait", Some("MODEL_TIMEOUT"), None, None, None, Some(5)).unwrap());
    // 同 fingerprint 重放：续作返回同一作业（重试不换输入）。
    let again = store
        .adjudication_create(&scope, &claimed.id, memory_contract::ADMISSION_VERSION_V3,
                             crate::adjudication::ADJUDICATE_V1, Some("m1"), &inputs, &[])
        .unwrap()
        .unwrap();
    assert_eq!(again.id, adj.id, "同指纹幂等（重试不换输入）");
    // 6 秒后可再领取（provider_wait 到期即端点恢复续作；run_after 为 finish 内部
    // 时钟 + 5s，晚于 now2，故再补 1 秒余量）。
    let later = (chrono::DateTime::parse_from_rfc3339(&now2).unwrap() + chrono::Duration::seconds(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let (_s2, resumed) = store.adjudication_claim(&later, 900).unwrap().unwrap();
    assert_eq!(resumed.id, adj.id, "provider_wait 到期续作");
    // Dream 账本证据保持 assigned（不被误标 processed）。
    let est: String = store
        .conn()
        .query_row(
            "SELECT status FROM dream_evidence_state WHERE evidence_id=?1",
            rusqlite::params![store.dream_input_evidence_ids(&scope, &claimed.id).unwrap()[0]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(est, "assigned");
}
