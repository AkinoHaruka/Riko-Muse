//! Resident 固定记忆存储（doc6/02 §2、doc6/03 §3/§4）。
//!
//! pin/unpin 是选择配置，不改 L1 状态；unpin 不删行（enabled=0 且 version+1），
//! 重新 pin 沿原行增 version（doc6/02 §2）。位置写入统一走「临时偏移 → 归一化
//! 0..n-1」路径（doc6/02 §2：若唯一检查妨碍交换，先临时偏移，不能半更新），
//! 且每个变更操作整体包在一个 SQLite 事务内（doc6/02 §8.5 重排原子性）。
//! 回执（receipt）与业务修改同事务提交（doc6/02 §2）。D6-1/D6-2 提供存储与
//! 可见性初核；resident 选择算法、预算与 suggestions 在 D6-3。

use memory_domain::ScopeKey;
use rusqlite::{params, OptionalExtension};

use crate::{now_rfc3339, Store, StoreError};

/// 偏移带起点：位置写入期间先把 enabled 行搬出正常区间 [0, REORDER_OFFSET)，
/// 规避 enabled position 部分唯一索引的中间态冲突。
const REORDER_OFFSET: i64 = 2_000_000;
/// 新增/重激活行的带外临时位置：位于正常带与偏移带之间，独立于两者，
/// 归一化前短暂存在（同一连接串行执行，不会有两行同时处于该位置）。
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

