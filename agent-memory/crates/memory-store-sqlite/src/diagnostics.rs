//! 只读诊断查询（doc4/04）：作业列表/详情、held 候选查看、doctor 聚合计数。
//! 本模块不提供任何状态写入；列表只返回诊断字段，不返回事件正文、Prompt 或密钥。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};

use crate::{now_rfc3339, Store, StoreError};

/// 作业列表项（GET /v1/jobs 与 CLI 共用；doc4/04 §1）。
#[derive(Debug, Clone)]
pub struct JobListItem {
    pub id: String,
    pub host_id: String,
    pub session_id: String,
    pub through_event_seq: i64,
    pub status: String,
    pub attempts: i32,
    pub run_after: String,
    pub lease_until: Option<String>,
    pub error_code: Option<String>,
    /// 存在 extraction_job_skips 行（doc4/03 §4）。
    pub skipped: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 作业详情：列表字段 + prompt_version、admission_version、window_key 与可用的模型用量。
#[derive(Debug, Clone)]
pub struct JobDetail {
    pub item: JobListItem,
    pub window_key: String,
    pub prompt_version: String,
    /// 生成该作业时的准入规则版本（doc5/03 §1，迁移 0004）。
    pub admission_version: String,
    pub model_name: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

const JOB_LIST_COLUMNS: &str = "id, host_id, session_id, through_event_seq, status, attempts, \
     run_after, lease_until, error_code, \
     EXISTS(SELECT 1 FROM extraction_job_skips s \
            WHERE s.tenant_id=extraction_jobs.tenant_id \
              AND s.user_id=extraction_jobs.user_id AND s.job_id=extraction_jobs.id), \
     created_at, updated_at";

fn job_list_mapper() -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<JobListItem> {
    |r: &rusqlite::Row<'_>| {
        Ok(JobListItem {
            id: r.get(0)?,
            host_id: r.get(1)?,
            session_id: r.get(2)?,
            through_event_seq: r.get(3)?,
            status: r.get(4)?,
            attempts: r.get(5)?,
            run_after: r.get(6)?,
            lease_until: r.get(7)?,
            error_code: r.get(8)?,
            skipped: r.get::<_, i64>(9)? != 0,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    }
}

fn candidate_list_mapper() -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<CandidateListItem> {
    |r: &rusqlite::Row<'_>| {
        let quote: String = r.get(5)?;
        Ok(CandidateListItem {
            id: r.get(0)?,
            kind: r.get(1)?,
            reason_code: r.get(2)?,
            created_at: r.get(3)?,
            primary_evidence_id: r.get(4)?,
            quote_len: quote.chars().count(),
        })
    }
}

impl Store {
    /// scope 内分页作业列表（doc4/04 §1）。`status` 为五个状态之一或 `all`；
    /// cursor 是排序键 `(created_at,id)` 的 keyset 边界（由 HTTP 层编码/校验）。
    /// SQL 的每一分支都显式带 (tenant_id,user_id)。
    pub fn list_jobs(
        &self,
        scope: &ScopeKey,
        status: &str,
        limit: usize,
        cursor: Option<(&str, &str)>,
    ) -> Result<Vec<JobListItem>, StoreError> {
        let mut sql = format!(
            "SELECT {JOB_LIST_COLUMNS} FROM extraction_jobs
             WHERE tenant_id=?1 AND user_id=?2"
        );
        if status != "all" {
            sql.push_str(" AND status=?3");
        }
        if cursor.is_some() {
            sql.push_str(" AND (created_at < ?4 OR (created_at = ?4 AND id < ?5))");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ");
        sql.push_str(&(limit as i64).to_string());
        let mut stmt = self.conn().prepare(&sql)?;
        // 参数按位置绑定：status='all' 时不绑定 ?3，cursor 参数相应前移。
        let rows = if status != "all" {
            match cursor {
                Some((c_at, c_id)) => stmt.query_map(
                    params![scope.tenant_id, scope.user_id, status, c_at, c_id],
                    job_list_mapper(),
                )?,
                None => stmt.query_map(
                    params![scope.tenant_id, scope.user_id, status],
                    job_list_mapper(),
                )?,
            }
        } else {
            match cursor {
                Some((c_at, c_id)) => stmt.query_map(
                    params![scope.tenant_id, scope.user_id, c_at, c_id],
                    job_list_mapper(),
                )?,
                None => stmt.query_map(params![scope.tenant_id, scope.user_id], job_list_mapper())?,
            }
        };
        let out = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// scope 内作业详情；跨 scope/缺失一律 None（HTTP 404，不泄露存在性）。
    pub fn get_job_detail(&self, scope: &ScopeKey, job_id: &str) -> Result<Option<JobDetail>, StoreError> {
        let row = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {JOB_LIST_COLUMNS}, window_key, prompt_version, admission_version,
                            model_name, input_tokens, output_tokens
                     FROM extraction_jobs WHERE tenant_id=?1 AND user_id=?2 AND id=?3"
                ),
                params![scope.tenant_id, scope.user_id, job_id],
                |r| {
                    Ok(JobDetail {
                        item: job_list_mapper()(r)?,
                        window_key: r.get(12)?,
                        prompt_version: r.get(13)?,
                        admission_version: r.get(14)?,
                        model_name: r.get(15)?,
                        input_tokens: r.get(16)?,
                        output_tokens: r.get(17)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// doctor 聚合计数（doc4/04 §3）：全部 scope 聚合，只给总数，不给用户列表。
    pub fn job_doctor_stats(&self) -> Result<JobDoctorStats, StoreError> {
        let count = |status: &str| -> Result<i64, StoreError> {
            Ok(self.conn().query_row(
                "SELECT count(*) FROM extraction_jobs WHERE status=?1",
                params![status],
                |r| r.get(0),
            )?)
        };
        let queued = count("queued")?;
        let running = count("running")?;
        let retryable = count("retryable_failed")?;
        let succeeded = count("succeeded")?;
        let dead = count("dead")?;
        let lease_expired_running: i64 = self.conn().query_row(
            "SELECT count(*) FROM extraction_jobs
             WHERE status='running' AND (lease_until IS NULL OR lease_until<=?1)",
            params![now_rfc3339()?],
            |r| r.get(0),
        )?;
        let (dead_skipped, dead_unskipped): (i64, i64) = {
            let skipped: i64 = self.conn().query_row(
                "SELECT count(*) FROM extraction_job_skips s
                 JOIN extraction_jobs j ON j.tenant_id=s.tenant_id AND j.user_id=s.user_id
                       AND j.id=s.job_id
                 WHERE j.status='dead'",
                [],
                |r| r.get(0),
            )?;
            (skipped, dead - skipped)
        };
        let held_candidates: i64 = self.conn().query_row(
            "SELECT count(*) FROM memory_candidates WHERE status='held'",
            [],
            |r| r.get(0),
        )?;
        let oldest_pending: Option<String> = self
            .conn()
            .query_row(
                "SELECT MIN(created_at) FROM extraction_jobs
                 WHERE status IN ('queued','retryable_failed','running')",
                [],
                |r| r.get::<_, Option<String>>(0),
            )?
            .filter(|s| !s.is_empty());
        let oldest_pending_age_secs = oldest_pending
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds());
        Ok(JobDoctorStats {
            queued,
            running,
            retryable_failed: retryable,
            succeeded,
            dead,
            lease_expired_running,
            dead_unskipped,
            dead_skipped,
            held_candidates,
            oldest_pending_age_secs,
        })
    }

    /// scope 内候选列表（只读，doc4/04 §2）。`status` 限于三状态之一；
    /// `before` 必须是本 scope 真实候选 ID，以其 (created_at,id) 为 keyset 边界。
    pub fn list_candidates(
        &self,
        scope: &ScopeKey,
        status: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Result<Vec<CandidateListItem>, StoreError> {
        let boundary: Option<String> = match before {
            None => None,
            Some(id) => self
                .conn()
                .query_row(
                    "SELECT created_at FROM memory_candidates
                     WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(StoreError::JobNotFound)?,
        };
        let mut sql = String::from(
            "SELECT id, kind, reason_code, created_at, primary_evidence_id, quote
             FROM memory_candidates
             WHERE tenant_id=?1 AND user_id=?2 AND status=?3",
        );
        if boundary.is_some() {
            sql.push_str(" AND (created_at < ?4 OR (created_at = ?4 AND id < ?5))");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ");
        sql.push_str(&(limit as i64).to_string());
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = match boundary {
            Some(b) => stmt.query_map(
                params![scope.tenant_id, scope.user_id, status, b, before.unwrap()],
                candidate_list_mapper(),
            )?,
            None => stmt.query_map(
                params![scope.tenant_id, scope.user_id, status],
                candidate_list_mapper(),
            )?,
        };
        let out = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// 候选详情（只读）：含 quote 与证据定位；本机交互终端使用，不进日志。
    pub fn get_candidate(
        &self,
        scope: &ScopeKey,
        id: &str,
    ) -> Result<Option<CandidateDetail>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT c.id, c.kind, c.status, c.reason_code, c.created_at,
                        c.primary_evidence_id, c.quote, c.quote_sha256,
                        e.start_byte, e.end_byte
                 FROM memory_candidates c
                 LEFT JOIN candidate_evidence e
                   ON e.tenant_id=c.tenant_id AND e.user_id=c.user_id
                  AND e.candidate_id=c.id AND e.evidence_id=c.primary_evidence_id
                 WHERE c.tenant_id=?1 AND c.user_id=?2 AND c.id=?3",
                params![scope.tenant_id, scope.user_id, id],
                |r| {
                    Ok(CandidateDetail {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        status: r.get(2)?,
                        reason_code: r.get(3)?,
                        created_at: r.get(4)?,
                        primary_evidence_id: r.get(5)?,
                        quote: r.get(6)?,
                        quote_sha256: r.get(7)?,
                        evidence_start_byte: r.get(8)?,
                        evidence_end_byte: r.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}

/// doctor 聚合计数（doc4/04 §3）。
#[derive(Debug, Clone)]
pub struct JobDoctorStats {
    pub queued: i64,
    pub running: i64,
    pub retryable_failed: i64,
    pub succeeded: i64,
    pub dead: i64,
    pub lease_expired_running: i64,
    pub dead_unskipped: i64,
    pub dead_skipped: i64,
    pub held_candidates: i64,
    /// 最老待办（queued/retryable/running）距现在的秒数；无待办为 None。
    pub oldest_pending_age_secs: Option<i64>,
}

impl JobDoctorStats {
    pub fn summary(&self) -> String {
        let oldest = match self.oldest_pending_age_secs {
            Some(s) => format!("{s}s"),
            None => "-".into(),
        };
        format!(
            "jobs queued={} running={} retryable={} succeeded={} dead={}（未跳过={} 已跳过={}）\
             lease_expired_running={} held_candidates={} oldest_pending={}",
            self.queued, self.running, self.retryable_failed, self.succeeded, self.dead,
            self.dead_unskipped, self.dead_skipped, self.lease_expired_running,
            self.held_candidates, oldest
        )
    }
}

/// 候选列表项（只显示诊断字段，不返回 quote 正文）。
#[derive(Debug, Clone)]
pub struct CandidateListItem {
    pub id: String,
    pub kind: String,
    pub reason_code: Option<String>,
    pub created_at: String,
    pub primary_evidence_id: String,
    /// quote 的 Unicode 字符数。
    pub quote_len: usize,
}

/// 候选详情（含 quote 与证据定位；仅本机交互终端使用）。
#[derive(Debug, Clone)]
pub struct CandidateDetail {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub reason_code: Option<String>,
    pub created_at: String,
    pub primary_evidence_id: String,
    pub quote: String,
    pub quote_sha256: String,
    pub evidence_start_byte: Option<i64>,
    pub evidence_end_byte: Option<i64>,
}

#[cfg(test)]
mod tests {
    //! doc4/04 §1—2 的确定性检查：多 scope 隔离、分页无重复/漏页、skip 标记、
    //! doctor 聚合计数、held 候选只读查看。不涉及模型调用。
    use super::*;
    use crate::{FlushOutcome, Store};
    use memory_domain::Origin;

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations")
    }

    fn plus_secs(rfc3339: &str, secs: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn setup(tag: &str) -> Store {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-diag-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u1", &dir.join("u1.token")).unwrap();
        store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();
        store
    }

    fn scope(user: &str) -> ScopeKey {
        ScopeKey { tenant_id: "t".into(), user_id: user.into() }
    }

    fn ingest(store: &mut Store, user: &str, session: &str, seq: i64, content: &str) -> String {
        let t = chrono::Utc::now();
        let o = Origin { host_id: "dsh".into(), agent_id: "agent-a".into(), session_id: session.into() };
        match store
            .record_evidence(&scope(user), &o, seq, "user", "user", &t, content)
            .unwrap()
        {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        }
    }

    fn flush(store: &mut Store, user: &str, session: &str, through: i64) -> String {
        match store.flush_window(&scope(user), "dsh", session, through).unwrap() {
            FlushOutcome::Created { job_id, .. } => job_id,
            other => panic!("应创建作业，实际 {other:?}"),
        }
    }

    fn force_dead(store: &mut Store, job_id: &str, error_code: &str) {
        store
            .conn_mut()
            .execute(
                "UPDATE extraction_jobs SET status='dead', error_code=?2, lease_until=NULL WHERE id=?1",
                params![job_id, error_code],
            )
            .unwrap();
    }

    #[test]
    fn list_jobs_scope_isolation_filter_and_skipped_flag() {
        let mut store = setup("iso");
        ingest(&mut store, "u1", "s1", 1, "u1-s1");
        let dead_u1 = flush(&mut store, "u1", "s1", 1);
        force_dead(&mut store, &dead_u1, "WINDOW_TOO_LARGE");
        ingest(&mut store, "u1", "s2", 1, "u1-s2");
        let queued_u1 = flush(&mut store, "u1", "s2", 1);
        ingest(&mut store, "u2", "s1", 1, "u2-s1");
        let dead_u2 = flush(&mut store, "u2", "s1", 1);
        force_dead(&mut store, &dead_u2, "MODEL_TIMEOUT");

        // scope 隔离：u1 的 dead 列表不含 u2 的作业。
        let dead_u1_list = store.list_jobs(&scope("u1"), "dead", 20, None).unwrap();
        assert_eq!(dead_u1_list.len(), 1);
        assert_eq!(dead_u1_list[0].id, dead_u1);
        assert!(!dead_u1_list[0].skipped);
        let dead_u2_list = store.list_jobs(&scope("u2"), "dead", 20, None).unwrap();
        assert_eq!(dead_u2_list.len(), 1);
        assert_eq!(dead_u2_list[0].id, dead_u2);

        // 状态过滤：all 含两种状态；queued 只含 queued。
        assert_eq!(store.list_jobs(&scope("u1"), "all", 20, None).unwrap().len(), 2);
        let queued = store.list_jobs(&scope("u1"), "queued", 20, None).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, queued_u1);

        // skip 标记出现在列表中。
        assert!(store.skip_dead_job(&scope("u1"), &dead_u1, "测试跳过").unwrap());
        let after = store.list_jobs(&scope("u1"), "dead", 20, None).unwrap();
        assert_eq!(after.len(), 1, "skip 不改变列表可见性，只加标记");
        assert!(after[0].skipped);
    }

    #[test]
    fn list_jobs_cursor_pagination_no_gap_no_dup() {
        let mut store = setup("page");
        let mut ids = Vec::new();
        for i in 1..=5 {
            ingest(&mut store, "u1", &format!("s{i}"), 1, &format!("事件{i}"));
            let id = flush(&mut store, "u1", &format!("s{i}"), 1);
            force_dead(&mut store, &id, "MODEL_TIMEOUT");
            ids.push(id);
        }
        // keyset 翻页：每页 2 条，拿全 5 条，无重复无漏页。
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<(String, String)> = None;
        loop {
            let page = store
                .list_jobs(&scope("u1"), "dead", 2, cursor.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))
                .unwrap();
            if page.is_empty() {
                break;
            }
            seen.extend(page.iter().map(|j| j.id.clone()));
            let last = page.last().unwrap();
            cursor = Some((last.created_at.clone(), last.id.clone()));
            assert!(page.len() <= 2);
        }
        assert_eq!(seen.len(), 5, "分页必须拿全 5 条");
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 5, "分页不得重复");
        assert!(ids.iter().all(|id| seen.contains(id)), "分页不得漏页");
    }

    #[test]
    fn job_detail_fields_and_cross_scope_404() {
        let mut store = setup("detail");
        ingest(&mut store, "u1", "s1", 1, "我叫洛溪");
        let job_id = flush(&mut store, "u1", "s1", 1);
        let run_after = {
            let d = store.get_job_detail(&scope("u1"), &job_id).unwrap().unwrap();
            assert_eq!(d.item.status, "queued");
            assert_eq!(d.prompt_version, memory_contract::EXTRACT_PROMPT_VERSION);
            d.item.run_after.clone()
        };
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        store
            .complete_job(&job_id, claimed.claim_generation, 1, "test-model", Some(11), Some(22))
            .unwrap();
        let d = store.get_job_detail(&scope("u1"), &job_id).unwrap().unwrap();
        assert_eq!(d.item.status, "succeeded");
        assert_eq!(d.model_name.as_deref(), Some("test-model"));
        assert_eq!(d.input_tokens, Some(11));
        assert_eq!(d.output_tokens, Some(22));
        assert_eq!(d.window_key, "v1:1");
        // 跨 scope：None（HTTP 404，不泄露存在性）。
        assert!(store.get_job_detail(&scope("u2"), &job_id).unwrap().is_none());
    }

    #[test]
    fn doctor_stats_aggregate_counts() {
        let mut store = setup("doctor");
        ingest(&mut store, "u1", "s1", 1, "u1-s1");
        let d1 = flush(&mut store, "u1", "s1", 1);
        force_dead(&mut store, &d1, "WINDOW_TOO_LARGE");
        ingest(&mut store, "u2", "s1", 1, "u2-s1");
        let d2 = flush(&mut store, "u2", "s1", 1);
        force_dead(&mut store, &d2, "MODEL_TIMEOUT");
        ingest(&mut store, "u1", "s2", 1, "s2 事件");
        let _queued = flush(&mut store, "u1", "s2", 1);
        store.skip_dead_job(&scope("u1"), &d1, "跳过").unwrap();
        let stats = store.job_doctor_stats().unwrap();
        assert_eq!(stats.dead, 2, "全 scope 聚合");
        assert_eq!(stats.dead_skipped, 1);
        assert_eq!(stats.dead_unskipped, 1);
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.running, 0);
        assert_eq!(stats.lease_expired_running, 0);
        assert_eq!(stats.held_candidates, 0);
        assert!(stats.oldest_pending_age_secs.is_some(), "存在待办时最老待办时长应给出");
        let summary = stats.summary();
        assert!(!summary.contains("u1"), "聚合输出不得打印用户列表");
    }

    #[test]
    fn candidate_list_show_readonly_and_isolation() {
        let mut store = setup("cand");
        // u1：一个 held 候选。
        let ev = ingest(&mut store, "u1", "s1", 1, "我喜欢深色主题");
        let job_id = flush(&mut store, "u1", "s1", 1);
        let run_after = store.get_job_detail(&scope("u1"), &job_id).unwrap().unwrap().item.run_after;
        let claimed = store
            .claim_next_ordered_job(&plus_secs(&run_after, 1))
            .unwrap()
            .unwrap();
        let origin = Origin { host_id: "dsh".into(), agent_id: "extract".into(), session_id: "s1".into() };
        let c = memory_extract::ModelCandidate {
            source_event_id: ev.clone(),
            quote: "我喜欢深色主题".into(),
            kind: "preference".into(),
            occurred_at: None,
            valid_until: None,
            confidence: None,
        };
        let outcome = store
            .save_candidate(&scope("u1"), &claimed, &origin, &c, memory_extract::Admission::Held("NOT_EXPLICIT"))
            .unwrap();
        assert!(matches!(outcome, crate::CandidateOutcome::Held { .. }));
        let candidate_id: String = store
            .conn()
            .query_row(
                "SELECT id FROM memory_candidates WHERE tenant_id='t' AND user_id='u1' AND status='held'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // 列表：u1 可见（quote 长度 7 字符），u2 不可见。
        let u1_list = store.list_candidates(&scope("u1"), "held", 20, None).unwrap();
        assert_eq!(u1_list.len(), 1);
        assert_eq!(u1_list[0].id, candidate_id);
        assert_eq!(u1_list[0].quote_len, 7);
        assert_eq!(u1_list[0].reason_code.as_deref(), Some("NOT_EXPLICIT"));
        assert!(store.list_candidates(&scope("u2"), "held", 20, None).unwrap().is_empty());

        // before：边界 ID 跨 scope / 不存在 → JobNotFound；同 scope 的正确边界 → 空页。
        assert!(matches!(
            store.list_candidates(&scope("u2"), "held", 20, Some(&candidate_id)),
            Err(StoreError::JobNotFound)
        ));
        let fake = format!("c{candidate_id}");
        assert!(matches!(
            store.list_candidates(&scope("u1"), "held", 20, Some(fake.as_str())),
            Err(StoreError::JobNotFound)
        ));
        assert!(store.list_candidates(&scope("u1"), "held", 20, Some(&candidate_id)).unwrap().is_empty());

        // show：quote 与证据定位可见；跨 scope None。只读。
        let d1 = store.get_candidate(&scope("u1"), &candidate_id).unwrap().unwrap();
        assert_eq!(d1.quote, "我喜欢深色主题");
        assert_eq!(d1.primary_evidence_id, ev);
        assert!(d1.evidence_start_byte.is_some(), "应有证据定位 span");
        assert!(store.get_candidate(&scope("u2"), &candidate_id).unwrap().is_none());
        // 非 held 状态不在 held 列表。
        assert!(store.list_candidates(&scope("u1"), "rejected", 20, None).unwrap().is_empty());
    }
}
