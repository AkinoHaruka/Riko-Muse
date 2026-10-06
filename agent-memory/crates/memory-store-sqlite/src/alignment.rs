//! doc7（Riko-Muse）M1/M2/M3 存储层：
//! - M1：`valid_until` 到期记忆自动转 `expired`（CAS + memory_revisions 审计 + 向量失效）。
//! - M2：rupture（纠正/裂痕）事件扫描（确定性规则，memory_domain::rupture）、
//!   repair（修复）线程 7 天窗口归组与显式关线。
//! - M3：alignment synthesis（相处指南）确定性派生、版本递增、来源可溯。
//!
//! 规则与 DDL 见 doc7/01；全部变更整体包在一个 SQLite 事务内（与 resident/pages 同纪律）。
//! rupture 的 detected_at 取事件 `received_at`（服务端可观测的发生时刻），
//! RFC3339 固定微秒格式下字典序即时间序。

use memory_domain::rupture::rupture_matches;
use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension};

use crate::{now_rfc3339, Store, StoreError};

/// 修复线程归组窗口（doc7/01 §1.3）：open 线程 last_rupture_at 距本次 rupture
/// 不超过该天数则归入同线程，否则新建。
pub const REPAIR_THREAD_REGROUP_DAYS: i64 = 7;
/// M1 单批到期转换上限。
pub const EXPIRE_BATCH_LIMIT: i64 = 500;
/// rupture 扫描单批事件数上限（游标分批推进）。
pub const RUPTURE_SCAN_BATCH: i64 = 2000;
/// synthesis 再生成触发（c）：上一版本窗口距今超过该时长则重算指标。
pub const SYNTHESIS_REGEN_MIN_SECS: i64 = 24 * 60 * 60;
/// 线程 title 截断（Unicode 标量字符数，内容派生，≤80）。
pub const THREAD_TITLE_MAX_CHARS: usize = 80;
/// rupture/synthesis 列表读路径的默认与上限。
pub const RUPTURE_LIST_LIMIT: usize = 50;
pub const RUPTURE_LIST_LIMIT_MAX: usize = 200;

#[derive(Debug, Clone)]
pub struct RuptureEventRow {
    pub id: String,
    pub evidence_id: String,
    pub host_id: String,
    pub session_id: String,
    pub event_seq: i64,
    pub signal: String,
    pub cue: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub thread_id: Option<String>,
    pub detected_at: String,
}

#[derive(Debug, Clone)]
pub struct RepairThreadRow {
    pub id: String,
    pub title: String,
    pub status: String,
    pub rupture_count: i64,
    pub first_rupture_at: String,
    pub last_rupture_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AlignmentSynthesisRow {
    pub version: i64,
    pub window_since: String,
    pub window_until: String,
    pub rupture_turns: i64,
    pub user_turns: i64,
    pub correction_free_rate: f64,
    pub open_repair_threads: i64,
    pub body: String,
    pub source_refs_json: String,
    pub generated_at: String,
}

#[derive(Debug, Default)]
pub struct RuptureScanOutcome {
    /// 本次扫描覆盖的事件行数（含无命中）。
    pub scanned_events: usize,
    /// 实际新插入的 rupture 行数（幂等去重后）。
    pub inserted_ruptures: usize,
    /// 新建线程数。
    pub opened_threads: usize,
}

impl Store {
    // ---- 调度器辅助 ----

