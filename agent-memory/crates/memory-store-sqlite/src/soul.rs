//! Soul（用户编辑人格）存储与 metadata-only 审计（doc6/02 §2、doc6/03 §1、doc6/14）。
//!
//! D6-1 仅提供存储层方法；CLI/HTTP 出入口由 D6-2 接线。写路径单事务提交
//! profile + revision；已存在记录的更新在提交后 best-effort 追加一条 L3
//! `memory_audit`（doc6/14 §4：审计失败告警，不回滚业务写入——调用方负责
//! 告警，本层吞掉审计错误并保持业务结果）。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{now_rfc3339, Store, StoreError};

/// Soul 正文上限：2000 个 Unicode 标量字符（doc6/01 §4），超限拒绝不截断。
pub const SOUL_BODY_MAX_CHARS: usize = 2000;
/// agent_id 长度约束（doc6/02 §2）。
pub const SOUL_AGENT_ID_MAX_CHARS: usize = 256;
/// 允许的 actor_kind（doc6/02 §2）：无 model。
pub const SOUL_ACTOR_KINDS: [&str; 3] = ["user_cli", "user_api", "admin_cli"];

#[derive(Debug, Clone)]
pub struct SoulProfile {
    pub agent_id: String,
    pub body_md: String,
    pub version: i64,
    pub body_sha256: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct SoulRevision {
    pub version: i64,
    pub body_md: String,
    pub body_sha256: String,
    pub actor_kind: String,
    pub changed_at: String,
}

#[derive(Debug)]
pub enum SoulUpsertOutcome {
    /// 首次创建（expected_version 必须为 0）。新建不写 memory_audit（doc6/14 §2）。
    Created { version: i64 },
    /// 内容变更，版本 +1，新增 revision，best-effort 写 L3 update 审计。
    Updated { version: i64 },
    /// 相同正文且 expected_version 命中：幂等不增版本、不写 revision/审计。
    Unchanged { version: i64 },
}

/// memory_audit 的目标层（doc6/02 §2）：L1 原子记忆 / L2 派生文档 / L3 Soul。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditLayer {
    L1,
    L2,
    L3,
}

impl AuditLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditLayer::L1 => "L1",
            AuditLayer::L2 => "L2",
            AuditLayer::L3 => "L3",
        }
    }
}

/// memory_audit 的动作（doc6/14 §3）：只有 update/delete，无 create。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Update,
    Delete,
}

impl AuditAction {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Update => "update",
            AuditAction::Delete => "delete",
        }
    }
}

/// 一次已有记录修改的审计元数据（doc6/14 §3）。record_id 表示固定：
/// L1=memory_id、L2=page_id、L3=本 scope 的 agent_id。
#[derive(Debug, Clone)]
pub struct MemoryAuditEntry {
    pub record_id: String,
    pub layer: AuditLayer,
    pub action: AuditAction,
    pub agent_id: Option<String>,
    pub task_id: Option<String>,
    pub version: i64,
    pub updated_at_ms: i64,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MutationReceipt {
    pub request_sha256: String,
    pub result_status: String,
    pub response_json: String,
    pub created_at: String,
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// 幂等键规则（doc6/02 §2）：1—128 个 ASCII [A-Za-z0-9._-]。
fn validate_idempotency_key(key: &str) -> Result<(), StoreError> {
    let ok_len = !key.is_empty() && key.len() <= 128;
    let ok_chars = key.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
    });
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err(StoreError::InvalidIdempotencyKey)
    }
}

