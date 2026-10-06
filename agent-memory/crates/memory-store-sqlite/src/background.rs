//! V2-B1/A1 后台闭环（doc7/08）：处理账本、任务回执与修复行动。
//!
//! 硬边界：这里**不调用任何模型**。账本保证「同一信号只消化一次」，
//! 调度判定是 \`memory_domain::schedule\` 的纯函数；模型只能**提议**修复行动，
//! 激活与关闭必须有人/可信规则的授权记录。

use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

pub const TASK_KINDS: [&str; 5] = [
    "upkeep",
    "relationships",
    "nightly",
    "quiet",
    "rupture_scan",
];
const RUN_OUTCOMES: [&str; 4] = ["ok", "empty", "error", "skipped"];
const PROPOSERS: [&str; 3] = ["model", "user", "cli"];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TaskRun {
    pub id: String,
    pub task_kind: String,
    pub outcome: String,
    pub signal_count: i64,
    pub detail: String,
    pub run_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepairAction {
    pub id: String,
    pub thread_id: String,
    pub action: String,
    pub expected_behavior: String,
    pub conditions: String,
    pub counterexamples: String,
    pub status: String,
    pub proposed_by: String,
    pub authorized_by: Option<String>,
    pub authorized_at: Option<String>,
    pub close_reason: Option<String>,
    pub recurrence_count: i64,
    pub source_memory_id: Option<String>,
    pub generator_version: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct ProposeAction<'a> {
    pub thread_id: &'a str,
    pub action: &'a str,
    pub expected_behavior: &'a str,
    pub conditions: &'a str,
    pub counterexamples: &'a str,
    pub proposed_by: &'a str,
    pub source_memory_id: Option<&'a str>,
    pub generator_version: Option<&'a str>,
}

fn map_action(r: &rusqlite::Row<'_>) -> rusqlite::Result<RepairAction> {
    Ok(RepairAction {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        action: r.get(2)?,
        expected_behavior: r.get(3)?,
        conditions: r.get(4)?,
        counterexamples: r.get(5)?,
        status: r.get(6)?,
        proposed_by: r.get(7)?,
        authorized_by: r.get(8)?,
        authorized_at: r.get(9)?,
        close_reason: r.get(10)?,
        recurrence_count: r.get(11)?,
        source_memory_id: r.get(12)?,
        generator_version: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

const ACTION_COLS: &str = "id, thread_id, action, expected_behavior, conditions, counterexamples, \
     status, proposed_by, authorized_by, authorized_at, close_reason, recurrence_count, \
     source_memory_id, generator_version, created_at, updated_at";

impl Store {
    /// 记账一个已消化的信号（doc7/08 §1）。返回 true=本次新记，false=已经记过。
    pub fn ledger_record(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        task_kind: &str,
        signal_ref: &str,
        generation: i64,
    ) -> Result<bool, StoreError> {
        if !TASK_KINDS.contains(&task_kind) || signal_ref.is_empty() {
            return Err(StoreError::StateConflict);
        }
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO processing_ledger
               (tenant_id, user_id, domain_id, task_kind, signal_ref, generation, processed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                scope.tenant_id,
                scope.user_id,
                dom.write,
                task_kind,
                signal_ref,
                generation.max(1),
                now_rfc3339()?
            ],
        )?;
        Ok(n > 0)
    }

    pub fn ledger_has(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        task_kind: &str,
        signal_ref: &str,
    ) -> Result<bool, StoreError> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM processing_ledger
             WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind=?4 AND signal_ref=?5",
            params![
                scope.tenant_id,
                scope.user_id,
                dom.write,
                task_kind,
                signal_ref
            ],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn ledger_count(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        task_kind: &str,
    ) -> Result<i64, StoreError> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM processing_ledger
             WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind=?4",
            params![scope.tenant_id, scope.user_id, dom.write, task_kind],
            |r| r.get(0),
        )?)
    }

    /// 记一次任务运行回执。失败也留痕，不吞失败当成功。
    pub fn task_run_record(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
        task_kind: &str,
        outcome: &str,
        signal_count: i64,
        detail: &str,
    ) -> Result<String, StoreError> {
        if !TASK_KINDS.contains(&task_kind) || !RUN_OUTCOMES.contains(&outcome) {
            return Err(StoreError::StateConflict);
        }
        let id = Uuid::now_v7().to_string();
        self.conn_mut().execute(
            "INSERT INTO task_runs
               (id, tenant_id, user_id, domain_id, task_kind, outcome, signal_count, detail, run_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                dom.write,
                task_kind,
                outcome,
                signal_count.max(0),
                detail,
                now_rfc3339()?
            ],
        )?;
        Ok(id)
    }

    /// 指定时间点之后的运行回执（RFC3339 字符串按字典序即时间序）。
    pub fn task_runs_since(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        task_kind: &str,
        since: &str,
    ) -> Result<Vec<TaskRun>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, task_kind, outcome, signal_count, detail, run_at FROM task_runs
             WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind=?4 AND run_at >= ?5
             ORDER BY run_at",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, dom.write, task_kind, since],
            |r| {
                Ok(TaskRun {
                    id: r.get(0)?,
                    task_kind: r.get(1)?,
                    outcome: r.get(2)?,
                    signal_count: r.get(3)?,
                    detail: r.get(4)?,
                    run_at: r.get(5)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 提议一条修复行动（doc7/08 §4）。模型只能落 \`proposed\`，且不得自带授权。
    pub fn repair_action_propose(
        &mut self,
        scope: &ScopeKey,
        req: &ProposeAction<'_>,
    ) -> Result<String, StoreError> {
        if !PROPOSERS.contains(&req.proposed_by) {
            return Err(StoreError::StateConflict);
        }
        // 行动必须具体：空话不算行动，也不能没有期望行为。
        if req.action.trim().chars().count() < 4 || req.expected_behavior.trim().chars().count() < 4
        {
            return Err(StoreError::InvalidRepairAction);
        }
        let exists: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM repair_threads
             WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id, scope.user_id, req.thread_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(StoreError::ThreadNotFound);
        }
        let id = Uuid::now_v7().to_string();
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO repair_actions
               (id, tenant_id, user_id, thread_id, action, expected_behavior, conditions,
                counterexamples, status, proposed_by, authorized_by, authorized_at, close_reason,
                recurrence_count, source_memory_id, generator_version, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'proposed',?9,NULL,NULL,NULL,0,?10,?11,?12,?12)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                req.thread_id,
                req.action.trim(),
                req.expected_behavior.trim(),
                req.conditions,
                req.counterexamples,
                req.proposed_by,
                req.source_memory_id,
                req.generator_version,
                now
            ],
        )?;
        Ok(id)
    }

    pub fn repair_action_list(
        &self,
        scope: &ScopeKey,
        thread_id: Option<&str>,
    ) -> Result<Vec<RepairAction>, StoreError> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {ACTION_COLS} FROM repair_actions
             WHERE tenant_id=?1 AND user_id=?2 AND (?3 IS NULL OR thread_id=?3)
             ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, thread_id],
            map_action,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 激活：只有 proposed → active，且必须有授权人。
    pub fn repair_action_activate(
        &mut self,
        scope: &ScopeKey,
        action_id: &str,
        authorized_by: &str,
    ) -> Result<bool, StoreError> {
        if authorized_by.trim().is_empty() {
            return Err(StoreError::StateConflict);
        }
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE repair_actions SET status='active', authorized_by=?3, authorized_at=?4, updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND id=?5 AND status='proposed'",
            params![scope.tenant_id, scope.user_id, authorized_by, now, action_id],
        )?;
        Ok(n > 0)
    }

    /// 关闭：必须显式理由。**单纯没有新纠正不构成修复证据**，所以不接受空理由，
    /// 也不接受「久未复发」这类自动理由（doc7/08 §4）。
    pub fn repair_action_close(
        &mut self,
        scope: &ScopeKey,
        action_id: &str,
        close_reason: &str,
        authorized_by: &str,
    ) -> Result<bool, StoreError> {
        let reason = close_reason.trim();
        if reason.chars().count() < 2 || authorized_by.trim().is_empty() {
            return Err(StoreError::StateConflict);
        }
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE repair_actions SET status='done', close_reason=?3, authorized_by=?4,
                    authorized_at=?5, updated_at=?5
             WHERE tenant_id=?1 AND user_id=?2 AND id=?6 AND status='active'",
            params![
                scope.tenant_id,
                scope.user_id,
                reason,
                authorized_by,
                now,
                action_id
            ],
        )?;
        Ok(n > 0)
    }

    /// 复发观察（doc7/08 §4）：同一线程下 active 的 action 记一条 recurrence。
    /// 同一 rupture_id 重复记录幂等。
    pub fn repair_action_record_recurrence(
        &mut self,
        scope: &ScopeKey,
        thread_id: &str,
        rupture_id: &str,
        observed_at: &str,
    ) -> Result<usize, StoreError> {
        let actions: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT id FROM repair_actions
                 WHERE tenant_id=?1 AND user_id=?2 AND thread_id=?3 AND status='active'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, thread_id], |r| {
                r.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut recorded = 0usize;
        for action_id in actions {
            let already: i64 = self.conn().query_row(
                "SELECT COUNT(*) FROM repair_action_events
                 WHERE tenant_id=?1 AND user_id=?2 AND action_id=?3 AND kind='recurrence'
                   AND rupture_id=?4",
                params![scope.tenant_id, scope.user_id, action_id, rupture_id],
                |r| r.get(0),
            )?;
            if already > 0 {
                continue;
            }
            self.conn_mut().execute(
                "INSERT INTO repair_action_events
                   (id, tenant_id, user_id, action_id, kind, rupture_id, detail, observed_at)
                 VALUES (?1,?2,?3,?4,'recurrence',?5,'',?6)",
                params![
                    Uuid::now_v7().to_string(),
                    scope.tenant_id,
                    scope.user_id,
                    action_id,
                    rupture_id,
                    observed_at
                ],
            )?;
            self.conn_mut().execute(
                "UPDATE repair_actions SET recurrence_count = recurrence_count + 1, updated_at=?3
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?4",
                params![scope.tenant_id, scope.user_id, observed_at, action_id],
            )?;
            recorded += 1;
        }
        Ok(recorded)
    }

    /// 台账视图：让调用方在跑之前知道「该不该跑」（只读，不触发任何调用）。
    pub fn schedule_state(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<ScheduleState, StoreError> {
        let mut counts = std::collections::BTreeMap::new();
        for k in TASK_KINDS {
            counts.insert(k.to_string(), self.ledger_count(scope, dom, k)?);
        }
        let last_run: Option<String> = self
            .conn()
            .query_row(
                "SELECT MAX(run_at) FROM task_runs WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, dom.write],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(ScheduleState {
            domain_id: dom.write.clone(),
            ledger_counts: counts,
            last_run_at: last_run,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDue {
    pub due: bool,
    pub reason: &'static str,
    pub unprocessed: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DueReport {
    pub domain_id: String,
    pub now: String,
    pub upkeep: TaskDue,
    pub relationships: TaskDue,
    pub nightly: TaskDue,
    pub quiet: TaskDue,
}

fn parse_epoch(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp())
}

impl Store {
    /// 调度判定（doc7/08 §1、§2）：**信号计数由 Rust 查库算出，不由模型自报**。
    /// 只读，不触发任何调用，也不改任何状态。
    pub fn background_due(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        cfg: &memory_domain::schedule::ScheduleConfig,
    ) -> Result<DueReport, StoreError> {
        use memory_domain::schedule as sched;
        let now = now_rfc3339()?;
        let now_epoch = parse_epoch(&now).unwrap_or(0);

        // 未消化信号：user 事件里没有出现在 upkeep 账本中的那些。
        let unprocessed: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM evidence_events e
             WHERE e.tenant_id=?1 AND e.user_id=?2 AND e.role='user' AND e.source_kind='user'
               AND NOT EXISTS (SELECT 1 FROM processing_ledger l
                   WHERE l.tenant_id=e.tenant_id AND l.user_id=e.user_id
                     AND l.task_kind='upkeep' AND l.signal_ref=e.id)",
            params![scope.tenant_id, scope.user_id],
            |r| r.get(0),
        )?;

        let last_activity: Option<String> = self
            .conn()
            .query_row(
                "SELECT MAX(received_at) FROM evidence_events WHERE tenant_id=?1 AND user_id=?2",
                params![scope.tenant_id, scope.user_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let last_run = |task: &str| -> Result<Option<i64>, StoreError> {
            let ts: Option<String> = self
                .conn()
                .query_row(
                    "SELECT MAX(run_at) FROM task_runs
                     WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind=?4",
                    params![scope.tenant_id, scope.user_id, dom.write, task],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            Ok(ts.as_deref().and_then(parse_epoch))
        };

        let today = &now[..10.min(now.len())];
        let runs_today: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM task_runs
             WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind='quiet' AND run_at >= ?4",
            params![
                scope.tenant_id,
                scope.user_id,
                dom.write,
                format!("{today}T00:00:00Z")
            ],
            |r| r.get(0),
        )?;

        // relationships 的「变化」= 上次成功运行之后更新的实体数。
        let last_rel = last_run("relationships")?;
        let changed_entities: i64 = match last_rel {
            None => self.conn().query_row(
                "SELECT COUNT(*) FROM relationship_entities
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND status='active'",
                params![scope.tenant_id, scope.user_id, dom.write],
                |r| r.get(0),
            )?,
            Some(epoch) => {
                let since = chrono::DateTime::from_timestamp(epoch, 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| now.clone());
                self.conn().query_row(
                    "SELECT COUNT(*) FROM relationship_entities
                     WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND status='active'
                       AND updated_at > ?4",
                    params![scope.tenant_id, scope.user_id, dom.write, since],
                    |r| r.get(0),
                )?
            }
        };

        let last_nightly: Option<String> = self
            .conn()
            .query_row(
                "SELECT MAX(run_at) FROM task_runs
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind='nightly'",
                params![scope.tenant_id, scope.user_id, dom.write],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let last_nightly_date = last_nightly.as_deref().map(|t| &t[..10.min(t.len())]);

        let upkeep_last = last_run("upkeep")?;
        let upkeep_due = sched::upkeep_due(cfg, now_epoch, upkeep_last, unprocessed);
        let relationships_due =
            sched::relationships_due(cfg, now_epoch, last_rel, changed_entities);
        let quiet_runs_last = last_run("quiet")?;
        let last_activity_epoch = last_activity
            .as_deref()
            .and_then(parse_epoch)
            .unwrap_or(now_epoch);
        let quiet_due =
            sched::quiet_due(cfg, now_epoch, last_activity_epoch, unprocessed, runs_today);
        let nightly_due = sched::nightly_due(cfg, today, last_nightly_date);
        // 先算好文案，避免把 `now` 的借用带进结构体字面量（那里要 move `now`）。
        let nightly_reason: &'static str = if !cfg.nightly_enabled {
            "disabled"
        } else if midnight_already_run(today, last_nightly_date) {
            "already_ran_today"
        } else {
            "due"
        };
        let _ = quiet_runs_last;

        Ok(DueReport {
            domain_id: dom.write.clone(),
            now,
            upkeep: TaskDue {
                due: upkeep_due,
                reason: if !cfg.upkeep_enabled {
                    "disabled"
                } else if unprocessed <= 0 {
                    "no_new_signal"
                } else if !upkeep_due {
                    "interval_not_reached"
                } else {
                    "due"
                },
                unprocessed,
            },
            relationships: TaskDue {
                due: relationships_due,
                reason: if !cfg.relationships_enabled {
                    "disabled"
                } else if changed_entities <= 0 {
                    "no_new_signal"
                } else if !relationships_due {
                    "interval_not_reached"
                } else {
                    "due"
                },
                unprocessed: changed_entities,
            },
            nightly: TaskDue {
                due: nightly_due,
                reason: nightly_reason,
                unprocessed,
            },
            quiet: TaskDue {
                due: quiet_due,
                reason: if !cfg.quiet_enabled {
                    "disabled"
                } else if unprocessed <= 0 {
                    "no_new_signal"
                } else if runs_today >= cfg.quiet_max_per_day {
                    "daily_limit_reached"
                } else if !quiet_due {
                    "not_idle_enough"
                } else {
                    "due"
                },
                unprocessed,
            },
        })
    }
}

fn midnight_already_run(today: &str, last_date: Option<&str>) -> bool {
    matches!(last_date, Some(d) if d >= today)
}

#[derive(Debug, Clone, Serialize)]
pub struct ScheduleState {
    pub domain_id: String,
    pub ledger_counts: std::collections::BTreeMap<String, i64>,
    pub last_run_at: Option<String>,
}