    /// 全部 principal scope（个人量级有界；M1/M2 调度器按此迭代）。
    pub fn all_scopes(&self) -> Result<Vec<ScopeKey>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT tenant_id, user_id FROM principals ORDER BY tenant_id, user_id")?;
        let rows = stmt.query_map([], |r| {
            Ok(ScopeKey {
                tenant_id: r.get(0)?,
                user_id: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ---- M1：valid_until 到期转 expired ----

    /// 到期转换（doc7/01 §3）：active 且 valid_until <= now → expired。
    /// CAS（version）+ memory_revisions 审计（actor_kind='system'）+ 向量 stale +
    /// index dirty，与 retire/correct 同一事务纪律。返回实际转换行数。
    pub fn expire_due_memories(&mut self) -> Result<usize, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let rows: Vec<(String, String, String, i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT tenant_id, user_id, id, version, claim FROM memories
                 WHERE status='active' AND valid_until IS NOT NULL AND valid_until <= ?1
                 ORDER BY valid_until ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![now, EXPIRE_BATCH_LIMIT], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut expired = 0usize;
        for (tenant_id, user_id, memory_id, version, claim) in rows {
            let scope = ScopeKey { tenant_id, user_id };
            let n = tx.execute(
                "UPDATE memories SET status='expired', version=version+1, updated_at=?1
                 WHERE tenant_id=?2 AND user_id=?3 AND id=?4
                   AND version=?5 AND status='active'",
                params![now, scope.tenant_id, scope.user_id, memory_id, version],
            )?;
            if n == 0 {
                // 并发修改（correct/forget/retire 已抢先）：本批跳过，下批重扫。
                continue;
            }
            tx.execute(
                "INSERT INTO memory_revisions
                 (tenant_id, user_id, memory_id, version, previous_claim, new_claim,
                  previous_status, new_status, actor_kind, actor_id, reason_code, changed_at)
                 VALUES (?1,?2,?3,?4,?5,?5,'active','expired','system','memoryd',
                         'valid_until_expired',?6)",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    memory_id,
                    version + 1,
                    claim,
                    now
                ],
            )?;
            Self::stale_vectors_in_tx(&tx, &scope, "memory", &memory_id)?;
            Self::mark_index_dirty(&tx)?;
            expired += 1;
        }
        tx.commit()?;
        Ok(expired)
    }

    // ---- M2：rupture 扫描 + 修复线程 ----

    /// rupture 扫描（doc7/01 §1.1–1.3）：按游标增量扫 user 事件，规则命中即落
    /// rupture_events（幂等键去重），并按 7 天窗口归组到 open 线程。
    /// detected_at = 事件 received_at；游标推进与业务写入同事务。
    pub fn rupture_scan(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<RuptureScanOutcome, StoreError> {
        let mut outcome = RuptureScanOutcome::default();
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let last_rowid: i64 = tx
            .query_row(
                "SELECT last_rowid FROM rupture_scan_cursors
                 WHERE tenant_id=?1 AND user_id=?2",
                params![scope.tenant_id, scope.user_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        // V2-S1：事件域取 evidence_domain_map（无映射按 user_main）；本扫描只处理
        // 读域集内事件，跨域事件留给对应域的扫描轮次（游标全局单调，见 doc7/04 §1.3）。
        let events: Vec<(i64, String, String, String, i64, String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT e.rowid, e.id, e.host_id, e.session_id, e.event_seq, e.content,
                        COALESCE(d.domain_id, 'user_main') AS domain_id
                 FROM evidence_events e
                 LEFT JOIN evidence_domain_map d
                   ON d.tenant_id=e.tenant_id AND d.user_id=e.user_id AND d.evidence_id=e.id
                 WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.rowid > ?3
                   AND e.role='user' AND e.source_kind='user'
                 ORDER BY e.rowid ASC LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    last_rowid,
                    RUPTURE_SCAN_BATCH
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        outcome.scanned_events = events.len();
        let mut last_scanned_rowid = last_rowid;
        // 本批触碰过的线程（新建或追加），事件循环后按 rupture_events 事实重算计数。
        let mut touched_threads: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (rowid, evidence_id, host_id, session_id, event_seq, content, event_domain) in events {
            last_scanned_rowid = rowid;
            if !dom.allows_read(&event_domain) {
                continue;
            }
            let matches = rupture_matches(&content);
            if matches.is_empty() {
                continue;
            }
            let detected_at: String = tx.query_row(
                "SELECT received_at FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, evidence_id],
                |r| r.get(0),
            )?;
            let (thread_id, opened) =
                assign_thread_tx(&tx, scope, &content, &detected_at, &now, &event_domain)?;
            if opened {
                outcome.opened_threads += 1;
            }
            touched_threads.insert(thread_id.clone());
            for m in matches {
                let n = tx.execute(
                    "INSERT OR IGNORE INTO rupture_events
                     (id, tenant_id, user_id, evidence_id, host_id, session_id, event_seq,
                      signal, cue, start_byte, end_byte, thread_id, detected_at, domain_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![
                        uuid::Uuid::now_v7().to_string(),
                        scope.tenant_id,
                        scope.user_id,
                        evidence_id,
                        host_id,
                        session_id,
                        event_seq,
                        m.signal,
                        m.cue,
                        m.start_byte as i64,
                        m.end_byte as i64,
                        thread_id,
                        detected_at,
                        event_domain
                    ],
                )?;
                if n > 0 {
                    outcome.inserted_ruptures += 1;
                }
            }
        }
        // 受影响线程按事实重算（doc7/01 §1.3：计数 = 该线程 rupture 行数；自愈防漂移）。
        for thread_id in &touched_threads {
            let (cnt, last_at): (i64, Option<String>) = tx.query_row(
                "SELECT COUNT(*), MAX(detected_at) FROM rupture_events
                 WHERE tenant_id=?1 AND user_id=?2 AND thread_id=?3",
                params![scope.tenant_id, scope.user_id, thread_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if cnt == 0 {
                tx.execute(
                    "DELETE FROM repair_threads WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                    params![scope.tenant_id, scope.user_id, thread_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE repair_threads
                     SET rupture_count=?1, last_rupture_at=?2, updated_at=?3
                     WHERE tenant_id=?4 AND user_id=?5 AND id=?6",
                    params![cnt, last_at, now, scope.tenant_id, scope.user_id, thread_id],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO rupture_scan_cursors (tenant_id, user_id, last_rowid, updated_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT (tenant_id, user_id)
             DO UPDATE SET last_rowid=?3, updated_at=?4",
            params![scope.tenant_id, scope.user_id, last_scanned_rowid, now],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    /// 修复线程列表（status 缺省全部）。
    pub fn repair_threads_list(
        &self,
        scope: &ScopeKey,
        status: Option<&str>,
        limit: usize,
        dom: &DomainScope,
    ) -> Result<Vec<RepairThreadRow>, StoreError> {
        let limit = limit.clamp(1, RUPTURE_LIST_LIMIT_MAX) as i64;
        let mut stmt = self.conn().prepare(
            "SELECT id, title, status, rupture_count, first_rupture_at, last_rupture_at,
                    closed_at, close_reason
             FROM repair_threads
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR status=?3)
               AND domain_id IN (SELECT value FROM json_each(?4))
             ORDER BY last_rupture_at DESC LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![
                scope.tenant_id,
                scope.user_id,
                status,
                dom.read_json(),
                limit
            ],
            |r| {
                Ok(RepairThreadRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    status: r.get(2)?,
                    rupture_count: r.get(3)?,
                    first_rupture_at: r.get(4)?,
                    last_rupture_at: r.get(5)?,
                    closed_at: r.get(6)?,
                    close_reason: r.get(7)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 显式关线（doc7/01 §1.3）：只有 open 可关；closed 幂等返回 false。
    pub fn repair_thread_close(
        &mut self,
        scope: &ScopeKey,
        thread_id: &str,
        reason: &str,
        actor_id: &str,
        dom: &DomainScope,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        // V2-S1：仅写域内线程可关（doc7/04 §3）。
        let (status, thread_domain): (Option<String>, Option<String>) = tx
            .query_row(
                "SELECT status, domain_id FROM repair_threads
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, thread_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .map(|(a, b)| (Some(a), Some(b)))
            .unwrap_or((None, None));
        if thread_domain.as_deref() != Some(dom.write.as_str()) {
            return Err(StoreError::ThreadNotFound);
        }
        match status.as_deref() {
            None => return Err(StoreError::ThreadNotFound),
            Some("closed") => {
                tx.commit()?;
                return Ok(false);
            }
            _ => {}
        }
        tx.execute(
            "UPDATE repair_threads
             SET status='closed', closed_at=?1, close_reason=?2, updated_at=?1
             WHERE tenant_id=?3 AND user_id=?4 AND id=?5 AND status='open'",
            params![now, reason, scope.tenant_id, scope.user_id, thread_id],
        )?;
        tx.execute(
            "INSERT INTO audit_events (id, tenant_id, user_id, actor_kind, actor_id, action,
                                       target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'user',?4,'repair_thread_close',?5,?6,?7)",
            params![
                uuid::Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                actor_id,
                thread_id,
                now,
                serde_json::json!({"reason": reason}).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 最近 rupture 事件（诊断读路径）。
    pub fn ruptures_list(
        &self,
        scope: &ScopeKey,
        limit: usize,
        dom: &DomainScope,
    ) -> Result<Vec<RuptureEventRow>, StoreError> {
        let limit = limit.clamp(1, RUPTURE_LIST_LIMIT_MAX) as i64;
        let mut stmt = self.conn().prepare(
            "SELECT id, evidence_id, host_id, session_id, event_seq, signal, cue,
                    start_byte, end_byte, thread_id, detected_at
             FROM rupture_events
             WHERE tenant_id=?1 AND user_id=?2
               AND domain_id IN (SELECT value FROM json_each(?3))
             ORDER BY detected_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, dom.read_json(), limit],
            |r| {
                Ok(RuptureEventRow {
                    id: r.get(0)?,
                    evidence_id: r.get(1)?,
                    host_id: r.get(2)?,
                    session_id: r.get(3)?,
                    event_seq: r.get(4)?,
                    signal: r.get(5)?,
                    cue: r.get(6)?,
                    start_byte: r.get(7)?,
                    end_byte: r.get(8)?,
                    thread_id: r.get(9)?,
                    detected_at: r.get(10)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ---- M3：alignment synthesis ----

    /// 最新 synthesis 版本（无则 None）。
    pub fn alignment_synthesis_latest(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<Option<AlignmentSynthesisRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT version, window_since, window_until, rupture_turns, user_turns,
                        correction_free_rate, open_repair_threads, body, source_refs_json,
                        generated_at
                 FROM alignment_synthesis
                 WHERE tenant_id=?1 AND user_id=?2
                   AND domain_id IN (SELECT value FROM json_each(?3))
                 ORDER BY version DESC LIMIT 1",
                params![scope.tenant_id, scope.user_id, dom.read_json()],
                |r| {
                    Ok(AlignmentSynthesisRow {
                        version: r.get(0)?,
                        window_since: r.get(1)?,
                        window_until: r.get(2)?,
                        rupture_turns: r.get(3)?,
                        user_turns: r.get(4)?,
                        correction_free_rate: r.get(5)?,
                        open_repair_threads: r.get(6)?,
                        body: r.get(7)?,
                        source_refs_json: r.get(8)?,
                        generated_at: r.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// synthesis 再生成（doc7/01 §1.4 触发策略）：无版本 / 累计 rupture 数超出已计入
    /// 各版本之和（覆盖回填）/ open 线程数与最新版本不一致 / 最新窗口距今超 24h。
    /// 任一满足才生成新版本；否则幂等返回当前最新。
    pub fn alignment_synthesis_refresh(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<AlignmentSynthesisRow, StoreError> {
        // V2-S1：synthesis 按域独立版本链；指标/线程/事件全部按写域过滤（doc7/04 §1.3）。
        let domain = &dom.write;
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let latest: Option<(i64, String, i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT version, window_until, rupture_turns, open_repair_threads, user_turns,
                        (SELECT COALESCE(SUM(rupture_turns),0) FROM alignment_synthesis
                          WHERE tenant_id=alignment_synthesis.tenant_id
                            AND user_id=alignment_synthesis.user_id
                            AND domain_id=alignment_synthesis.domain_id)
                 FROM alignment_synthesis
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3
                 ORDER BY version DESC LIMIT 1",
                params![scope.tenant_id, scope.user_id, domain],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let total_ruptures: i64 = tx.query_row(
            "SELECT COUNT(*) FROM rupture_events WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
            params![scope.tenant_id, scope.user_id, domain],
            |r| r.get(0),
        )?;
        let open_threads: i64 = tx.query_row(
            "SELECT COUNT(*) FROM repair_threads
             WHERE tenant_id=?1 AND user_id=?2 AND status='open' AND domain_id=?3",
            params![scope.tenant_id, scope.user_id, domain],
            |r| r.get(0),
        )?;
        let due = match &latest {
            None => true,
            Some((_, window_until, _, latest_open, _, counted_ruptures)) => {
                total_ruptures > *counted_ruptures
                    || open_threads != *latest_open
                    || window_until_older_than(&now, window_until, SYNTHESIS_REGEN_MIN_SECS)?
            }
        };
        if !due {
            let (latest_version, ..) = latest.expect("latest exists when !due");
            let row = tx
                .query_row(
                    "SELECT version, window_since, window_until, rupture_turns, user_turns,
                            correction_free_rate, open_repair_threads, body, source_refs_json,
                            generated_at
                     FROM alignment_synthesis
                     WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND version=?4",
                    params![scope.tenant_id, scope.user_id, domain, latest_version],
                    map_synthesis_row,
                )
                .optional()?
                .expect("latest version row must exist");
            tx.commit()?;
            return Ok(row);
        }
        let window_since = match latest.as_ref().map(|(_, w, ..)| w.clone()) {
            Some(w) => w,
            None => {
                // 首版：从最早 user 事件起算；无任何事件则空窗口 [now, now)。
                tx.query_row(
                    "SELECT MIN(e.received_at) FROM evidence_events e
                     LEFT JOIN evidence_domain_map d
                       ON d.tenant_id=e.tenant_id AND d.user_id=e.user_id AND d.evidence_id=e.id
                     WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.role='user' AND e.source_kind='user'
                       AND COALESCE(d.domain_id, 'user_main')=?3",
                    params![scope.tenant_id, scope.user_id, domain],
                    |r| r.get::<_, Option<String>>(0),
                )?
                .unwrap_or_else(|| now.clone())
            }
        };
        let rupture_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM rupture_events
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?5
                   AND detected_at >= ?3 AND detected_at < ?4
                 ORDER BY detected_at ASC, id ASC",
            )?;
            let rows = stmt.query_map(
                params![scope.tenant_id, scope.user_id, window_since, now, domain],
                |r| r.get::<_, String>(0),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let rupture_turns = rupture_ids.len() as i64;
        let user_turns: i64 = tx.query_row(
            "SELECT COUNT(*) FROM evidence_events e
             LEFT JOIN evidence_domain_map d
               ON d.tenant_id=e.tenant_id AND d.user_id=e.user_id AND d.evidence_id=e.id
             WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.role='user' AND e.source_kind='user'
               AND COALESCE(d.domain_id, 'user_main')=?5
               AND e.received_at >= ?3 AND e.received_at < ?4",
            params![scope.tenant_id, scope.user_id, window_since, now, domain],
            |r| r.get(0),
        )?;
        let open_thread_rows: Vec<(String, String, i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, title, rupture_count, last_rupture_at FROM repair_threads
                 WHERE tenant_id=?1 AND user_id=?2 AND status='open' AND domain_id=?3
                 ORDER BY last_rupture_at DESC",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, domain], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let correction_free_rate = if user_turns == 0 {
            1.0
        } else {
            (1.0 - rupture_turns as f64 / user_turns as f64).clamp(0.0, 1.0)
        };
        let source_refs = serde_json::json!({
            "rupture_event_ids": rupture_ids,
            "thread_ids": open_thread_rows.iter().map(|(id, _, _, _)| id).collect::<Vec<_>>(),
        });
        let body = render_synthesis_body(
            &window_since,
            &now,
            rupture_turns,
            user_turns,
            correction_free_rate,
            &open_thread_rows,
        );
        let version = match latest.as_ref().map(|(v, ..)| *v) {
            Some(v) => v + 1,
            None => 1,
        };
        tx.execute(
            "INSERT INTO alignment_synthesis
             (tenant_id, user_id, domain_id, version, window_since, window_until, rupture_turns,
              user_turns, correction_free_rate, open_repair_threads, body, source_refs_json,
              generated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                scope.tenant_id,
                scope.user_id,
                domain,
                version,
                window_since,
                now,
                rupture_turns,
                user_turns,
                correction_free_rate,
                open_threads,
                body,
                source_refs.to_string(),
                now
            ],
        )?;
        let row = tx
            .query_row(
                "SELECT version, window_since, window_until, rupture_turns, user_turns,
                        correction_free_rate, open_repair_threads, body, source_refs_json,
                        generated_at
                 FROM alignment_synthesis
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND version=?4",
                params![scope.tenant_id, scope.user_id, domain, version],
                map_synthesis_row,
            )
            .expect("just inserted");
        tx.commit()?;
        Ok(row)
    }
}

fn map_synthesis_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlignmentSynthesisRow> {
    Ok(AlignmentSynthesisRow {
        version: r.get(0)?,
        window_since: r.get(1)?,
        window_until: r.get(2)?,
        rupture_turns: r.get(3)?,
        user_turns: r.get(4)?,
        correction_free_rate: r.get(5)?,
        open_repair_threads: r.get(6)?,
        body: r.get(7)?,
        source_refs_json: r.get(8)?,
        generated_at: r.get(9)?,
    })
}

/// 线程归组（doc7/01 §1.3）：最近的 open 线程 last_rupture_at 落在
/// [detected_at - 7d, detected_at] 内则复用，否则新建（title = 事件摘要截 80 字符）。
/// 自由函数：tx 已独占 &mut Store 借用。
fn assign_thread_tx(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    content: &str,
    detected_at: &str,
    now: &str,
    domain: &str,
) -> Result<(String, bool), StoreError> {
    // V2-S1：线程按域归组，跨域事件不合线（doc7/04 §1.3）。
    let regroup_since = regroup_since_str(detected_at, REPAIR_THREAD_REGROUP_DAYS)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM repair_threads
             WHERE tenant_id=?1 AND user_id=?2 AND status='open'
               AND domain_id=?3
               AND last_rupture_at >= ?4
             ORDER BY last_rupture_at DESC LIMIT 1",
            params![scope.tenant_id, scope.user_id, domain, regroup_since],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok((id, false));
    }
    let id = uuid::Uuid::now_v7().to_string();
    let title: String = content.chars().take(THREAD_TITLE_MAX_CHARS).collect();
    tx.execute(
        "INSERT INTO repair_threads
         (id, tenant_id, user_id, title, status, rupture_count,
          first_rupture_at, last_rupture_at, created_at, updated_at, domain_id)
         VALUES (?1,?2,?3,?4,'open',1,?5,?5,?6,?6,?7)",
        params![
            id,
            scope.tenant_id,
            scope.user_id,
            title,
            detected_at,
            now,
            domain
        ],
    )?;
    Ok((id, true))
}

/// RFC3339（固定微秒 + Z）字符串下推算 `ts - days`，保持同一格式便于字典序比较。
fn regroup_since_str(ts: &str, days: i64) -> Result<String, StoreError> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts)
        .map_err(|e| StoreError::Time(format!("解析 {ts} 失败: {e}")))?;
    let since = dt - chrono::Duration::days(days);
    Ok(since.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

/// `now - secs > ts`（窗口过期判定）。
fn window_until_older_than(now: &str, ts: &str, secs: i64) -> Result<bool, StoreError> {
    let now_dt = chrono::DateTime::parse_from_rfc3339(now)
        .map_err(|e| StoreError::Time(format!("解析 {now} 失败: {e}")))?;
    let ts_dt = chrono::DateTime::parse_from_rfc3339(ts)
        .map_err(|e| StoreError::Time(format!("解析 {ts} 失败: {e}")))?;
    Ok(now_dt - ts_dt > chrono::Duration::seconds(secs))
}

/// 确定性渲染（doc7/01 §1.4）：结构化相处指南，所有来源 ID 内联可溯；非模型生成。
fn render_synthesis_body(
    window_since: &str,
    window_until: &str,
    rupture_turns: i64,
    user_turns: i64,
    correction_free_rate: f64,
    open_threads: &[(String, String, i64, String)],
) -> String {
    let mut s = String::new();
    s.push_str("# 相处指南（alignment synthesis，确定性派生）\n");
    s.push_str(&format!("窗口：{window_since} → {window_until}\n"));
    s.push_str(&format!(
        "指标：纠正轮次 {rupture_turns} / 用户轮次 {user_turns}（无纠正率 {correction_free_rate:.2}）；待修复线程 {}。\n",
        open_threads.len()
    ));
    s.push_str("\n## 待修复线程\n");
    if open_threads.is_empty() {
        s.push_str("- 无待修复线程。\n");
    } else {
        for (id, title, count, last_at) in open_threads {
            s.push_str(&format!(
                "- [{id}] {title}（open，累计 {count} 次，最近 {last_at}）\n"
            ));
        }
    }
    s.push_str("\n指导：优先复核上述线程对应话题；新对话中避免重复触发同类纠正。\n");
    s
}