impl Store {
    /// 读取当前 Soul；不存在返回 None（HTTP 层表现为 version 0）。
    pub fn get_soul(&self, scope: &ScopeKey, agent_id: &str) -> Result<Option<SoulProfile>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT agent_id, body_md, version, body_sha256, created_at, updated_at
                 FROM soul_profiles WHERE tenant_id=?1 AND user_id=?2 AND agent_id=?3",
                params![scope.tenant_id, scope.user_id, agent_id],
                |r| {
                    Ok(SoulProfile {
                        agent_id: r.get(0)?,
                        body_md: r.get(1)?,
                        version: r.get(2)?,
                        body_sha256: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 创建或更新 Soul（CAS）。单事务写 profile + revision；既有记录更新提交后
    /// best-effort 追加 L3 update 审计（doc6/14 §4，失败不回滚）。
    pub fn upsert_soul(
        &mut self,
        scope: &ScopeKey,
        agent_id: &str,
        body_md: &str,
        expected_version: i64,
        actor_kind: &str,
        request_id: Option<&str>,
    ) -> Result<SoulUpsertOutcome, StoreError> {
        // 边界校验（doc6/02 §2：正文字符数在 Rust 校验，不用 SQLite length）。
        if agent_id.is_empty() || agent_id.chars().count() > SOUL_AGENT_ID_MAX_CHARS {
            return Err(StoreError::InvalidAgentId);
        }
        if body_md.chars().count() > SOUL_BODY_MAX_CHARS {
            return Err(StoreError::SoulBodyTooLong);
        }
        if !SOUL_ACTOR_KINDS.contains(&actor_kind) {
            // 非法 actor 由调用方 DTO 把关；存储层按约束拒绝，不猜降级。
            return Err(StoreError::VersionConflict);
        }
        let sha = sha256_hex(body_md);
        let now = now_rfc3339()?;

        let existing: Option<(i64, String)> = self
            .conn()
            .query_row(
                "SELECT version, body_sha256 FROM soul_profiles
                 WHERE tenant_id=?1 AND user_id=?2 AND agent_id=?3",
                params![scope.tenant_id, scope.user_id, agent_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        match existing {
            None => {
                if expected_version != 0 {
                    return Err(StoreError::VersionConflict);
                }
                let tx = self.conn_mut().transaction()?;
                tx.execute(
                    "INSERT INTO soul_profiles
                       (tenant_id, user_id, agent_id, body_md, version, body_sha256, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)",
                    params![scope.tenant_id, scope.user_id, agent_id, body_md, sha, now],
                )?;
                tx.execute(
                    "INSERT INTO soul_revisions
                       (tenant_id, user_id, agent_id, version, body_md, body_sha256, actor_kind, changed_at)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                    params![scope.tenant_id, scope.user_id, agent_id, body_md, sha, actor_kind, now],
                )?;
                tx.commit()?;
                // 新建不写审计（doc6/14 §2）。
                Ok(SoulUpsertOutcome::Created { version: 1 })
            }
            Some((current_version, current_sha)) => {
                if sha == current_sha && expected_version == current_version {
                    // 相同内容幂等不增（doc6/02 §2）。
                    return Ok(SoulUpsertOutcome::Unchanged { version: current_version });
                }
                if expected_version != current_version {
                    return Err(StoreError::VersionConflict);
                }
                let new_version = current_version + 1;
                let tx = self.conn_mut().transaction()?;
                tx.execute(
                    "UPDATE soul_profiles
                     SET body_md=?4, version=?5, body_sha256=?6, updated_at=?7
                     WHERE tenant_id=?1 AND user_id=?2 AND agent_id=?3",
                    params![scope.tenant_id, scope.user_id, agent_id, body_md, new_version, sha, now],
                )?;
                tx.execute(
                    "INSERT INTO soul_revisions
                       (tenant_id, user_id, agent_id, version, body_md, body_sha256, actor_kind, changed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![scope.tenant_id, scope.user_id, agent_id, new_version, body_md, sha, actor_kind, now],
                )?;
                tx.commit()?;
                // best-effort L3 audit（doc6/14 §4）：失败只丢审计行，业务写入已提交。
                let audit_result = self.record_memory_audit(
                    scope,
                    &MemoryAuditEntry {
                        record_id: agent_id.to_string(),
                        layer: AuditLayer::L3,
                        action: AuditAction::Update,
                        agent_id: Some(agent_id.to_string()),
                        task_id: None,
                        version: new_version,
                        updated_at_ms: chrono::Utc::now().timestamp_millis(),
                        request_id: request_id.map(str::to_string),
                    },
                );
                if audit_result.is_err() {
                    // 调用方（D6-2 HTTP 层）负责把审计失败转为可观察告警；
                    // 存储层按 doc6/14 不回滚、不改报业务结果。
                }
                Ok(SoulUpsertOutcome::Updated { version: new_version })
            }
        }
    }

    /// 列出 Soul 历史版本元数据（正文经 get_soul_revision 单独读取，doc6/06 §2）。
    pub fn list_soul_revisions(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
    ) -> Result<Vec<SoulRevision>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT version, body_sha256, actor_kind, changed_at
             FROM soul_revisions WHERE tenant_id=?1 AND user_id=?2 AND agent_id=?3 ORDER BY version",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, agent_id], |r| {
            Ok(SoulRevision {
                version: r.get(0)?,
                body_md: String::new(),
                body_sha256: r.get(1)?,
                actor_kind: r.get(2)?,
                changed_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 读取单个历史版本正文（不存在返回 None）。
    pub fn get_soul_revision(
        &self,
        scope: &ScopeKey,
        agent_id: &str,
        version: i64,
    ) -> Result<Option<SoulRevision>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT version, body_md, body_sha256, actor_kind, changed_at
                 FROM soul_revisions
                 WHERE tenant_id=?1 AND user_id=?2 AND agent_id=?3 AND version=?4",
                params![scope.tenant_id, scope.user_id, agent_id, version],
                |r| {
                    Ok(SoulRevision {
                        version: r.get(0)?,
                        body_md: r.get(1)?,
                        body_sha256: r.get(2)?,
                        actor_kind: r.get(3)?,
                        changed_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 追加一条 memory_audit 元数据行（doc6/14）。严格版：失败返回 Err，
    /// 由调用方决定 best-effort 语义。scope 恒来自认证派生，不接受模型传入。
    pub fn record_memory_audit(
        &mut self,
        scope: &ScopeKey,
        entry: &MemoryAuditEntry,
    ) -> Result<(), StoreError> {
        self.conn_mut().execute(
            "INSERT INTO memory_audit
               (audit_id, record_id, layer, action, tenant_id, user_id, agent_id, task_id,
                version, updated_at_ms, request_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                Uuid::now_v7().to_string(),
                entry.record_id,
                entry.layer.as_str(),
                entry.action.as_str(),
                scope.tenant_id,
                scope.user_id,
                entry.agent_id,
                entry.task_id,
                entry.version,
                entry.updated_at_ms,
                entry.request_id,
            ],
        )?;
        Ok(())
    }

    /// 按幂等键取回执（doc6/02 §2）。同键同请求由调用方重放原响应；
    /// 同键异请求在 save 时报 IDEMPOTENCY_CONFLICT。
    pub fn fetch_mutation_receipt(
        &self,
        scope: &ScopeKey,
        operation: &str,
        idempotency_key: &str,
    ) -> Result<Option<MutationReceipt>, StoreError> {
        validate_idempotency_key(idempotency_key)?;
        let row = self
            .conn()
            .query_row(
                "SELECT request_sha256, result_status, response_json, created_at
                 FROM mutation_receipts
                 WHERE tenant_id=?1 AND user_id=?2 AND operation=?3 AND idempotency_key=?4",
                params![scope.tenant_id, scope.user_id, operation, idempotency_key],
                |r| {
                    Ok(MutationReceipt {
                        request_sha256: r.get(0)?,
                        result_status: r.get(1)?,
                        response_json: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 保存回执（doc6/02 §2：与目标修改同事务提交——调用方在自己事务内调用
    /// 时应复用同一连接；本方法单语句原子）。同键同请求幂等 Ok；异请求冲突。
    pub fn save_mutation_receipt(
        &mut self,
        scope: &ScopeKey,
        operation: &str,
        idempotency_key: &str,
        request_sha256: &str,
        result_status: &str,
        response_json: &str,
    ) -> Result<(), StoreError> {
        validate_idempotency_key(idempotency_key)?;
        let existing: Option<String> = self
            .conn()
            .query_row(
                "SELECT request_sha256 FROM mutation_receipts
                 WHERE tenant_id=?1 AND user_id=?2 AND operation=?3 AND idempotency_key=?4",
                params![scope.tenant_id, scope.user_id, operation, idempotency_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(recorded) = existing {
            if recorded == request_sha256 {
                return Ok(());
            }
            return Err(StoreError::IdempotencyConflict);
        }
        self.conn_mut().execute(
            "INSERT INTO mutation_receipts
               (tenant_id, user_id, operation, idempotency_key, request_sha256, result_status, response_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                scope.tenant_id,
                scope.user_id,
                operation,
                idempotency_key,
                request_sha256,
                result_status,
                response_json,
                now_rfc3339()?,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations")
    }

    fn setup(tag: &str) -> (Store, ScopeKey) {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-soul-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add("t", "u", &dir.join("u.token")).unwrap();
        let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        (store, scope)
    }

    fn scope_of(store: &mut Store, tenant: &str, user: &str, tag: &str) -> ScopeKey {
        let dir = std::env::temp_dir().join(format!("am-soul-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        store.principal_add(tenant, user, &dir.join(format!("{tenant}-{user}.token"))).unwrap();
        let token = std::fs::read_to_string(dir.join(format!("{tenant}-{user}.token"))).unwrap();
        store.verify_token(token.trim()).unwrap().unwrap()
    }

    #[test]
    fn soul_upsert_create_update_cas_and_audit() {
        // doc6/02 §2 / doc6/14：CAS 版本链、同内容幂等不增、L3 update 审计。
        let (mut store, scope) = setup("upsert");
        let v1 = store.upsert_soul(&scope, "agent-a", "# 角色\n先给结论。", 0, "user_cli", None).unwrap();
        assert!(matches!(v1, SoulUpsertOutcome::Created { version: 1 }));
        // 同内容同版本：幂等不增版本、不新增 revision。
        let same = store
            .upsert_soul(&scope, "agent-a", "# 角色\n先给结论。", 1, "user_cli", None)
            .unwrap();
        assert!(matches!(same, SoulUpsertOutcome::Unchanged { version: 1 }));
        // CAS 冲突：旧版本更新被拒，原版保持。
        let conflict = store.upsert_soul(&scope, "agent-a", "v2", 0, "user_cli", None);
        assert!(matches!(conflict, Err(StoreError::VersionConflict)));
        assert_eq!(store.get_soul(&scope, "agent-a").unwrap().unwrap().version, 1);
        // 正常更新：版本 +1、revision 两条、L3 update 审计一条（record_id=agent_id）。
        let v2 = store
            .upsert_soul(&scope, "agent-a", "# 角色\n先给结论，再讲取舍。", 1, "user_api", Some("req-1"))
            .unwrap();
        assert!(matches!(v2, SoulUpsertOutcome::Updated { version: 2 }));
        let profile = store.get_soul(&scope, "agent-a").unwrap().unwrap();
        assert_eq!(profile.version, 2);
        assert_eq!(profile.body_sha256, sha256_hex("# 角色\n先给结论，再讲取舍。"));
        let revisions = store.list_soul_revisions(&scope, "agent-a").unwrap();
        assert_eq!(revisions.len(), 2);
        let rev2 = store.get_soul_revision(&scope, "agent-a", 2).unwrap().unwrap();
        assert_eq!(rev2.body_md, "# 角色\n先给结论，再讲取舍。");
        assert_eq!(rev2.actor_kind, "user_api");
        let (layer, action, version, request_id): (String, String, i64, Option<String>) = store
            .conn()
            .query_row(
                "SELECT layer, action, version, request_id FROM memory_audit
                 WHERE tenant_id='t' AND user_id='u' AND record_id='agent-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((layer.as_str(), action.as_str(), version), ("L3", "update", 2));
        assert_eq!(request_id.as_deref(), Some("req-1"));
        // expected>0 且无行：拒绝创建。
        assert!(matches!(
            store.upsert_soul(&scope, "agent-b", "x", 3, "user_cli", None),
            Err(StoreError::VersionConflict)
        ));
    }

    #[test]
    fn soul_validation_rejects_oversize_and_bad_agent_id() {
        // doc6/01 §4：2000 Unicode 标量上限，不截断；agent_id 1—256。
        let (mut store, scope) = setup("validate");
        let long_body = "好".repeat(SOUL_BODY_MAX_CHARS + 1);
        assert!(matches!(
            store.upsert_soul(&scope, "a", &long_body, 0, "user_cli", None),
            Err(StoreError::SoulBodyTooLong)
        ));
        // 恰好 2000：允许（多字节字符按标量计）。
        let exact = "好".repeat(SOUL_BODY_MAX_CHARS);
        assert!(matches!(
            store.upsert_soul(&scope, "a", &exact, 0, "user_cli", None),
            Ok(SoulUpsertOutcome::Created { .. })
        ));
        assert!(matches!(
            store.upsert_soul(&scope, "", "x", 0, "user_cli", None),
            Err(StoreError::InvalidAgentId)
        ));
        let long_agent = "a".repeat(SOUL_AGENT_ID_MAX_CHARS + 1);
        assert!(matches!(
            store.upsert_soul(&scope, &long_agent, "x", 0, "user_cli", None),
            Err(StoreError::InvalidAgentId)
        ));
    }

    #[test]
    fn soul_cross_scope_isolation() {
        // doc6/02 §8.7：跨用户 404 语义（存储层为 None / MemoryNotFound 类）。
        let (mut store, scope_a) = setup("iso-a");
        let scope_b = scope_of(&mut store, "t", "u2", "iso-b");
        store.upsert_soul(&scope_a, "agent-a", "Alice 的 soul", 0, "user_cli", None).unwrap();
        assert!(store.get_soul(&scope_b, "agent-a").unwrap().is_none());
        assert!(store.list_soul_revisions(&scope_b, "agent-a").unwrap().is_empty());
        assert!(store.get_soul_revision(&scope_b, "agent-a", 1).unwrap().is_none());
        // B 无法写 A 的 agent 键产生混淆——B 写同名键是 B 自己的新 profile。
        store.upsert_soul(&scope_b, "agent-a", "Bob 的 soul", 0, "user_cli", None).unwrap();
        assert_eq!(store.get_soul(&scope_a, "agent-a").unwrap().unwrap().body_md, "Alice 的 soul");
        assert_eq!(store.get_soul(&scope_b, "agent-a").unwrap().unwrap().body_md, "Bob 的 soul");
    }

    #[test]
    fn audit_failure_does_not_rollback_soul_update() {
        // doc6/14 §4：审计写失败告警但不回滚业务修改——删除审计表模拟持久故障。
        let (mut store, scope) = setup("audit-fail");
        store.upsert_soul(&scope, "agent-a", "v1", 0, "user_cli", None).unwrap();
        store.conn_mut().execute_batch("DROP TABLE memory_audit;").unwrap();
        let result = store.upsert_soul(&scope, "agent-a", "v2", 1, "user_cli", None).unwrap();
        assert!(matches!(result, SoulUpsertOutcome::Updated { version: 2 }), "业务写入必须成功");
        let profile = store.get_soul(&scope, "agent-a").unwrap().unwrap();
        assert_eq!(profile.version, 2);
        assert_eq!(profile.body_md, "v2");
        assert_eq!(store.list_soul_revisions(&scope, "agent-a").unwrap().len(), 2);
    }

    #[test]
    fn mutation_receipt_replay_and_conflict() {
        // doc6/02 §2：同键同请求重放、同键异请求 IDEMPOTENCY_CONFLICT、坏键拒绝。
        let (mut store, scope) = setup("receipt");
        store
            .save_mutation_receipt(&scope, "soul_import", "key-1", "hash-a", "200", "{}")
            .unwrap();
        // 同键同哈希：幂等 Ok。
        store
            .save_mutation_receipt(&scope, "soul_import", "key-1", "hash-a", "200", "{}")
            .unwrap();
        // 同键异哈希：冲突。
        assert!(matches!(
            store.save_mutation_receipt(&scope, "soul_import", "key-1", "hash-b", "200", "{}"),
            Err(StoreError::IdempotencyConflict)
        ));
        let receipt = store.fetch_mutation_receipt(&scope, "soul_import", "key-1").unwrap().unwrap();
        assert_eq!(receipt.request_sha256, "hash-a");
        // 跨 operation 同键互不干扰。
        assert!(store.fetch_mutation_receipt(&scope, "pin", "key-1").unwrap().is_none());
        // 坏键：空、超长、非法字符。
        assert!(matches!(
            store.save_mutation_receipt(&scope, "soul_import", "", "h", "200", "{}"),
            Err(StoreError::InvalidIdempotencyKey)
        ));
        assert!(matches!(
            store.save_mutation_receipt(&scope, "soul_import", &"k".repeat(129), "h", "200", "{}"),
            Err(StoreError::InvalidIdempotencyKey)
        ));
        assert!(matches!(
            store.save_mutation_receipt(&scope, "soul_import", "bad key!", "h", "200", "{}"),
            Err(StoreError::InvalidIdempotencyKey)
        ));
    }
}
