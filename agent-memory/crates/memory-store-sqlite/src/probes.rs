//! doc5/05 §2 离线召回探针（doc5 卡 D5-5）：从规范表构造临时 SQLite，固定查询集，
//! 按 memory_id 断言 search/compose 的预期注入与预期缺席。不调用任何外部模型；
//! 确定性检查只证明召回规则执行正确，不证明真实模型利用率或回答质量。
//! 已知零词法盲区按 doc5/05 §2 记录为未解决缺口，不在本卡修 compose。
#[cfg(test)]
mod probes {
    use crate::{IngestOutcome, RememberOutcome, Store};
    use memory_domain::{MemoryKind, Origin, ScopeKey};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations")
    }

    /// 单条探针记录（doc5/05 §2 格式的本机执行版）。
    struct ProbeRecord {
        probe_id: &'static str,
        query: String,
        /// 词法机会档位：直接词 / 历史词 / 零相关。
        lexical: &'static str,
        expected_inject: Vec<String>,
        actual_inject: Vec<String>,
        expected_absent: Vec<String>,
        leaked_absent: Vec<String>,
        index_degraded: bool,
        notes: String,
        passed: bool,
    }

    impl ProbeRecord {
        fn to_json(&self) -> serde_json::Value {
            serde_json::json!({
                "probe_id": self.probe_id,
                "query": self.query,
                "lexical_opportunity": self.lexical,
                "expected_inject": self.expected_inject,
                "actual_inject": self.actual_inject,
                "expected_absent": self.expected_absent,
                "leaked_absent": self.leaked_absent,
                "index_degraded": self.index_degraded,
                "notes": self.notes,
                "passed": self.passed,
            })
        }
    }

    fn u1() -> ScopeKey {
        ScopeKey { tenant_id: "t".into(), user_id: "u1".into() }
    }

    fn u2() -> ScopeKey {
        ScopeKey { tenant_id: "t".into(), user_id: "u2".into() }
    }

    fn origin(session: &str, agent: &str) -> Origin {
        Origin { host_id: "dsh".into(), agent_id: agent.into(), session_id: session.into() }
    }

    /// 记入最新用户消息并经 remember 直写（普通内容沿既有路径）。
    fn save_direct(
        store: &mut Store,
        scope: &ScopeKey,
        session: &str,
        seq: i64,
        message: &str,
        quote: &str,
        kind: MemoryKind,
    ) -> String {
        let t = chrono::Utc::now();
        let ev = match store
            .record_evidence(scope, &origin(session, "agent-a"), seq, "user", "user", &t, message)
            .unwrap()
        {
            IngestOutcome::Recorded(id) => id,
            IngestOutcome::AlreadyRecorded(id) => id,
            other => panic!("意外 ingest 结果: {other:?}"),
        };
        match store.remember(scope, &origin(session, "agent-a"), &ev, quote, kind).unwrap() {
            RememberOutcome::Created { memory_id, .. } | RememberOutcome::Dedup { memory_id, .. } => {
                memory_id
            }
        }
    }

    fn compose_ids(store: &Store, scope: &ScopeKey, query: &str, max_items: usize, max_chars: usize) -> (Vec<String>, bool, String) {
        let r = store.compose_context(scope, "agent-b", query, max_items, max_chars).unwrap();
        (r.items.into_iter().map(|(id, _)| id).collect(), r.truncated, r.text)
    }

    #[test]
    fn recall_probe_matrix_offline() {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-probes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u1", &dir.join("u1.token")).unwrap();
        store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();
        let mut report: Vec<ProbeRecord> = Vec::new();

        // ---- P1 该生效：同用户跨 Agent 词法命中 + 跨用户隔离（doc5/05 §2）----
        let p1_u1 = save_direct(
            &mut store, &u1(), "s1", 1,
            "我喜欢用暗色主题写代码", "我喜欢用暗色主题写代码", MemoryKind::Preference,
        );
        // 干扰：另一用户的同句（Agent C 写入，Agent B 查询）。
        let p1_u2 = save_direct(
            &mut store, &u2(), "s1", 1,
            "我喜欢用暗色主题写代码", "我喜欢用暗色主题写代码", MemoryKind::Preference,
        );
        let (ids, degraded, text) = compose_ids(&store, &u1(), "VSCode 暗色主题怎么配", 5, 2000);
        let p1 = ProbeRecord {
            probe_id: "P1_should_activate_cross_agent",
            query: "VSCode 暗色主题怎么配".into(),
            lexical: "直接词",
            expected_inject: vec![p1_u1.clone()],
            actual_inject: ids.clone(),
            expected_absent: vec![p1_u2.clone()],
            leaked_absent: ids.iter().filter(|id| **id == p1_u2).cloned().collect(),
            index_degraded: degraded,
            notes: "同用户 Agent A 写入、Agent B 查询由词法命中注入；另一用户同句不参与".into(),
            passed: ids == vec![p1_u1.clone()],
        };
        assert!(p1.passed, "P1 应只注入 u1 的偏好，实际 {ids:?}");
        assert!(!text.contains(p1_u2.as_str()), "跨用户 memory_id 不得出现");
        report.push(p1);

        // ---- P2 正确沉默：无关查询不注入偏好；指令仍占名额 ----
        let p2_pref = p1_u1.clone();
        let p2_instr = save_direct(
            &mut store, &u1(), "s1", 2,
            "以后回答请用中文", "以后回答请用中文", MemoryKind::Instruction,
        );
        let (ids, degraded, text) = compose_ids(&store, &u1(), "如何焯西兰花", 5, 2000);
        let p2 = ProbeRecord {
            probe_id: "P2_correct_silence",
            query: "如何焯西兰花".into(),
            lexical: "零相关",
            expected_inject: vec![p2_instr.clone()],
            actual_inject: ids.clone(),
            expected_absent: vec![p2_pref.clone()],
            leaked_absent: ids.iter().filter(|id| **id == p2_pref).cloned().collect(),
            index_degraded: degraded,
            notes: "偏好与查询零词法重叠时不注入；长期指令仍由独立名额注入".into(),
            passed: ids == vec![p2_instr.clone()],
        };
        assert!(p2.passed, "P2 应只注入指令，实际 {ids:?}");
        assert!(!text.contains("暗色主题"), "P2 偏好 claim 不得出现");
        report.push(p2);

        // ---- P3 已知盲区：护眼配色零词法 → 偏好不注入（记录为召回覆盖缺口）----
        // 指令仍由独立名额注入——盲区专指与查询零词法重叠的偏好/事实。
        let (ids, degraded, _) = compose_ids(&store, &u1(), "给我推荐护眼配色", 5, 2000);
        let p3 = ProbeRecord {
            probe_id: "P3_known_blind_spot",
            query: "给我推荐护眼配色".into(),
            lexical: "零相关",
            expected_inject: vec![p2_instr.clone()],
            actual_inject: ids.clone(),
            expected_absent: vec![p2_pref.clone()],
            leaked_absent: ids.iter().filter(|id| **id == p2_pref).cloned().collect(),
            index_degraded: degraded,
            notes: "已知盲区（未解决）：零词法时偏好不注入（指令不受影响）；模型可能碰巧答深色，\
                    不得把回答措辞记为记忆生效。改召回须先过 doc5/05 §3 门槛"
                .into(),
            passed: ids == vec![p2_instr.clone()],
        };
        assert!(p3.passed, "P3 盲区行为=偏好不注入，实际 {ids:?}");
        report.push(p3);

        // ---- P4 冲突：两条同属性 active 共存不自动消歧；correct 后 superseded 退出普通查询 ----
        let p4_hz = save_direct(&mut store, &u2(), "s2", 1, "我住在杭州", "我住在杭州", MemoryKind::Fact);
        let p4_cd = save_direct(&mut store, &u2(), "s2", 2, "我住在成都", "我住在成都", MemoryKind::Fact);
        let (ids_hz, _, _) = compose_ids(&store, &u2(), "杭州", 5, 2000);
        let (ids_cd, _, _) = compose_ids(&store, &u2(), "成都", 5, 2000);
        let both_active = {
            let n: usize = store
                .conn()
                .query_row(
                    "SELECT count(*) FROM memories WHERE tenant_id='t' AND user_id='u2'
                     AND kind='fact' AND status='active' AND (claim LIKE '%住在%')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            n == 2
        };
        let p4a = ProbeRecord {
            probe_id: "P4a_conflict_two_active_no_disambiguation",
            query: "杭州 / 成都".into(),
            lexical: "直接词",
            expected_inject: vec![p4_hz.clone(), p4_cd.clone()],
            actual_inject: [ids_hz.clone(), ids_cd.clone()].concat(),
            expected_absent: vec![],
            leaked_absent: vec![],
            index_degraded: false,
            notes: "两条同属性 active 共存（remember 直写不冲突检测）；普通 compose 只按查询命中，\
                    系统不声称自动消歧；需用户 correct 显式处理"
                .into(),
            passed: ids_hz == vec![p4_hz.clone()] && ids_cd == vec![p4_cd.clone()] && both_active,
        };
        assert!(p4a.passed, "P4a 各查询应命中各自 active，实际 {ids_hz:?}/{ids_cd:?}，both_active={both_active}");
        report.push(p4a);
        // correct：旧杭州 superseded，新成都 active；普通查询不再见杭州。
        {
            let t = chrono::Utc::now();
            let ev = match store
                .record_evidence(&u2(), &origin("s2", "agent-a"), 3, "user", "user", &t, "过去我住在杭州，现在我住在成都")
                .unwrap()
            {
                IngestOutcome::Recorded(id) => id,
                _ => panic!(),
            };
            store
                .correct_memory(
                    &u2(),
                    &p4_hz,
                    &crate::CorrectRequest {
                        expected_version: 1,
                        origin: origin("s2", "agent-a"),
                        user_evidence_id: ev,
                        old_quote: "我住在杭州".into(),
                        replacement_quote: "我住在成都".into(),
                    },
                )
                .unwrap();
        }
        let (ids_after, _, _) = compose_ids(&store, &u2(), "杭州", 5, 2000);
        let hist = store.search_memories(&u2(), "我住在杭州", 5, true).unwrap().0;
        let p4b = ProbeRecord {
            probe_id: "P4b_superseded_exits_normal_query",
            query: "杭州（普通） / 我住在杭州（历史）".into(),
            lexical: "直接词 / 历史词",
            expected_inject: vec![],
            actual_inject: ids_after.clone(),
            expected_absent: vec![p4_hz.clone()],
            leaked_absent: ids_after.iter().filter(|id| **id == p4_hz).cloned().collect(),
            index_degraded: false,
            notes: "correct 后旧记忆 superseded：普通查询不见；历史查询（include_history=true\
                    ，历史词门槛在 HTTP/适配器层）可见 superseded，forgotten 仍永不返回"
                .into(),
            passed: ids_after.is_empty() && hist.iter().any(|h| h.memory_id == p4_hz),
        };
        assert!(p4b.passed, "P4b superseded 应退出普通查询、历史查询可见，实际 {ids_after:?}");
        report.push(p4b);

        // ---- P5 失效/遗忘/抑制：valid_until 过期、forgotten、SUPPRESSED_SOURCE ----
        let p5_exp = save_direct(
            &mut store, &u2(), "s3", 1,
            "我喜欢骑单车通勤", "我喜欢骑单车通勤", MemoryKind::Preference,
        );
        // 夹具：从规范表直接置过期（公共 API 无写入 valid_until 的路径；探针构造许可）。
        store
            .conn()
            .execute(
                "UPDATE memories SET valid_until='2020-01-01T00:00:00Z' WHERE id=?1",
                rusqlite::params![p5_exp],
            )
            .unwrap();
        let p5_forget = save_direct(
            &mut store, &u2(), "s3", 2,
            "我喜欢手工咖啡", "我喜欢手工咖啡", MemoryKind::Preference,
        );
        {
            let t = chrono::Utc::now();
            let ev = match store
                .record_evidence(&u2(), &origin("s3", "agent-a"), 3, "user", "user", &t, "忘记我喜欢手工咖啡")
                .unwrap()
            {
                IngestOutcome::Recorded(id) => id,
                _ => panic!(),
            };
            store
                .forget_memory(
                    &u2(),
                    &p5_forget,
                    &crate::ForgetRequest {
                        expected_version: 1,
                        origin: origin("s3", "agent-a"),
                        user_evidence_id: ev,
                        target_quote: "我喜欢手工咖啡".into(),
                    },
                )
                .unwrap();
        }
        // 过期与遗忘都不得出现在 search/compose；历史查询也不得复活 forgotten。
        let (search_ids, _) = store.search_memories(&u2(), "通勤", 20, false).unwrap();
        let (hist_ids, _) = store.search_memories(&u2(), "通勤", 20, true).unwrap();
        let (c_ids, _, _) = compose_ids(&store, &u2(), "通勤 咖啡", 5, 2000);
        let p5 = ProbeRecord {
            probe_id: "P5_expired_forgotten_suppressed_invisible",
            query: "通勤 咖啡".into(),
            lexical: "直接词",
            expected_inject: vec![],
            actual_inject: c_ids.clone(),
            expected_absent: vec![p5_exp.clone(), p5_forget.clone()],
            leaked_absent: c_ids
                .iter()
                .filter(|id| **id == p5_exp || **id == p5_forget)
                .cloned()
                .collect(),
            index_degraded: false,
            notes: "valid_until<=now 与 forgotten 不得在 search/compose 出现；\
                    include_history 也不得返回 forgotten；遗忘后同旧证据重放候选 \
                    → rejected:SUPPRESSED_SOURCE（A23，jobs 测试覆盖）"
                .into(),
            passed: c_ids.is_empty()
                && search_ids.iter().all(|h| h.memory_id != p5_exp && h.memory_id != p5_forget)
                && hist_ids.iter().all(|h| h.memory_id != p5_forget),
        };
        assert!(p5.passed, "P5 过期/遗忘必须不可见，实际 search={search_ids:?} compose={c_ids:?}");
        report.push(p5);

        // ---- P6 预算：指令名额 2、max_items、单条超长跳过（不截断否定词）----
        let p6_i1 = save_direct(&mut store, &u1(), "s4", 1, "以后先说明风险再动手", "以后先说明风险再动手", MemoryKind::Instruction);
        let p6_i2 = save_direct(&mut store, &u1(), "s4", 2, "以后回答要给代码示例", "以后回答要给代码示例", MemoryKind::Instruction);
        // 第三条与 P2 指令同文 → remember 去重返回既有记忆（p6_i3 == p2_instr）并刷新
        // updated_at；名额按 updated_at DESC 取最新两条 = 该记忆与 p6_i2，p6_i1 被挤出。
        let p6_i3 = save_direct(&mut store, &u1(), "s4", 3, "以后回答请用中文", "以后回答请用中文", MemoryKind::Instruction);
        assert_eq!(p6_i3, p2_instr, "同文指令应去重为同一记忆");
        let (ids, degraded, text) = compose_ids(&store, &u1(), "随便聊聊今天天气", 5, 2000);
        let p6a = ProbeRecord {
            probe_id: "P6a_instruction_slots_two",
            query: "随便聊聊今天天气".into(),
            lexical: "零相关",
            expected_inject: vec![p6_i3.clone(), p6_i2.clone()],
            actual_inject: ids.clone(),
            expected_absent: vec![p6_i1.clone()],
            leaked_absent: ids.iter().filter(|id| **id == p6_i1).cloned().collect(),
            index_degraded: degraded,
            notes: "多条指令只有 2 个独立名额：按 updated_at DESC 取最新两条，无关查询也注入；                    第三条与 P2 指令同文，remember 幂等去重为同一记忆"
                .into(),
            passed: ids.len() == 2 && ids.contains(&p6_i3) && ids.contains(&p6_i2),
        };
        assert!(p6a.passed, "P6a 指令名额应为最新两条，实际 {ids:?}");
        assert!(!text.contains(p6_i1.as_str()), "被挤出名额的指令不得出现：{text:?}");
        report.push(p6a);

        // max_items=2：6 条同词偏好只有 2 条注入（按命中顺序）。
        let mut p6_pref_ids = Vec::new();
        for (i, w) in ["蓝", "绿", "红", "黄", "紫", "橙"].iter().enumerate() {
            let claim = format!("我喜欢{w}色便签");
            p6_pref_ids.push(save_direct(
                &mut store, &u2(), &format!("s5_{i}"), 1, &claim, &claim, MemoryKind::Preference,
            ));
        }
        let (ids, _, _) = compose_ids(&store, &u2(), "色便签", 2, 2000);
        let p6b = ProbeRecord {
            probe_id: "P6b_max_items_limit",
            query: "色便签".into(),
            lexical: "直接词",
            expected_inject: vec![],
            actual_inject: ids.clone(),
            expected_absent: p6_pref_ids[2..].to_vec(),
            leaked_absent: ids[2..].to_vec(),
            index_degraded: false,
            notes: "max_items=2 时其余命中被跳过；被跳过 ID 如实记录（doc5/05 §2 预算档）".into(),
            passed: ids.len() == 2,
        };
        assert!(p6b.passed, "P6b 应只有 2 条，实际 {ids:?}");
        report.push(p6b);

        // 单条超长跳过：max_chars 小于单行 → truncated 且整条跳过，不截断否定词。
        let p6_neg = save_direct(
            &mut store, &u2(), "s6", 1,
            "我不喜欢嘈杂环境", "我不喜欢嘈杂环境", MemoryKind::Preference,
        );
        let (ids, truncated, text) = compose_ids(&store, &u2(), "嘈杂", 5, 80);
        let p6c = ProbeRecord {
            probe_id: "P6c_oversized_item_skipped_not_truncated",
            query: "嘈杂".into(),
            lexical: "直接词",
            expected_inject: vec![],
            actual_inject: ids.clone(),
            expected_absent: vec![p6_neg.clone()],
            leaked_absent: ids.iter().filter(|id| **id == p6_neg).cloned().collect(),
            index_degraded: false,
            notes: "预算不足时整条跳过（truncated=true），绝不截出可能丢失否定词的半句".into(),
            passed: truncated && ids.is_empty() && !text.contains("我不喜欢"),
        };
        assert!(p6c.passed, "P6c 应整条跳过，实际 truncated={truncated} ids={ids:?} text={text:?}");
        report.push(p6c);
        // 预算充足时否定词完整出现。
        let (_, _, text) = compose_ids(&store, &u2(), "嘈杂", 5, 2000);
        assert!(text.contains("我不喜欢嘈杂环境"), "预算充足须完整注入否定句：{text:?}");

        // ---- 汇总报告：写临时文件并打印（不作为任何质量提升证明）----
        let all_passed = report.iter().all(|p| p.passed);
        let summary = serde_json::json!({
            "doc": "doc5/05 §2 离线召回探针（D5-5）",
            "note": "确定性本机检查：只证明召回规则执行正确；零词法盲区为未解决缺口；非模型质量验证",
            "all_passed": all_passed,
            "probes": report.iter().map(|p| p.to_json()).collect::<Vec<_>>(),
        });
        let out = std::env::temp_dir().join("agent-memory-recall-probes.json");
        std::fs::write(&out, serde_json::to_string_pretty(&summary).unwrap()).unwrap();
        println!("召回探针报告已写入 {}", out.display());
        assert!(all_passed, "存在未通过的探针");
        let _ = fs::remove_dir_all(&dir);
    }

    use std::fs;
}
