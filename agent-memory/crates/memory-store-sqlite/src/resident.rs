//! Resident 固定记忆存储（doc6/02 §2、doc6/03 §3/§4）。
//!
//! pin/unpin 是选择配置，不改 L1 状态；unpin 不删行（enabled=0 且 version+1），
//! 重新 pin 沿原行增 version（doc6/02 §2）。位置写入统一走「临时偏移 → 归一化
//! 0..n-1」路径（doc6/02 §2：若唯一检查妨碍交换，先临时偏移，不能半更新）。
//! D6-1 只提供存储与可见性初核（active + 有效期）；resident 选择算法与预算在
//! D6-3 扩展。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};

use crate::{now_rfc3339, Store, StoreError};

/// 临时偏移量：位置写入期间先把 enabled 行搬出正常区间 [0, REORDER_OFFSET)，
/// 规避 enabled position 部分唯一索引的中间态冲突。
const REORDER_OFFSET: i64 = 2_000_000;
/// 新 INSERT 行的带外临时位置：位于正常带与偏移带之间，独立于两者，
/// 归一化前短暂存在（同一连接串行执行，不会有两个行同时处于该位置）。
const INSERT_TEMP_POSITION: i64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct ResidentPinRow {
    pub memory_id: String,
    pub position: i64,
    pub pinned_at: String,
    pub version: i64,
}

#[derive(Debug)]
pub enum PinOutcome {
    /// 新建 pin 行或重新激活：版本 +1（新建为 1）。
    Pinned { version: i64, position: i64 },
    /// 已 enabled 且位置不变的重复请求：幂等，不增版本。
    Unchanged { version: i64, position: i64 },
}

#[derive(Debug)]
pub struct UnpinOutcome {
    pub version: i64,
    /// 行本就 enabled=0 或无 pin 行：幂等结果，版本不变（无行时为 0）。
    pub already_disabled: bool,
}

/// 可见 pin（doc6/03 §4 初核）：active、未过期；retired 覆盖过滤随 0010
/// （D6-9）加入，预算与选择算法归 D6-3。
#[derive(Debug, Clone)]
pub struct VisiblePin {
    pub memory_id: String,
    pub position: i64,
    pub version: i64,
    pub kind: String,
    pub claim: String,
}