/// CLI 列表行（doc6/03 §5：ID、顺序、当前可用/省略原因）。
#[derive(Debug, Clone)]
pub struct ResidentPinStatusRow {
    pub memory_id: String,
    pub position: i64,
    pub version: i64,
    pub memory_status: String,
    pub visible: bool,
    /// 不可见原因：STATUS_FORGOTTEN / STATUS_SUPERSEDED / STATUS_EXPIRED；可见为空。
    pub reason: &'static str,
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
    /// 其余 enabled 行保持相对顺序。`receipt` 提供时与变更同事务提交
    /// （operation="resident_pin"）。
    pub fn resident_pin(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        position: Option<i64>,
        expected_pin_version: Option<i64>,
        receipt: Option<(&str, &str)>,
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
                let tx = self.conn_mut().transaction()?;
                // 先以带外临时位置落行（独立于正常带与偏移带），再归一化。
                tx.execute(
                    "INSERT INTO resident_pins
                       (tenant_id, user_id, memory_id, enabled, position, pinned_at, version)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, 1)",
                    params![scope.tenant_id, scope.user_id, memory_id, INSERT_TEMP_POSITION, now],
                )?;
                reposition_in_tx(&tx, scope, memory_id, position)?;
                if let Some((key, hash)) = receipt {
                    crate::soul::insert_receipt_tx(
                        &tx,
                        scope,
                        "resident_pin",
                        key,
                        hash,
                        "pinned",
                        r#"{"status":"pinned","pin_version":1}"#,
                    )?;
                }
                tx.commit()?;
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
                let tx = self.conn_mut().transaction()?;
                if enabled {
                    // 已 enabled：仅重排（版本 +1）。
                    reposition_in_tx(&tx, scope, memory_id, position)?;
                } else {
                    // 重新激活沿原行增版本；先带外入列再归一化，避免占用冲突位置。
                    tx.execute(
                        "UPDATE resident_pins SET enabled=1, position=?4
                         WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                        params![scope.tenant_id, scope.user_id, memory_id, INSERT_TEMP_POSITION],
                    )?;
                    reposition_in_tx(&tx, scope, memory_id, position)?;
                }
                tx.execute(
                    "UPDATE resident_pins SET version=?4, pinned_at=?5
                     WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                    params![scope.tenant_id, scope.user_id, memory_id, new_version, now],
                )?;
                if let Some((key, hash)) = receipt {
                    crate::soul::insert_receipt_tx(
                        &tx,
                        scope,
                        "resident_pin",
                        key,
                        hash,
                        "pinned",
                        &format!(r#"{{"status":"pinned","pin_version":{new_version}}}"#),
                    )?;
                }
                tx.commit()?;
                let pos = self.pin_position_of(scope, memory_id)?.unwrap_or_default();
                Ok(PinOutcome::Pinned { version: new_version, position: pos })
            }
        }
    }

    /// 解除固定：置 enabled=0 并增版本；行不存在或已 disabled 返回幂等结果。
    /// 无 pin 行且记忆 ID 不属于当前 scope 时返回 MemoryNotFound（doc6/06 §2 404）。
    /// `receipt` 提供时与变更同事务提交（operation="resident_unpin"）。
    pub fn resident_unpin(
        &mut self,
        scope: &ScopeKey,
        memory_id: &str,
        expected_pin_version: Option<i64>,
        receipt: Option<(&str, &str)>,
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
                // 无 pin 行：幂等已解除（同 scope 记忆）；HTTP 层可记回执。
                if let Some((key, hash)) = receipt {
                    self.save_mutation_receipt(
                        scope,
                        "resident_unpin",
                        key,
                        hash,
                        "already_disabled",
                        r#"{"status":"already_disabled","pin_version":0}"#,
                    )?;
                }
                return Ok(UnpinOutcome { version: 0, already_disabled: true });
            }
        };
        if !enabled {
            if let Some((key, hash)) = receipt {
                self.save_mutation_receipt(
                    scope,
                    "resident_unpin",
                    key,
                    hash,
                    "already_disabled",
                    &format!(r#"{{"status":"already_disabled","pin_version":{version}}}"#),
                )?;
            }
            return Ok(UnpinOutcome { version, already_disabled: true });
        }
        if let Some(expected) = expected_pin_version {
            if expected != version {
                return Err(StoreError::VersionConflict);
            }
        }
        let tx = self.conn_mut().transaction()?;
        tx.execute(
            "UPDATE resident_pins SET enabled=0, version=?4
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1],
        )?;
        if let Some((key, hash)) = receipt {
            crate::soul::insert_receipt_tx(
                &tx,
                scope,
                "resident_unpin",
                key,
                hash,
                "unpinned",
                &format!(r#"{{"status":"unpinned","pin_version":{}}}"#, version + 1),
            )?;
        }
        tx.commit()?;
        Ok(UnpinOutcome { version: version + 1, already_disabled: false })
    }

    /// 重排：把一条 enabled pin 移到新下标（越界钳制到末尾），其余 enabled 行
    /// 保持相对顺序；reposition 与版本更新在同一事务内（doc6/02 §8.5）。
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
        let tx = self.conn_mut().transaction()?;
        reposition_in_tx(&tx, scope, memory_id, Some(new_position))?;
        tx.execute(
            "UPDATE resident_pins SET version=?4
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, memory_id, version + 1],
        )?;
        tx.commit()?;
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

    /// CLI 列表：enabled pin + 当前可见性与原因（doc6/03 §5）。
    /// 预算省略（ITEM_LIMIT/CHAR_LIMIT）与 conflict_ids 归 D6-3 选择函数。
    pub fn resident_pins_with_status(
        &self,
        scope: &ScopeKey,
        now: &str,
    ) -> Result<Vec<ResidentPinStatusRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT p.memory_id, p.position, p.version, m.status,
                    (m.status='active' AND (m.valid_until IS NULL OR m.valid_until > ?3))
             FROM resident_pins p JOIN memories m
               ON m.tenant_id=p.tenant_id AND m.user_id=p.user_id AND m.id=p.memory_id
             WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.enabled=1
             ORDER BY p.position, p.memory_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| {
            let memory_status: String = r.get(3)?;
            let visible: bool = r.get(4)?;
            let reason = if visible {
                ""
            } else if memory_status == "forgotten" {
                "STATUS_FORGOTTEN"
            } else if memory_status == "superseded" {
                "STATUS_SUPERSEDED"
            } else {
                "STATUS_EXPIRED"
            };
            Ok(ResidentPinStatusRow {
                memory_id: r.get(0)?,
                position: r.get(1)?,
                version: r.get(2)?,
                memory_status,
                visible,
                reason,
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
}

/// 重排核心：目标移到目标下标（None = 末尾），随后整组归一化 0..n-1。
/// 先把全部 enabled 行加临时偏移（彼此仍唯一），再写回最终位置——任何
/// 中间态都不触碰部分唯一索引冲突；调用方负责在事务内执行并在失败时回滚。
fn reposition_in_tx(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    memory_id: &str,
    desired: Option<i64>,
) -> Result<(), StoreError> {
    let current: Vec<String> = {
        let mut stmt = tx.prepare(
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
    tx.execute(
        "UPDATE resident_pins SET position = position + ?3
         WHERE tenant_id=?1 AND user_id=?2 AND enabled=1",
        params![scope.tenant_id, scope.user_id, REORDER_OFFSET],
    )?;
    for (index, id) in ordered.iter().enumerate() {
        tx.execute(
            "UPDATE resident_pins SET position=?4
             WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
            params![scope.tenant_id, scope.user_id, id, index as i64],
        )?;
    }
    Ok(())
}

// ---- D6-3：resident 选择函数 v1（doc6/03 §3，Rust 纯查询函数）----

/// resident 正文条目：携带 ID/kind/来源，不把裸记忆正文升级为 system 指令。
#[derive(Debug, Clone)]
pub struct ResidentItem {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    /// 选择原因：pinned | resident_auto（未 pin 的 active instruction）。
    pub reason: &'static str,
    pub version: i64,
    pub evidence_ids: Vec<String>,
}

/// 每轮重算的 resident 选择（doc6/03 §3/§4）：
/// pinned 优先（position 升序），随后未 pin 的 active instruction
/// （updated_at DESC, id ASC，无固定条数特权）；未 pin 的
/// fact/preference/episode 不自动常驻。预算逐条完整计数，不截断半句；
/// 已知 contradicts 的候选对整体退出正文并进 conflict_ids。
#[derive(Debug, Default)]
pub struct ResidentSelection {
    pub text: String,
    pub items: Vec<ResidentItem>,
    /// (memory_id, reason)：ITEM_LIMIT / CHAR_LIMIT（预算省略可见，doc6/03 §3）。
    pub omitted: Vec<(String, &'static str)>,
    /// 已知 contradicts 且两端均进入候选的 ID（不选赢家，两条都不进正文）。
    pub conflict_ids: Vec<String>,
    /// pin 指向非 active 记忆（correct 产生新 ID 不自动迁移；doc6/03 §4）。
    pub needs_review: Vec<String>,
    /// pin 的派生文档来源失效（STALE_SOURCE 省略原因；doc6/03 §4）。
    pub stale_pages: Vec<String>,
    pub truncated: bool,
}

/// suggestions 候选（doc6/03 §3）：可供 pin 的 active fact/preference；
/// 建议列表不改变注入。
#[derive(Debug, Clone)]
pub struct SuggestionRow {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub updated_at: String,
    pub evidence_ids: Vec<String>,
}

/// 渲染一条 resident 条目（doc6/03 §2 视图格式的注入形态）。
fn render_entry(item: &ResidentItem) -> String {
    if item.kind == "page" {
        format!("- [page: {}] {}", item.memory_id, item.claim)
    } else {
        format!("- [memory: {}] {}", item.memory_id, item.claim)
    }
}

impl Store {
    /// resident 选择 v1。同一连接读事务语义（SQLite 单连接顺序读）；
    /// retired 覆盖过滤随 0010（D6-9）加入。
    pub fn select_resident(
        &self,
        scope: &ScopeKey,
        now: &str,
        max_items: usize,
        max_chars: usize,
    ) -> Result<ResidentSelection, StoreError> {
        // 1. enabled pin JOIN active 未过期记忆（position 升序）。
        let mut items: Vec<ResidentItem> = Vec::new();
        let mut candidate_ids: Vec<String> = Vec::new();
        let mut needs_review: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn().prepare(
                "SELECT p.memory_id, m.kind, m.claim, m.version, m.status
                 FROM resident_pins p JOIN memories m
                   ON m.tenant_id=p.tenant_id AND m.user_id=p.user_id AND m.id=p.memory_id
                 WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.enabled=1
                 ORDER BY p.position, p.memory_id",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (memory_id, kind, claim, version, status) = row?;
                if status != "active" {
                    // forgotten/superseded/expired：pin 行保留作历史，但不可见；
                    // correct 后旧 pin 不自动迁移，报 needs_review 供用户决策。
                    needs_review.push(memory_id);
                    continue;
                }
                candidate_ids.push(memory_id.clone());
                items.push(ResidentItem {
                    memory_id,
                    kind,
                    claim,
                    reason: "pinned",
                    version,
                    evidence_ids: Vec::new(),
                });
            }
        }
        // 1b. 用户 pin 的已发布派生文档（doc6/03 §3 第 2 层）：按 position ASC；
        // 全部来源仍有效才进正文（get_page 读时复核）；失效 → stale_pages
        // （doc6/03 §4：来源失效时 resident 立即省略并给原因 STALE_SOURCE）。
        let mut stale_pages: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn().prepare(
                "SELECT pp.page_id FROM resident_page_pins pp
                 JOIN memory_pages pg ON pg.tenant_id=pp.tenant_id AND pg.user_id=pp.user_id
                    AND pg.id=pp.page_id
                 WHERE pp.tenant_id=?1 AND pp.user_id=?2 AND pp.enabled=1
                 ORDER BY pp.position, pp.page_id",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| r.get::<_, String>(0))?;
            let pinned_page_ids: Vec<String> = rows.collect::<Result<Vec<_>, _>>()?;
            for page_id in pinned_page_ids {
                match self.get_page(scope, &page_id, now)? {
                    Some(p) => {
                        candidate_ids.push(page_id.clone());
                        items.push(ResidentItem {
                            memory_id: page_id,
                            kind: "page".into(),
                            claim: p.title,
                            reason: "pinned_page",
                            version: p.version,
                            evidence_ids: Vec::new(),
                        });
                    }
                    None => {
                        stale_pages.push(page_id);
                    }
                }
            }
        }
        // 2. 未 pin 的 active instruction（updated_at DESC, id ASC；无固定条数特权）。
        {
            let mut stmt = self.conn().prepare(
                "SELECT m.id, m.kind, m.claim, m.version FROM memories m
                 WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.status='active'
                   AND m.kind='instruction'
                   AND (m.valid_until IS NULL OR m.valid_until > ?3)
                   AND NOT EXISTS (
                     SELECT 1 FROM resident_pins p
                     WHERE p.tenant_id=m.tenant_id AND p.user_id=m.user_id
                       AND p.memory_id=m.id AND p.enabled=1
                   )
                 ORDER BY m.updated_at DESC, m.id ASC LIMIT 200",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, now], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (memory_id, kind, claim, version) = row?;
                candidate_ids.push(memory_id.clone());
                items.push(ResidentItem {
                    memory_id,
                    kind,
                    claim,
                    reason: "resident_auto",
                    version,
                    evidence_ids: Vec::new(),
                });
            }
        }
        // 3. 已知 contradicts：两端均在本轮候选中 → 全部退出正文（doc6/03 §3）。
        let mut conflict_ids: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn().prepare(
                "SELECT from_memory_id, to_memory_id FROM memory_relations
                 WHERE tenant_id=?1 AND user_id=?2 AND kind='contradicts'",
            )?;
            let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (from, to) = row?;
                if candidate_ids.contains(&from) && candidate_ids.contains(&to) {
                    if !conflict_ids.contains(&from) {
                        conflict_ids.push(from);
                    }
                    if !conflict_ids.contains(&to) {
                        conflict_ids.push(to);
                    }
                }
            }
        }
        // 4. 预算装配：逐条完整渲染计数；条数满/字符超 → omitted，不截断半句。
        let mut selection = ResidentSelection {
            conflict_ids,
            needs_review,
            stale_pages,
            ..Default::default()
        };
        let mut used_chars = 0usize;
        for item in items {
            if selection.conflict_ids.contains(&item.memory_id) {
                continue; // 冲突对整体退出正文，单独诊断，不算 omitted。
            }
            if selection.items.len() >= max_items {
                selection.omitted.push((item.memory_id, "ITEM_LIMIT"));
                continue;
            }
            let evidence_ids = self
                .evidence_refs_of(scope, &item.memory_id)?
                .into_iter()
                .map(|(id, _, _)| id)
                .collect();
            let entry = ResidentItem { evidence_ids, ..item };
            let entry_chars = render_entry(&entry).chars().count();
            if used_chars > 0 && used_chars + entry_chars + 1 > max_chars {
                // 字符超预算：跳过该条，继续看后续更短条（doc6/03 §3）。
                selection.omitted.push((entry.memory_id, "CHAR_LIMIT"));
                continue;
            }
            used_chars += entry_chars + if selection.items.is_empty() { 0 } else { 1 };
            selection.items.push(entry);
        }
        selection.text =
            selection.items.iter().map(render_entry).collect::<Vec<_>>().join("\n");
        selection.truncated = !selection.omitted.is_empty();
        Ok(selection)
    }

    /// resident suggestions（doc6/03 §3）：active fact/preference 按
    /// updated_at DESC, id ASC 有界列表，排除已 enabled pin；不改变注入。
    pub fn resident_suggestions(
        &self,
        scope: &ScopeKey,
        now: &str,
        limit: usize,
    ) -> Result<Vec<SuggestionRow>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT m.id, m.kind, m.claim, m.updated_at FROM memories m
             WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.status='active'
               AND m.kind IN ('fact','preference')
               AND (m.valid_until IS NULL OR m.valid_until > ?3)
               AND NOT EXISTS (
                 SELECT 1 FROM resident_pins p
                 WHERE p.tenant_id=m.tenant_id AND p.user_id=m.user_id
                   AND p.memory_id=m.id AND p.enabled=1
               )
             ORDER BY m.updated_at DESC, m.id ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, now, limit as i64],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (memory_id, kind, claim, updated_at) = row?;
            let evidence_ids = self
                .evidence_refs_of(scope, &memory_id)?
                .into_iter()
                .map(|(id, _, _)| id)
                .collect();
            out.push(SuggestionRow { memory_id, kind, claim, updated_at, evidence_ids });
        }
        Ok(out)
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
        match store.remember(scope, origin, &ev, claim, MemoryKind::Fact).unwrap() {
            crate::RememberOutcome::Created { memory_id, .. } => memory_id,
            crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    }

    #[test]
    fn pin_unpin_repin_flow() {
        // doc6/02 §2：unpin 不删行、版本递增、重复 unpin 幂等、重 pin 沿原行增版本。
        let (mut store, scope, origin) = setup("flow");
        let m1 = remember_one(&mut store, &scope, &origin, 1, "用户住在杭州");
        match store.resident_pin(&scope, &m1, None, None, None).unwrap() {
            PinOutcome::Pinned { version: 1, position: 0 } => {}
            other => panic!("首次 pin 应为 v1/pos0：{other:?}"),
        }
        // 重复 pin 同位置：幂等不增版本。
        match store.resident_pin(&scope, &m1, None, None, None).unwrap() {
            PinOutcome::Unchanged { version: 1, position: 0 } => {}
            other => panic!("重复 pin 应幂等：{other:?}"),
        }
        // CAS 冲突。
        assert!(matches!(
            store.resident_pin(&scope, &m1, None, Some(99), None),
            Err(StoreError::VersionConflict)
        ));
        // unpin：版本 2、行保留；回执同事务落行。
        let un = store.resident_unpin(&scope, &m1, Some(1), Some(("key-unpin", "hash-unpin"))).unwrap();
        assert_eq!(un.version, 2);
        assert!(!un.already_disabled);
        let receipt = store.fetch_mutation_receipt(&scope, "resident_unpin", "key-unpin").unwrap().unwrap();
        assert_eq!(receipt.result_status, "unpinned");
        assert_eq!(store.resident_pins(&scope).unwrap().len(), 0, "enabled=0 不出现在 pin 列表");
        // 重复 unpin（带或不带 CAS）：幂等。
        let un2 = store.resident_unpin(&scope, &m1, Some(2), None).unwrap();
        assert!(un2.already_disabled && un2.version == 2);
        // 重新 pin：沿原行版本 3。
        match store.resident_pin(&scope, &m1, None, None, None).unwrap() {
            PinOutcome::Pinned { version: 3, position: 0 } => {}
            other => panic!("重 pin 应为 v3：{other:?}"),
        }
        // 首次 pin 不接受 expected_pin_version。
        let m2 = remember_one(&mut store, &scope, &origin, 2, "用户偏好先看结论");
        assert!(matches!(
            store.resident_pin(&scope, &m2, None, Some(1), None),
            Err(StoreError::VersionConflict)
        ));
    }

    #[test]
    fn pin_cross_scope_and_missing_memory_rejected() {
        // doc6/02 §8.7：跨 scope/不存在 ID 一律 MemoryNotFound，不泄露存在性。
        let (mut store, scope, origin) = setup("iso");
        let other_scope = {
            let dir =
                std::env::temp_dir().join(format!("am-res-test-{}-iso-u2", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            store.principal_add("t", "u2", &dir.join("u2.token")).unwrap();
            let token = std::fs::read_to_string(dir.join("u2.token")).unwrap();
            store.verify_token(token.trim()).unwrap().unwrap()
        };
        let m_alice = remember_one(&mut store, &scope, &origin, 1, "Alice 的居住地");
        assert!(matches!(
            store.resident_pin(&other_scope, &m_alice, None, None, None),
            Err(StoreError::MemoryNotFound)
        ));
        assert!(matches!(
            store.resident_unpin(&other_scope, &m_alice, None, None),
            Err(StoreError::MemoryNotFound)
        ));
        assert!(matches!(
            store.resident_move(&other_scope, &m_alice, 0, None),
            Err(StoreError::MemoryNotFound)
        ));
        assert!(matches!(
            store.resident_pin(&scope, "no-such-id", None, None, None),
            Err(StoreError::MemoryNotFound)
        ));
    }

    #[test]
    fn pinned_forgotten_or_expired_memory_not_visible() {
        // doc6/03 §4：forgotten/expired 的 pin 行保留作历史，但不可见。
        // forget 自身的证据指认语义由 memories.rs 独立测试覆盖，此处直接按
        // v1 终态置 forgotten（保留 evidence/revision）。
        let (mut store, scope, origin) = setup("visibility");
        let m_keep = remember_one(&mut store, &scope, &origin, 1, "长期有效的事实");
        let m_gone = remember_one(&mut store, &scope, &origin, 2, "将被遗忘的事实");
        store.resident_pin(&scope, &m_keep, None, None, None).unwrap();
        store.resident_pin(&scope, &m_gone, None, None, None).unwrap();
        store
            .conn()
            .execute(
                "UPDATE memories SET status='forgotten' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, m_gone],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE memories SET valid_until='2020-01-01T00:00:00Z' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, m_keep],
            )
            .unwrap();
        let now = now_rfc3339().unwrap();
        assert!(store.resident_visible_pins(&scope, &now).unwrap().is_empty());
        // CLI list 给出不可见原因；pin 行保留作历史。
        let rows = store.resident_pins_with_status(&scope, &now).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.visible));
        assert!(rows.iter().any(|r| r.reason == "STATUS_FORGOTTEN"));
        assert!(rows.iter().any(|r| r.reason == "STATUS_EXPIRED"));
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
        // doc6/02 §2/§8.5：重排同事务完成，无半更新；enabled position 全程唯一。
        let (mut store, scope, origin) = setup("reorder");
        let m1 = remember_one(&mut store, &scope, &origin, 1, "事实一");
        let m2 = remember_one(&mut store, &scope, &origin, 2, "事实二");
        let m3 = remember_one(&mut store, &scope, &origin, 3, "事实三");
        store.resident_pin(&scope, &m1, None, None, None).unwrap();
        store.resident_pin(&scope, &m2, None, None, None).unwrap();
        store.resident_pin(&scope, &m3, None, None, None).unwrap();
        let order =
            |store: &Store| -> Vec<String> { store.resident_pins(&scope).unwrap().into_iter().map(|p| p.memory_id).collect() };
        assert_eq!(order(&store), vec![m1.clone(), m2.clone(), m3.clone()]);
        // m3 移到首位：版本 +1，顺序 [m3,m1,m2]。
        let v = store.resident_move(&scope, &m3, 0, Some(1)).unwrap();
        assert_eq!(v, 2);
        assert_eq!(order(&store), vec![m3.clone(), m1.clone(), m2.clone()]);
        // 越界钳制到末尾。
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
        store.resident_unpin(&scope, &m3, None, None).unwrap();
        assert!(matches!(
            store.resident_move(&scope, &m3, 0, None),
            Err(StoreError::StateConflict)
        ));
        // 重新 pin 到占用位置（显式 position 0）：通过临时偏移路径，不冲突。
        // 版本链：pin v1 → move v2 → move v3 → unpin v4 → 重激活 v5。
        match store.resident_pin(&scope, &m3, Some(0), None, None).unwrap() {
            PinOutcome::Pinned { version: 5, position: 0 } => {}
            other => panic!("重激活应 v5/pos0：{other:?}"),
        }
        assert_eq!(order(&store), vec![m3.clone(), m1.clone(), m2.clone()]);
        let positions2: Vec<i64> =
            store.resident_pins(&scope).unwrap().into_iter().map(|p| p.position).collect();
        assert_eq!(positions2, vec![0, 1, 2]);
    }

    // ---- D6-3：选择函数 v1（doc6/03 §3/§4）----

    fn remember_kind(
        store: &mut Store,
        scope: &ScopeKey,
        origin: &Origin,
        seq: i64,
        claim: &str,
        kind: MemoryKind,
    ) -> String {
        let t = chrono::Utc::now();
        let ev = match store
            .record_evidence(scope, origin, seq, "user", "user", &t, claim)
            .unwrap()
        {
            IngestOutcome::Recorded(id) => id,
            IngestOutcome::AlreadyRecorded(id) => id,
        };
        match store.remember(scope, origin, &ev, claim, kind).unwrap() {
            crate::RememberOutcome::Created { memory_id, .. } => memory_id,
            crate::RememberOutcome::Dedup { memory_id, .. } => memory_id,
        }
    }

    #[test]
    fn select_resident_pinned_first_instructions_auto_facts_not() {
        // doc6/03 §3：pinned 优先；未 pin instruction 自动；未 pin fact/episode 不常驻。
        let (mut store, scope, origin) = setup("select");
        let f1 = remember_kind(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
        let ep = remember_kind(&mut store, &scope, &origin, 2, "用户上周去了西湖", MemoryKind::Episode);
        let i1 = remember_kind(&mut store, &scope, &origin, 3, "以后回答先给结论", MemoryKind::Instruction);
        let i2 = remember_kind(&mut store, &scope, &origin, 4, "以后回答用中文", MemoryKind::Instruction);
        store.resident_pin(&scope, &f1, None, None, None).unwrap();
        let now = now_rfc3339().unwrap();
        let sel = store.select_resident(&scope, &now, 24, 3000).unwrap();
        let ids: Vec<&str> = sel.items.iter().map(|i| i.memory_id.as_str()).collect();
        // pinned fact 在前；instruction 按 updated_at DESC（最新的 i2 在前）。
        assert_eq!(ids, vec![f1.as_str(), i2.as_str(), i1.as_str()]);
        // 未 pin 的 fact/episode 不自动常驻。
        assert!(!ids.contains(&ep.as_str()));
        assert_eq!(sel.omitted.len(), 0);
        assert!(!sel.truncated);
        // reasons：pinned / resident_auto。
        assert_eq!(sel.items[0].reason, "pinned");
        assert_eq!(sel.items[1].reason, "resident_auto");
        // 每条携带证据引用。
        assert!(!sel.items[0].evidence_ids.is_empty());
        // text 渲染含 ID。
        assert!(sel.text.contains(&format!("[memory: {}]", f1)));
    }

    #[test]
    fn select_resident_budget_omits_without_truncation() {
        // doc6/03 §3：条数满 → ITEM_LIMIT；字符超 → CHAR_LIMIT 且后续短条可进；
        // 不截断半句（items 内均为完整条目）。
        let (mut store, scope, origin) = setup("budget");
        let f1 = remember_kind(&mut store, &scope, &origin, 1, "第一条比较长的偏好内容用于占预算", MemoryKind::Fact);
        let f2 = remember_kind(&mut store, &scope, &origin, 2, "第二条也很长的偏好内容继续占位", MemoryKind::Preference);
        let i1 = remember_kind(&mut store, &scope, &origin, 3, "以后回答简短", MemoryKind::Instruction);
        store.resident_pin(&scope, &f1, None, None, None).unwrap();
        store.resident_pin(&scope, &f2, None, None, None).unwrap();
        let now = now_rfc3339().unwrap();
        // 条数限制：2 条 pin 后 instruction 被 ITEM_LIMIT。
        let sel = store.select_resident(&scope, &now, 2, 10000).unwrap();
        assert_eq!(sel.items.len(), 2);
        assert!(sel.omitted.iter().any(|(id, r)| id == &i1 && *r == "ITEM_LIMIT"));
        assert!(sel.truncated);
        // 字符限制：预算极小 → 长条 CHAR_LIMIT；正文仍为完整条目（非半句）。
        let sel2 = store.select_resident(&scope, &now, 24, 40).unwrap();
        assert!(sel2.items.iter().all(|i| render_entry(i).chars().count() <= 40 || sel2.items.len() == 1));
        assert!(sel2.omitted.iter().any(|(_, r)| *r == "CHAR_LIMIT"));
    }

    #[test]
    fn select_resident_conflicts_exit_body_not_omitted() {
        // doc6/03 §3：已知 contradicts 且两端均为候选 → conflict_ids，两条都不进正文。
        let (mut store, scope, origin) = setup("conflict");
        let f1 = remember_kind(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
        let f2 = remember_kind(&mut store, &scope, &origin, 2, "用户住在上海", MemoryKind::Fact);
        store.resident_pin(&scope, &f1, None, None, None).unwrap();
        store.resident_pin(&scope, &f2, None, None, None).unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO memory_relations (tenant_id, user_id, from_memory_id, to_memory_id, kind, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'contradicts', '2026-09-26T00:00:00Z')",
                params![scope.tenant_id, scope.user_id, f1, f2],
            )
            .unwrap();
        let now = now_rfc3339().unwrap();
        let sel = store.select_resident(&scope, &now, 24, 3000).unwrap();
        assert_eq!(sel.conflict_ids.len(), 2, "两端都进 conflict_ids");
        assert!(sel.items.is_empty(), "冲突对默认都不进正文");
        assert!(sel.omitted.is_empty(), "冲突退出不算预算省略");
        assert!(sel.text.is_empty());
    }

    #[test]
    fn select_resident_needs_review_on_forgotten_pin() {
        // doc6/03 §4：correct/forget 后旧 pin 不自动迁移也不可注入 → needs_review。
        let (mut store, scope, origin) = setup("review");
        let f1 = remember_kind(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
        store.resident_pin(&scope, &f1, None, None, None).unwrap();
        store
            .conn()
            .execute(
                "UPDATE memories SET status='forgotten' WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
                params![scope.tenant_id, scope.user_id, f1],
            )
            .unwrap();
        let now = now_rfc3339().unwrap();
        let sel = store.select_resident(&scope, &now, 24, 3000).unwrap();
        assert!(sel.needs_review.contains(&f1), "旧 pin 报 needs_review");
        assert!(sel.items.is_empty());
    }

    #[test]
    fn suggestions_list_active_facts_excluding_pinned() {
        // doc6/03 §3：建议= active fact/preference（未 pin），episode/instruction 不在建议。
        let (mut store, scope, origin) = setup("suggest");
        let f1 = remember_kind(&mut store, &scope, &origin, 1, "用户住在杭州", MemoryKind::Fact);
        let p2 = remember_kind(&mut store, &scope, &origin, 2, "用户偏好简短回答", MemoryKind::Preference);
        let _ep = remember_kind(&mut store, &scope, &origin, 3, "用户上周去了西湖", MemoryKind::Episode);
        let _i4 = remember_kind(&mut store, &scope, &origin, 4, "以后回答先给结论", MemoryKind::Instruction);
        store.resident_pin(&scope, &f1, None, None, None).unwrap();
        let now = now_rfc3339().unwrap();
        let rows = store.resident_suggestions(&scope, &now, 20).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.memory_id.as_str()).collect();
        assert!(!ids.contains(&f1.as_str()), "已 pin 的不出现在建议");
        assert!(ids.contains(&p2.as_str()));
        assert_eq!(rows.len(), 1, "episode/instruction 不进建议");
    }
}
