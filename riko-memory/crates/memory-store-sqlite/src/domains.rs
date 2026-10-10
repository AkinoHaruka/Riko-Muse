//! V2-S1 记忆域存储操作（doc7/04 §2—§4）。
//!
//! 身份 scope（tenant/user）保持不变；域是存储边界，由服务端从可信宿主会话绑定
//! 与已配置策略解析，绝不接受请求正文覆盖。域功能未启用时调用方恒传
//! DomainScope::user_main()，查询结果与 schema 14 行为一致。

use memory_domain::{ScopeKey, USER_MAIN_DOMAIN};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// 域名上限（doc7/04 §4）：1—64 个 ASCII [A-Za-z0-9_-]。
const DOMAIN_ID_MAX: usize = 64;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DomainRow {
    pub domain_id: String,
    pub kind: String,
    pub status: String,
    pub policy_version: i64,
    pub created_reason: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DomainBindingRow {
    pub host_id: String,
    pub session_id: String,
    pub domain_id: String,
    pub registered_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DomainGrantRow {
    pub id: String,
    pub reader_domain: String,
    pub granted_domain: String,
    pub granted_by: String,
    pub reason: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

/// 域名合法性（doc7/04 §4）：保留名与非法字符在这里统一拒绝，
/// 不让「看起来像域名的任意字符串」进入存储边界。
pub fn validate_domain_id(domain_id: &str) -> Result<(), StoreError> {
    if domain_id.is_empty() || domain_id.len() > DOMAIN_ID_MAX {
        return Err(StoreError::InvalidDomainId);
    }
    if !domain_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidDomainId);
    }
    if domain_id == USER_MAIN_DOMAIN {
        return Err(StoreError::DomainReserved);
    }
    Ok(())
}

fn map_domain_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DomainRow> {
    Ok(DomainRow {
        domain_id: r.get(0)?,
        kind: r.get(1)?,
        status: r.get(2)?,
        policy_version: r.get(3)?,
        created_reason: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

const DOMAIN_COLS: &str =
    "domain_id, kind, status, policy_version, created_reason, created_at, updated_at";

impl Store {
    /// 幂等确保 user_main 域注册行存在（迁移已回填旧库；新建 principal 时同事务写入）。
    pub fn domain_ensure_main(&mut self, scope: &ScopeKey) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO memory_domains
               (tenant_id, user_id, domain_id, kind, status, policy_version,
                created_reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'user_main', 'active', 1, 'principal_create', ?4, ?4)
             ON CONFLICT(tenant_id, user_id, domain_id) DO NOTHING",
            params![scope.tenant_id, scope.user_id, USER_MAIN_DOMAIN, now],
        )?;
        Ok(())
    }

    /// 列出本 scope 全部域（含已关闭），按 domain_id 排序。
    pub fn domain_list(&self, scope: &ScopeKey) -> Result<Vec<DomainRow>, StoreError> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {DOMAIN_COLS} FROM memory_domains
             WHERE tenant_id=?1 AND user_id=?2 ORDER BY domain_id"
        ))?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], map_domain_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn domain_get(
        &self,
        scope: &ScopeKey,
        domain_id: &str,
    ) -> Result<Option<DomainRow>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {DOMAIN_COLS} FROM memory_domains
                     WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3"
                ),
                params![scope.tenant_id, scope.user_id, domain_id],
                map_domain_row,
            )
            .optional()?)
    }

    /// 读取域必须存在且 active；否则 404/409（doc7/04 §2.3）。
    pub fn domain_require_active(
        &self,
        scope: &ScopeKey,
        domain_id: &str,
    ) -> Result<DomainRow, StoreError> {
        match self.domain_get(scope, domain_id)? {
            None => Err(StoreError::DomainNotFound),
            Some(row) if row.status != "active" => Err(StoreError::DomainClosed),
            Some(row) => Ok(row),
        }
    }

    /// 建 side 域。幂等：同名已存在直接返回既有行（created=false）。
    pub fn domain_create_side(
        &mut self,
        scope: &ScopeKey,
        domain_id: &str,
        reason: &str,
    ) -> Result<(DomainRow, bool), StoreError> {
        validate_domain_id(domain_id)?;
        if let Some(existing) = self.domain_get(scope, domain_id)? {
            return Ok((existing, false));
        }
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO memory_domains
               (tenant_id, user_id, domain_id, kind, status, policy_version,
                created_reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'side', 'active', 1, ?4, ?5, ?5)",
            params![scope.tenant_id, scope.user_id, domain_id, reason, now],
        )?;
        let row = self
            .domain_get(scope, domain_id)?
            .ok_or(StoreError::DomainNotFound)?;
        Ok((row, true))
    }

    /// 关闭域：数据保留，不可再作为读/写/绑定目标。user_main 不可关闭。
    /// 返回 true=本次关闭，false=已经是 closed（幂等）。
    pub fn domain_close(&mut self, scope: &ScopeKey, domain_id: &str) -> Result<bool, StoreError> {
        if domain_id == USER_MAIN_DOMAIN {
            return Err(StoreError::DomainReserved);
        }
        self.domain_require_active(scope, domain_id)?;
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE memory_domains SET status='closed', updated_at=?4
             WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND status='active'",
            params![scope.tenant_id, scope.user_id, domain_id, now],
        )?;
        Ok(n > 0)
    }

    /// 绑定查询（原始行，供管理端点列出）。
    pub fn domain_binding_list(
        &self,
        scope: &ScopeKey,
    ) -> Result<Vec<DomainBindingRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT host_id, session_id, domain_id, registered_by, created_at
             FROM session_domain_bindings WHERE tenant_id=?1 AND user_id=?2
             ORDER BY host_id, session_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
            Ok(DomainBindingRow {
                host_id: r.get(0)?,
                session_id: r.get(1)?,
                domain_id: r.get(2)?,
                registered_by: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 写域解析（doc7/04 §2.2）：绑定域，无绑定为 user_main。
    /// 绑定到已关闭域时拒绝（不能静默回落到主域）。
    pub fn resolve_write_domain(
        &self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
    ) -> Result<String, StoreError> {
        let bound: Option<String> = self
            .conn()
            .query_row(
                "SELECT domain_id FROM session_domain_bindings
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
                params![scope.tenant_id, scope.user_id, host_id, session_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(domain_id) = bound else {
            return Ok(USER_MAIN_DOMAIN.to_string());
        };
        self.domain_require_active(scope, &domain_id)?;
        Ok(domain_id)
    }

    /// 登记会话到域绑定。目标域必须存在且 active；已绑定到别的域即 409（不静默改绑）。
    /// 返回 true=新建，false=已存在且目标相同（幂等）。
    pub fn domain_binding_put(
        &mut self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
        domain_id: &str,
        registered_by: &str,
    ) -> Result<bool, StoreError> {
        self.domain_require_active(scope, domain_id)?;
        let existing: Option<String> = self
            .conn()
            .query_row(
                "SELECT domain_id FROM session_domain_bindings
                 WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
                params![scope.tenant_id, scope.user_id, host_id, session_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(d) if d == domain_id => Ok(false),
            Some(_) => Err(StoreError::DomainBindingConflict),
            None => {
                let now = now_rfc3339()?;
                self.conn_mut().execute(
                    "INSERT INTO session_domain_bindings
                       (tenant_id, user_id, host_id, session_id, domain_id, registered_by, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        scope.tenant_id,
                        scope.user_id,
                        host_id,
                        session_id,
                        domain_id,
                        registered_by,
                        now
                    ],
                )?;
                Ok(true)
            }
        }
    }

    /// 解除绑定；返回是否删除了行。解绑后该会话回到 user_main。
    pub fn domain_binding_delete(
        &mut self,
        scope: &ScopeKey,
        host_id: &str,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        let n = self.conn_mut().execute(
            "DELETE FROM session_domain_bindings
             WHERE tenant_id=?1 AND user_id=?2 AND host_id=?3 AND session_id=?4",
            params![scope.tenant_id, scope.user_id, host_id, session_id],
        )?;
        Ok(n > 0)
    }

    pub fn domain_grant_list(&self, scope: &ScopeKey) -> Result<Vec<DomainGrantRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, reader_domain, granted_domain, granted_by, reason, created_at, revoked_at
             FROM cross_domain_grants WHERE tenant_id=?1 AND user_id=?2
             ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
            Ok(DomainGrantRow {
                id: r.get(0)?,
                reader_domain: r.get(1)?,
                granted_domain: r.get(2)?,
                granted_by: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
                revoked_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 已生效的授权域列表（reader 域可额外读取的域）。
    pub fn domain_grants_for(
        &self,
        scope: &ScopeKey,
        reader_domain: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT granted_domain FROM cross_domain_grants
             WHERE tenant_id=?1 AND user_id=?2 AND reader_domain=?3 AND revoked_at IS NULL
             ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, reader_domain],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 建跨域授权。两端域都必须存在且 active；reader==granted 拒绝。
    /// 同 (reader, granted) 已有生效授权即幂等返回既有行。
    pub fn domain_grant_add(
        &mut self,
        scope: &ScopeKey,
        reader_domain: &str,
        granted_domain: &str,
        granted_by: &str,
        reason: &str,
    ) -> Result<(DomainGrantRow, bool), StoreError> {
        if reader_domain == granted_domain {
            return Err(StoreError::StateConflict);
        }
        self.domain_require_active(scope, reader_domain)?;
        self.domain_require_active(scope, granted_domain)?;
        let existing = self.domain_grant_list(scope)?.into_iter().find(|g| {
            g.revoked_at.is_none()
                && g.reader_domain == reader_domain
                && g.granted_domain == granted_domain
        });
        if let Some(row) = existing {
            return Ok((row, false));
        }
        let id = Uuid::now_v7().to_string();
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO cross_domain_grants
               (id, tenant_id, user_id, reader_domain, granted_domain, granted_by, reason, created_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                reader_domain,
                granted_domain,
                granted_by,
                reason,
                now
            ],
        )?;
        let row = self
            .domain_grant_list(scope)?
            .into_iter()
            .find(|g| g.id == id)
            .ok_or(StoreError::DomainNotFound)?;
        Ok((row, true))
    }

    /// 撤销授权（置 revoked_at，不删行，保留审计）；返回是否本次撤销。
    pub fn domain_grant_revoke(&mut self, scope: &ScopeKey, id: &str) -> Result<bool, StoreError> {
        let now = now_rfc3339()?;
        let n = self.conn_mut().execute(
            "UPDATE cross_domain_grants SET revoked_at=?3
             WHERE tenant_id=?1 AND user_id=?2 AND id=?4 AND revoked_at IS NULL",
            params![scope.tenant_id, scope.user_id, now, id],
        )?;
        Ok(n > 0)
    }

    /// L0 证据域映射（doc7/04 §1.1、§2.2）：无映射行按 user_main 处理。
    /// 已存在映射的重放不得改域（同事件绑定冲突明确拒绝）。
    pub fn evidence_domain_set(
        &mut self,
        scope: &ScopeKey,
        evidence_id: &str,
        domain_id: &str,
    ) -> Result<(), StoreError> {
        match self.evidence_domain_get(scope, evidence_id)? {
            Some(d) if d == domain_id => return Ok(()),
            Some(_) => return Err(StoreError::StateConflict),
            None => {}
        }
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO evidence_domain_map (tenant_id, user_id, evidence_id, domain_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![scope.tenant_id, scope.user_id, evidence_id, domain_id, now],
        )?;
        Ok(())
    }

    pub fn evidence_domain_get(
        &self,
        scope: &ScopeKey,
        evidence_id: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT domain_id FROM evidence_domain_map
                 WHERE tenant_id=?1 AND user_id=?2 AND evidence_id=?3",
                params![scope.tenant_id, scope.user_id, evidence_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 域管理审计（doc7/04 §4：action=domain_admin，detail_json 含操作、目标、操作者）。
    pub fn domain_audit(
        &mut self,
        scope: &ScopeKey,
        actor_kind: &str,
        actor_id: &str,
        target_id: &str,
        detail_json: serde_json::Value,
    ) -> Result<(), StoreError> {
        let now = now_rfc3339()?;
        self.conn_mut().execute(
            "INSERT INTO audit_events
               (id, tenant_id, user_id, actor_kind, actor_id, action, target_id, occurred_at, detail_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 'domain_admin', ?6, ?7, ?8)",
            params![
                Uuid::now_v7().to_string(),
                scope.tenant_id,
                scope.user_id,
                actor_kind,
                actor_id,
                target_id,
                now,
                detail_json.to_string()
            ],
        )?;
        Ok(())
    }
}
