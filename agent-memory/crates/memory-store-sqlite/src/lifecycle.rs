//! D6-9 生命周期治理（doc6/02 §7、doc6/12）：可逆退休（retire/restore）、
//! purge 两阶段执行闭包与墓碑、retention 策略与清理作业。
//!
//! retire 是可逆覆盖：不扩展 memories.status CHECK、不改 v1 /forget 语义；
//! 所有 read/query path 在排序前排除 retired（doc6/09 卡要求）。purge 只经
//! 可信 UI/CLI 两阶段 preview+confirm，不注册 Agent/Dream 工具。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// 退休覆盖请求（doc6/09 卡：API 须带最新真实用户事件 ID、指令 quote 与 span，
/// 由调用方（HTTP/CLI handler）先行核验后传入核验结论；存储层仍核 scope/版本）。
pub struct RetireRequest {
    pub expected_version: i64,
    pub actor_kind: &'static str,
    pub reason_code: Option<String>,
    pub user_evidence_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetireOverrideRow {
    pub memory_id: String,
    pub memory_version: i64,
    pub actor_kind: String,
    pub reason_code: Option<String>,
    pub retired_at: String,
}

impl Store {
    // ---- retire / restore ----

    /// 退休（可逆覆盖）：核 memory 存在且 active、版本 CAS；幂等（已退休同目标
    /// 返回 false=已覆盖）。成功后 best-effort 记 memory_audit L1 update。
    pub fn retire_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        req: &RetireRequest,
    ) -> Result<bool, StoreError> {
        let Some((_kind, _claim, version, status)) = self.memory_status_row(scope, memory_id)? else {
            return Err(StoreError::MemoryNotFound);
        };
        if status != "active" {
            return Err(StoreError::MemoryNotFound);
        }
        if version != req.expected_version {
            return Err(StoreError::VersionConflict);
        }
        let now = now_rfc3339()?;
        let tx = self.conn_mut().transaction()?;
        let n = tx.execute(
            "INSERT INTO memory_retirements
               (tenant_id, user_id, memory_id, memory_version, actor_kind, reason_code,
                user_evidence_id, retired_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT (tenant_id, user_id, memory_id) DO NOTHING",
            params![scope.tenant_id, scope.user_id, memory_id, version,
                    req.actor_kind, req.reason_code, req.user_evidence_id, now],
        )?;
        if n == 0 {
            return Ok(false); // 已退休：幂等确认
        }
        Self::record_l1_audit_tx(&tx, scope, memory_id, version, req.actor_kind)?;
        // 退休与向量失效不互斥：置 stale 立即从语义支路消失（读路径另有 get_memory 门）。
        Self::stale_vectors_in_tx(&tx, scope, "memory", memory_id)?;
        Self::mark_index_dirty(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    /// 恢复（同事务移除当前覆盖）：要求当前 active、未到期、至少一条有效 evidence；
    /// 旧 page/vector 不复活（页面来源失效为单向派生状态）。
    pub fn restore_memory(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        actor_kind: &'static str,
    ) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let row: Option<(i64, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT m.version, m.valid_until FROM memories m
                 WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3 AND m.status='active'
                   AND (m.valid_until IS NULL OR m.valid_until > ?4)",
                params![scope.tenant_id, scope.user_id, memory_id, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((version, _valid_until)) = row else {
            return Err(StoreError::MemoryNotFound); // 非 active/已到期/不存在
        };
        let n_ev: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM memory_evidence WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get::<_, i64>(0),
        )?;
        if n_ev == 0 {
            return Err(StoreError::MemoryNotFound); // 至少一条有效 evidence
        }
        let tx = self.conn_mut().transaction()?;
        let n = tx.execute(
            "DELETE FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id],
        )?;
        if n == 0 {
            return Ok(false); // 未退休：幂等确认
        }
        Self::record_l1_audit_tx(&tx, scope, memory_id, version, actor_kind)?;
        tx.commit()?;
        Ok(true)
    }

    /// 退休覆盖行（诊断）。
    pub fn retirement_get(&self, scope: &ScopeKey, memory_id: &str) -> Result<Option<RetireOverrideRow>, StoreError> {
        self.conn()
            .query_row(
                "SELECT memory_id, memory_version, actor_kind, reason_code, retired_at
                 FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| {
                    Ok(RetireOverrideRow {
                        memory_id: r.get(0)?,
                        memory_version: r.get(1)?,
                        actor_kind: r.get(2)?,
                        reason_code: r.get(3)?,
                        retired_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 退休 memory ID 集合（search/resident 组装后过滤用；有界 scope 内集合）。
    pub fn retired_ids(&self, scope: &ScopeKey) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT memory_id FROM memory_retirements WHERE tenant_id=?1 AND user_id=?2")?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| r.get(0))?;
        Ok(rows.collect::<Result<std::collections::HashSet<_>, _>>()?)
    }

    fn memory_status_row(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Option<(String, String, i64, String)>, StoreError> {
        self.conn()
            .query_row(
                "SELECT kind, claim, version, status FROM memories
                 WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    /// metadata-only L1 update 审计（doc6/14：不写正文/前后值；失败不回滚由调用方处理）。
    pub(crate) fn record_l1_audit_tx(
        tx: &rusqlite::Transaction<'_>,
        scope: &ScopeKey,
        memory_id: &str,
        version: i64,
        _actor_kind: &str,
    ) -> Result<(), StoreError> {
        let ms = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO memory_audit
               (audit_id, record_id, layer, action, tenant_id, user_id, agent_id, task_id,
                version, updated_at_ms, request_id)
             VALUES (?1,?2,'L1','update',?3,?4,NULL,NULL,?5,?6,NULL)",
            params![Uuid::now_v7().to_string(), memory_id, scope.tenant_id, scope.user_id, version, ms],
        )?;
        Ok(())
    }
}

pub(crate) fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}