impl Store {
    /// 固定一条记忆。跨 scope/不存在的 memory 一律 MemoryNotFound（不泄露存在性）。
    /// expected_pin_version 提供 CAS；position 为目标下标（越界钳制到末尾），
    /// 其余 enabled 行保持相对顺序。
    pub fn resident_pin(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        position: Option<i64>,
        expected_pin_version: Option<i64>,
    ) -> Result<PinOutcome, StoreError> {
        if matches!(position, Some(p) if p < 0) {
            // position >= 0 由 SQL CHECK 兜底；负数在入口给确定性错误。
            return Err(StoreError::StateConflict);
        }
        let now = now_rfc3339()?;
        let exists: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(StoreError::MemoryNotFound);
        }
        let existing: Option<(bool, i64, i64)> = self
            .conn()
            .query_row(
                "SELECT enabled, position, version FROM resident_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        match existing {
            None => {
                if expected_pin_version.is_some() {
                    return Err(StoreError::VersionConflict);
                }
                // 先以带外临时位置落行（独立于正常带与偏移带），再归一化。
                self.conn_mut().execute(
                    "INSERT INTO resident_pins
                       (tenant_id, user_id, memory_id, enabled, position, pinned_at, version)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, 1)",
                    params![scope.tenant_id, scope.user_id, memory_id, INSERT_TEMP_POSITION, now],
                )?;
                self.reposition_pin(scope, memory_id, position)?;
                let pos = self.pin_position_of(scope, memory_id)?.unwrap_or_default();
                Ok(PinOutcome::Pinned { version: 1, position: pos })
            }
            Some((enabled, current_position, current_version)) => {
                if let Some(expected) = expected_pin_version {
                    if expected != current_version {
                        return Err(StoreError::VersionConflict);
                    }
                }
                if enabled && (position.is_none() || position == Some(current_position)) {
                    return Ok(PinOutcome::Unchanged {
                        version: current_version,
                        position: current_position,
                    });
                }
                let new_version = current_version + 1;
                if enabled {
                    // 已 enabled：仅重排（版本 +1）。
                    self.reposition_pin(scope, memory_id, position)?;
                } else {
                    // 重新激活沿原行增版本；先带外入列再归一化，避免占用冲突位置。
                    self.conn_mut().execute(
                        "UPDATE resident_pins SET enabled=1, position=?4
                         WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                        params![scope.tenant_id, scope.user_id, memory_id, INSERT_TEMP_POSITION],
                    )?;
                    self.reposition_pin(scope, memory_id, position)?;
                }
                self.conn_mut().execute(
                    "UPDATE resident_pins SET version=?4, pinned_at=?5
                     WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                    params![scope.tenant_id, scope.user_id, memory_id, new_version, now],
                )?;
                let pos = self.pin_position_of(scope, memory_id)?.unwrap_or_default();
                Ok(PinOutcome::Pinned { version: new_version, position: pos })
            }
        }
    }

    /// 解除固定：置 enabled=0 并增版本；行不存在或已 disabled 返回幂等结果。
    /// 无 pin 行且记忆 ID 不属于当前 scope 时返回 MemoryNotFound（doc6/06 §2 404）。
    pub fn resident_unpin(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        expected_pin_version: Option<i64>,
    ) -> Result<UnpinOutcome, StoreError> {
        let row: Option<(bool, i64)> = self
            .conn()
            .query_row(
                "SELECT enabled, version FROM resident_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?)),
            )
            .optional()?;
        let (enabled, version) = match row {
            Some(v) => v,
            None => {
                let exists: bool = self
                    .conn()
                    .query_row(
                        "SELECT 1 FROM memories WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                        params![scope.tenant_id, scope.user_id, memory_id],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if !exists {
                    return Err(StoreError::MemoryNotFound);
                }
                return Ok(UnpinOutcome { version: 0, already_disabled: true });
            }
        };
        if !enabled {
            return Ok(UnpinOutcome { version, already_disabled: true });
        }
        if let Some(expected) = expected_pin_version {
            if expected != version {
                return Err(StoreError::VersionConflict);
            }
        }
        self.conn_mut().execute(
            "UPDATE resident_pins SET enabled=0, version=?4
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1],
        )?;
        Ok(UnpinOutcome { version: version + 1, already_disabled: false })
    }

    /// 重排：把一条 enabled pin 移到新下标（越界钳制到末尾），其余 enabled 行
    /// 保持相对顺序；成功后位置归一化为 0..n-1。目标行版本由调用方负责 +1。
    pub fn resident_move(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        new_position: i64,
        expected_pin_version: Option<i64>,
    ) -> Result<i64, StoreError> {
        if new_position < 0 {
            return Err(StoreError::StateConflict);
        }
        let (enabled, version): (bool, i64) = self
            .conn()
            .query_row(
                "SELECT enabled, version FROM resident_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::MemoryNotFound)?;
        if !enabled {
            return Err(StoreError::StateConflict);
        }
        if let Some(expected) = expected_pin_version {
            if expected != version {
                return Err(StoreError::VersionConflict);
            }
        }
        self.reposition_pin(scope, memory_id, Some(new_position))?;
        self.conn_mut().execute(
            "UPDATE resident_pins SET version=?4
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1],
        )?;
        Ok(version + 1)
    }

    /// 全部 enabled pin（按 position 升序），不含状态过滤（诊断/管理用）。
    pub fn resident_pins(&self, scope: &ScopeKey) -> Result<Vec<ResidentPinRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT memory_id, position, pinned_at, version FROM resident_pins
             WHERE tenant_id=?1 AND user_id=?2 AND enabled=1 ORDER BY position, memory_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
            Ok(ResidentPinRow {
                memory_id: r.get(0)?,
                position: r.get(1)?,
                pinned_at: r.get(2)?,
                version: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 当前对模型可见的 pin（doc6/03 §4 初核）：status='active' 且未过期；
    /// 已 forgotten/superseded/expired 的 pin 行保留作历史但不可见。
    pub fn resident_visible_pins(
        &self,
        scope: &ScopeKey,
        now: &str,
    ) -> Result<Vec<VisiblePin>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT p.memory_id, p.position, p.version, m.kind, m.claim
             FROM resident_pins p JOIN memories m
               ON m.tenant_id=p.tenant_id AND m.user_id=p.user_id AND m.id=p.memory_id
             WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.enabled=1
               AND m.status='active'
               AND (m.valid_until IS NULL OR m.valid_until > ?3)
             ORDER BY p.position, p.memory_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| {
            Ok(VisiblePin {
                memory_id: r.get(0)?,
                position: r.get(1)?,
                version: r.get(2)?,
                kind: r.get(3)?,
                claim: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn pin_position_of(&self, scope: &ScopeKey, memory_id: &str) -> Result<Option<i64>, StoreError> {
        let pos = self
            .conn()
            .query_row(
                "SELECT position FROM resident_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(pos)
    }

    /// 重排核心：目标移到目标下标（None = 末尾），随后整组归一化 0..n-1。
    /// 先把全部 enabled 行加临时偏移（彼此仍唯一），再写回最终位置——任何
    /// 中间态都不触碰部分唯一索引冲突；失败时调用方事务回滚（doc6/02 §2）。
    fn reposition_pin(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        desired: Option<i64>,
    ) -> Result<(), StoreError> {
        let current: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT memory_id FROM resident_pins
                 WHERE tenant_id=?1 AND user_id=?2 AND enabled=1 ORDER BY position, memory_id",
            )?;
            let rows =
                stmt.query_map(params![scope.tenant_id, scope.user_id], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if !current.iter().any(|id| id == memory_id) {
            return Err(StoreError::MemoryNotFound);
        }
        let mut ordered: Vec<String> =
            current.iter().filter(|id| id.as_str() != memory_id).cloned().collect();
        let target_index = match desired {
            None => ordered.len(),
            Some(p) => p.clamp(0, ordered.len() as i64) as usize,
        };
        ordered.insert(target_index, memory_id.to_string());
        self.conn_mut().execute(
            "UPDATE resident_pins SET position = position + ?3
             WHERE tenant_id=?1 AND user_id=?2 AND enabled=1",
            params![scope.tenant_id, scope.user_id, REORDER_OFFSET],
        )?;
        for (index, id) in ordered.iter().enumerate() {
            self.conn_mut().execute(
                "UPDATE resident_pins SET position=?4
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, id, index as i64],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IngestOutcome;
    use memory_domain::{MemoryKind, Origin};

    fn migrations_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("migrations")
    }

    fn setup(tag: &str) -> (Store, ScopeKey, Origin) {
        let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
        let dir = std::env::temp_dir().join(format!("am-res-test-{}-{tag}", std::process::id()));
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
        claim: &str,
    ) -> String {
        let t = chrono::Utc::now();
        let ev = match store
            .record_evidence(scope, origin, seq, "user", "user", &t, claim)
            .unwrap()
        {
            IngestOutcome::Recorded(id) => id,
            IngestOutcome::AlreadyRecorded(id) => id,
        };
        match store
            .remember(scope, origin, &ev, claim, MemoryKind::Fact)
            .unwrap()
        {
            crate::RememberOutcome::Created { memory_id, .. } => memory_id,
            crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    }

    #[test]
    fn pin_unpin_repin_flow() {
        // doc6/02 §2：unpin 不删行、版本递增、重复 unpin 幂等、重 pin 沿原行增版本。
        let (mut store, scope, origin) = setup("flow");
        let m1 = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州");
        match store.resident_pin(&scope, &m1, None, None).unwrap() {
            PinOutcome::Pinned { version: 1, position: 0 } => {}
            other => panic!("首次 pin 应为 v1/pos0：{other:?}"),
        }
        // 重复 pin 同位置：幂等不增版本。
        match store.resident_pin(&scope, &m1, None, None).unwrap() {
            PinOutcome::Unchanged { version: 1, position: 0 } => {}
            other => panic!("重复 pin 应幂等：{other:?}"),
        }
        // CAS 冲突。
        assert!(matches!(
            store.resident_pin(&scope, &m1, None, Some(99)),
            Err(StoreError::VersionConflict)
        ));
        // unpin：版本 2、行保留。
        let un = store.resident_unpin(&scope, &m1, Some(1)).unwrap();
        assert_eq!(un.version, 2);
        assert!(!un.already_disabled);
        assert_eq!(store.resident_pins(&scope).unwrap().len(), 0, "enabled=0 不出现在 pin 列表");
        // 重复 unpin（带或不带 CAS）：幂等。
        let un2 = store.resident_unpin(&scope, &m1, Some(2)).unwrap();
        assert!(un2.already_disabled && un2.version == 2);
        // 重新 pin：沿原行版本 3。
        match store.resident_pin(&scope, &m1, None, None).unwrap() {
            PinOutcome::Pinned { version: 3, position: 0 } => {}
            other => panic!("重 pin 应为 v3：{other:?}"),
        }
        // 首次 pin 不接受 expected_pin_version。
        let m2 = remember_one(&mut store, &scope, &origin, 2, "用户偏好先看结论");
        assert!(matches!(
            store.resident_pin(&scope, &m2, None, Some(1)),
            Err(StoreError::VersionConflict)
        ));
    }

    #[test]
    fn pin_cross_scope_and_missing_memory_rejected() {
        // doc6/02 §8.7：跨 scope/不存在 ID 一律 MemoryNotFound，不泄露存在性。
        let (mut store, scope, origin) = setup("iso");
        let other_scope = {
            let dir = std::env::temp_dir().join(format!("am-res-test-{}-iso-u2", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();
            let token = std::fs::read_to_string(dir.join("u2.token")).unwrap();
            store.verify_token(token.trim()).unwrap().unwrap()
        };
        let m_alice = remember_one(&mut store, &scope, &origin, 1, "Alice 的居住地");
        // u2 pin Alice 的 memory：404 语义。
        assert!(matches!(
            store.resident_pin(&other_scope, &m_alice, None, None),
            Err(StoreError::MemoryNotFound)
        ));
        assert!(matches!(
            store.resident_unpin(&other_scope, &m_alice, None),
            Err(StoreError::MemoryNotFound)
        ));
        assert!(matches!(
            store.resident_move(&other_scope, &m_alice, 0, None),
            Err(StoreError::MemoryNotFound)
        ));
        // 完全不存在的 ID 同样 404 语义。
        assert!(matches!(
            store.resident_pin(&scope, "no-such-id", None, None),
            Err(StoreError::MemoryNotFound)
        ));
    }

    #[test]
    fn pinned_forgotten_or_expired_memory_not_visible() {
        // doc6/03 §4：forgotten/expired 的 pin 行保留作历史，但不可见。
        let (mut store, scope, origin) = setup("visibility");
        let m_keep = remember_one(&mut store, &scope, &origin, 1, "长期有效的事实");
        let m_gone = remember_one(&mut store, &scope, &origin, 2, "将被遗忘的事实");
        store.resident_pin(&scope, &m_keep, None, None).unwrap();
        store.resident_pin(&scope, &m_gone, None, None).unwrap();
        // forget 语义由 memories.rs 独立测试覆盖；此处只验证「pin 行保留、不可见」，
        // 直接按 v1 契约置 forgotten（保留 evidence/revision）。
        store
            .conn()
            .execute(
                "UPDATE memories SET status='forgotten' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, m_gone],
            )
            .unwrap();
        // 过期 m_keep 的 valid_until 置为过去。
        store
            .conn()
            .execute(
                "UPDATE memories SET valid_until='2020-01-01T00:00:00Z' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, m_keep],
            )
            .unwrap();
        let now = now_rfc3339().unwrap();
        let visible = store.resident_visible_pins(&scope, &now).unwrap();
        assert!(visible.is_empty(), "forgotten 与过期 pin 均不可见：{visible:?}");
        // pin 行保留作历史。
        assert_eq!(store.resident_pins(&scope).unwrap().len(), 2);
        // 恢复有效期后 active pin 重新可见。
        store
            .conn()
            .execute(
                "UPDATE memories SET valid_until=NULL WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, m_keep],
            )
            .unwrap();
        let visible2 = store.resident_visible_pins(&scope, &now).unwrap();
        assert_eq!(visible2.len(), 1);
        assert_eq!(visible2[0].memory_id, m_keep);
    }

    #[test]
    fn reorder_is_atomic_and_unique() {
        // doc6/02 §2：重排同事务完成，无半更新；enabled position 全程唯一。
        let (mut store, scope, origin) = setup("reorder");
        let m1 = remember_one(&mut store, &scope, &origin, 1, "事实一");
        let m2 = remember_one(&mut store, &scope, &origin, 2, "事实二");
        let m3 = remember_one(&mut store, &scope, &origin, 3, "事实三");
        store.resident_pin(&scope, &m1, None, None).unwrap();
        store.resident_pin(&scope, &m2, None, None).unwrap();
        store.resident_pin(&scope, &m3, None, None).unwrap();
        let order = |store: &Store| -> Vec<String> {
            store.resident_pins(&scope).unwrap().into_iter().map(|p| p.memory_id).collect()
        };
        assert_eq!(order(&store), vec![m1.clone(), m2.clone(), m3.clone()]);
        // m3 移到首位：版本 +1，顺序 [m3,m1,m2]。
        let v = store.resident_move(&scope, &m3, 0, Some(1)).unwrap();
        assert_eq!(v, 2);
        assert_eq!(order(&store), vec![m3.clone(), m1.clone(), m2.clone()]);
        // 越界钳制到末尾：m3 移到 99 → [m1,m2,m3]。
        store.resident_move(&scope, &m3, 99, Some(2)).unwrap();
        assert_eq!(order(&store), vec![m1.clone(), m2.clone(), m3.clone()]);
        // 位置归一化 0..n-1，无临时偏移残留。
        let positions: Vec<i64> =
            store.resident_pins(&scope).unwrap().into_iter().map(|p| p.position).collect();
        assert_eq!(positions, vec![0, 1, 2]);
        // 错误 CAS 拒绝且顺序不变。
        assert!(matches!(
            store.resident_move(&scope, &m1, 0, Some(99)),
            Err(StoreError::VersionConflict)
        ));
        assert_eq!(order(&store), vec![m1.clone(), m2.clone(), m3.clone()]);
        // disabled 行不可 move。
        store.resident_unpin(&scope, &m3, None).unwrap();
        assert!(matches!(
            store.resident_move(&scope, &m3, 0, None),
            Err(StoreError::StateConflict)
        ));
        // 重新 pin 到占用位置（显式 position 0）：通过临时偏移路径，不冲突。
        store.resident_pin(&scope, &m3, Some(0), None).unwrap();
        assert_eq!(order(&store), vec![m3.clone(), m1.clone(), m2.clone()]);
        let positions2: Vec<i64> =
            store.resident_pins(&scope).unwrap().into_iter().map(|p| p.position).collect();
        assert_eq!(positions2, vec![0, 1, 2]);
    }
}
