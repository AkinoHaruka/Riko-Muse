//! L0 证据接收：幂等、冲突检测、审计（doc/05 §1、doc/11 §3、doc/13 §2）。
//!
//! 证据不可变，只追加；同键同 hash 返回原 ID，同键异 hash 硬失败。

use chrono::{DateTime, Utc};
use memory_domain::{Origin, ScopeKey};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// 接收结果：Recorded=首次入库（201）；AlreadyRecorded=同键同 hash（200）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Recorded(String),
    AlreadyRecorded(String),
}

impl Store {
    /// 该会话已收到的最大事件序号（flush 校验用）。
    pub fn max_event_seq(
        &self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
    ) -> Result<Option<i64>, StoreError> {
        let v: Option<i64> = self
            .conn()
            .query_row(
                "SELECT MAX(event_seq) FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
                rusqlite::params![scope.tenant_id, scope.user_id, host_id, session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// 该会话最新 role=user 且 source_kind=user 的事件（remember/correct/forget 二次检查用）。
    pub fn latest_user_event(
        &self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
    ) -> Result<Option<(String, i64, String)>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, event_seq, content FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4
                   AND role='user' AND source_kind='user'
                 ORDER BY event_seq DESC LIMIT 1",
                rusqlite::params![scope.tenant_id, scope.user_id, host_id, session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// 读取单条证据（必须属于当前 scope）。
    pub fn get_evidence(
        &self,
        scope: &ScopeKey,
        evidence_id: &str,
    ) -> Result<Option<(String, String, String, String, String)>, StoreError> {
        // 返回 (host_id, session_id, role, source_kind, content)
        let row = self
            .conn()
            .query_row(
                "SELECT host_id, session_id, role, source_kind, content FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, evidence_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// doc/13 §2 幂等接收伪代码的实现。`occurred_at` 已由 handler 转 UTC。
    pub fn record_evidence(
        &mut self,
        scope: &ScopeKey,
        origin: &Origin,
        event_seq: i64,
        role: &str,
        source_kind: &str,
        occurred_at: &DateTime<Utc>,
        content: &str,
    ) -> Result<IngestOutcome, StoreError> {
        let content_hash = hex::encode(Sha256::digest(content.as_bytes()));
        let occurred = occurred_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let received = now_rfc3339()?;

        let tx = self.conn_mut().transaction()?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id, content_sha256 FROM evidence_events
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4 AND event_seq=?5",
                rusqlite::params![
                    scope.tenant_id,
                    scope.user_id,
                    origin.host_id,
                    origin.session_id,
                    event_seq
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((id, existing_hash)) = existing {
            if existing_hash == content_hash {
                return Ok(IngestOutcome::AlreadyRecorded(id));
            }
            return Err(StoreError::EventConflict);
        }

        let id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO evidence_events
             (id, tenant_id, user_id, host_id, agent_id, session_id, event_seq,
              role, source_kind, occurred_at, received_at, content, content_sha256)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                id,
                scope.tenant_id,
                scope.user_id,
                origin.host_id,
                origin.agent_id,
                origin.session_id,
                event_seq,
                role,
                source_kind,
                occurred,
                received,
                content,
                content_hash
            ],
        )?;
        // 审计不写正文（doc/09）。
        tx.execute(
            "INSERT INTO audit_events (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1,?2,?3,'system','memoryd','event_ingested',?4,?5,?6)",
            rusqlite::params![
                Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                id,
                received,
                format!(
                    "{{\"host_id\":\"{}\",\"session_id\":\"{}\",\"event_seq\":{}}}",
                    origin.host_id, origin.session_id, event_seq
                )
            ],
        )?;
        tx.commit()?;
        Ok(IngestOutcome::Recorded(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations")
    }

    fn setup() -> (Store, ScopeKey) {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-ev-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store
            .principal_add("t", "u", &dir.join("u.token"))
            .unwrap();
        let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        (store, scope)
    }

    fn origin() -> Origin {
        Origin {
            host_id: "dsh".into(),
            agent_id: "agent-a".into(),
            session_id: "s1".into(),
        }
    }

    #[test]
    fn replay_is_idempotent_and_conflict_detected() {
        let (mut store, scope) = setup();
        let t = chrono::Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap();
        let o = origin();
        let r1 = store
            .record_evidence(&scope, &o, 1, "user", "user", &t, "你好")
            .unwrap();
        let id1 = match r1 {
            IngestOutcome::Recorded(id) => id,
            _ => panic!("首次应 Recorded"),
        };
        let r2 = store
            .record_evidence(&scope, &o, 1, "user", "user", &t, "你好")
            .unwrap();
        assert_eq!(r2, IngestOutcome::AlreadyRecorded(id1));
        // 同键异文 → 冲突
        let err = store
            .record_evidence(&scope, &o, 1, "user", "user", &t, "篡改内容")
            .unwrap_err();
        assert!(matches!(err, StoreError::EventConflict));
        // 行数只有 1
        let n: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM evidence_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn latest_user_event_skips_plugin_and_assistant() {
        let (mut store, scope) = setup();
        let t = chrono::Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap();
        let o = origin();
        store.record_evidence(&scope, &o, 0, "user", "user", &t, "第一句").unwrap();
        store.record_evidence(&scope, &o, 1, "assistant", "assistant", &t, "助手回复").unwrap();
        // plugin 来源的 user 消息不算用户证据（doc/05 §1）
        store.record_evidence(&scope, &o, 2, "user", "plugin", &t, "注入的记忆块").unwrap();
        store.record_evidence(&scope, &o, 3, "user", "user", &t, "第二句").unwrap();
        let (id, seq, content) = store.latest_user_event(&scope, "dsh", "s1").unwrap().unwrap();
        assert_eq!(seq, 3);
        assert_eq!(content, "第二句");
        let _ = id;
    }
}
