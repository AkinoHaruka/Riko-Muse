//! V2-P1 统一可见性与精读读模型（doc7/05 §1—§3、§5）。
//!
//! 可见性只有一个真相：\`visible_memory_sql\`。读接口不再依赖 M1 调度器「先把行改成
//! expired」的时机——\`valid_until\` 一过，get/search/explain 立刻不可见。

use memory_domain::refs::memory_stable_ref;
use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params_from_iter, OptionalExtension};
use serde::Serialize;

use crate::{now_rfc3339, Store, StoreError};

/// 记忆的可见性谓词（doc7/05 §1）。别名固定为 \`m\`。
///
/// 绑定参数按 SQL 文本顺序：**now**、**read_domain_json**（两处无名 \`?\`）。
/// 四条：status、valid_until、未被 retire、至少一条有效来源。域条件紧随其后。
pub fn visible_memory_sql(include_history: bool) -> String {
    let status = if include_history {
        "m.status IN ('active','superseded','expired')"
    } else {
        "m.status = 'active'"
    };
    format!(
        "{status}
         AND (m.valid_until IS NULL OR m.valid_until > ?)
         AND m.domain_id IN (SELECT value FROM json_each(?))
         AND NOT EXISTS (SELECT 1 FROM memory_retirements r
             WHERE r.tenant_id=m.tenant_id AND r.user_id=m.user_id AND r.memory_id=m.id)
         AND EXISTS (SELECT 1 FROM memory_evidence me
             WHERE me.tenant_id=m.tenant_id AND me.user_id=m.user_id AND me.memory_id=m.id
               AND NOT EXISTS (SELECT 1 FROM suppressed_sources ss
                   WHERE ss.tenant_id=me.tenant_id AND ss.user_id=me.user_id
                     AND ss.evidence_id=me.evidence_id
                     AND ss.domain_id=m.domain_id))"
    )
}

/// 单条 evidence 的精读视图（doc7/05 §2）。\`quote\` 一律按 UTF-8 字节 span 逐字切片。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvidenceDetail {
    pub evidence_id: String,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
    pub quote: String,
    /// true=逐字 span 精确；false=该行没有 span，\`quote\` 是整条事件正文。
    pub span_exact: bool,
    pub role: String,
    pub source_kind: String,
    pub occurred_at: String,
    pub host_id: String,
    pub session_id: String,
    pub suppressed: bool,
    pub tombstoned: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ExplainRelations {
    /// 本条被哪条取代（correct 产生的新记忆）。
    pub superseded_by: Option<String>,
    /// 本条取代了哪条。
    pub supersedes: Option<String>,
    pub retired: bool,
    pub retirement_reason_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryExplain {
    pub memory_id: String,
    pub kind: String,
    pub claim: String,
    pub status: String,
    pub version: i64,
    pub domain_id: String,
    pub source_class: String,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub evidence: Vec<EvidenceDetail>,
    /// 谁表达了证据（role/source_kind 映射）；不推断具体人物身份。
    pub speaker: Option<String>,
    /// 命题主体。内核没有可靠字段，恒为 None，不得由 claim/quote 反推。
    pub subject: Option<String>,
    pub subject_source: &'static str,
    /// 保存原因码（\`memory_candidates.reason_code\`）；缺就返回 None，不编造理由。
    pub reason_code: Option<String>,
    pub relations: ExplainRelations,
    pub stable_ref: String,
    /// 本行是否通过统一可见性判定（\`include_history=true\` 时可能为 false 但仍返回）。
    pub visible: bool,
}

/// 由 role/source_kind 映射 speaker（doc7/05 §2）。不认识的角色返回 None。
fn speaker_of(role: &str, source_kind: &str) -> Option<String> {
    // role 是「谁在说话」，source_kind 是来源类别；role 可识别时以 role 为准，
    // 只有 role 不认识才回落到 source_kind。两者都不认识就返回 None。
    let r = role.to_ascii_lowercase();
    let s = source_kind.to_ascii_lowercase();
    for candidate in [r.as_str(), s.as_str()] {
        match candidate {
            "user" => return Some("user".to_string()),
            "assistant" => return Some("assistant".to_string()),
            "tool" => return Some("tool".to_string()),
            _ => {}
        }
    }
    None
}

impl Store {
    /// 批量过滤候选 ID（doc7/05 §1）：谓词只有一个副本，检索各道合并后统一过闸。
    /// 返回保序去重后的可见 ID。
    pub fn filter_visible(
        &self,
        scope: &ScopeKey,
        ids: &[String],
        dom: &DomainScope,
        include_history: bool,
    ) -> Result<Vec<String>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique: Vec<String> = Vec::new();
        for id in ids {
            if !unique.iter().any(|x| x == id) {
                unique.push(id.clone());
            }
        }
        let placeholders = vec!["?"; unique.len()].join(",");
        let sql = format!(
            "SELECT m.id FROM memories m
             WHERE m.tenant_id=? AND m.user_id=? AND m.id IN ({placeholders})
               AND ({})",
            visible_memory_sql(include_history)
        );
        let now = now_rfc3339()?;
        let read_json = dom.read_json();
        let mut bind: Vec<&dyn rusqlite::ToSql> = Vec::new();
        bind.push(&scope.tenant_id);
        bind.push(&scope.user_id);
        for id in &unique {
            bind.push(id);
        }
        bind.push(&now);
        bind.push(&read_json);
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(bind), |r| r.get::<_, String>(0))?;
        let visible: Vec<String> = rows.collect::<Result<Vec<_>, _>>()?;
        // 保持输入顺序。
        Ok(unique
            .into_iter()
            .filter(|id| visible.contains(id))
            .collect())
    }

    /// 精读（doc7/05 §2/§3）。\`include_history=true\` 时允许 superseded/expired，
    /// 但 forgotten 与已 purge 的正文永不返回。
    pub fn memory_explain(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        dom: &DomainScope,
        include_history: bool,
    ) -> Result<Option<MemoryExplain>, StoreError> {
        let now = now_rfc3339()?;
        let sql = format!(
            "SELECT m.kind, m.claim, m.status, m.version, m.domain_id, m.source_class,
                    m.occurred_at, m.valid_from, m.valid_until, m.created_at, m.updated_at,
                    CASE WHEN ({}) THEN 1 ELSE 0 END
             FROM memories m
             WHERE m.tenant_id=? AND m.user_id=? AND m.id=?
               AND m.domain_id IN (SELECT value FROM json_each(?))",
            visible_memory_sql(include_history)
        );
        let read_json = dom.read_json();
        let row: Option<(
            String,
            String,
            String,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            i64,
        )> = self
            .conn()
            .query_row(
                &sql,
                rusqlite::params![
                    now,
                    read_json,
                    scope.tenant_id,
                    scope.user_id,
                    memory_id,
                    read_json
                ],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            kind,
            claim,
            status,
            version,
            domain_id,
            source_class,
            occurred_at,
            valid_from,
            valid_until,
            created_at,
            updated_at,
            visible_flag,
        )) = row
        else {
            return Ok(None);
        };
        // forgotten 与 superseded 的历史可见性：forgotten 恒不返回。
        if status == "forgotten" {
            return Ok(None);
        }
        let visible = visible_flag == 1;
        if !include_history && !visible {
            return Ok(None);
        }

        let evidence = self.memory_evidence_detail(scope, memory_id)?;
        let speaker = evidence
            .first()
            .and_then(|e| speaker_of(&e.role, &e.source_kind));
        let reason_code = self.memory_reason_code(scope, memory_id, &claim)?;
        let relations = self.explain_relations(scope, memory_id)?;
        Ok(Some(MemoryExplain {
            memory_id: memory_id.to_string(),
            kind,
            claim,
            status,
            version,
            domain_id: domain_id.clone(),
            source_class,
            occurred_at,
            valid_from,
            valid_until,
            created_at,
            updated_at,
            evidence,
            speaker,
            subject: None,
            subject_source: "unknown",
            reason_code,
            relations,
            stable_ref: memory_stable_ref(
                &scope.tenant_id,
                &scope.user_id,
                &domain_id,
                memory_id,
                version,
            ),
            visible,
        }))
    }

    /// 证据明细：按 UTF-8 字节 span 从 evidence_events.content 逐字切片。
    /// span 为 NULL 或越界时返回整条正文并标 span_exact=false（doc7/05 §2）。
    pub fn memory_evidence_detail(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<Vec<EvidenceDetail>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT me.evidence_id, me.start_byte, me.end_byte, e.content, e.role, e.source_kind,
                    e.occurred_at, e.host_id, e.session_id,
                    EXISTS (SELECT 1 FROM suppressed_sources ss
                        WHERE ss.tenant_id=me.tenant_id AND ss.user_id=me.user_id
                          AND ss.evidence_id=me.evidence_id),
                    EXISTS (SELECT 1 FROM purge_tombstones pt
                        WHERE pt.tenant_id=me.tenant_id AND pt.user_id=me.user_id
                          AND pt.source_kind='evidence' AND pt.source_id=e.content_sha256)
             FROM memory_evidence me
             JOIN evidence_events e
               ON e.tenant_id=me.tenant_id AND e.user_id=me.user_id AND e.id=me.evidence_id
             WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.memory_id=?3
             ORDER BY e.occurred_at, me.evidence_id",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
            |r| {
                let content: String = r.get(3)?;
                let start: Option<i64> = r.get(1)?;
                let end: Option<i64> = r.get(2)?;
                let (quote, span_exact) = slice_quote(&content, start, end);
                Ok(EvidenceDetail {
                    evidence_id: r.get(0)?,
                    start_byte: start,
                    end_byte: end,
                    quote,
                    span_exact,
                    role: r.get(4)?,
                    source_kind: r.get(5)?,
                    occurred_at: r.get(6)?,
                    host_id: r.get(7)?,
                    session_id: r.get(8)?,
                    suppressed: r.get::<_, i64>(9)? != 0,
                    tombstoned: r.get::<_, i64>(10)? != 0,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 保存原因码：\`memory_candidates\` 里同一 claim 的候选（无则 None）。
    /// 理由文本与来源证据是两件事，缺就返回 None，不编造。
    fn memory_reason_code(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
        claim: &str,
    ) -> Result<Option<String>, StoreError> {
        let reason: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT c.reason_code FROM memory_evidence me
                 JOIN memory_candidates c
                   ON c.tenant_id=me.tenant_id AND c.user_id=me.user_id
                  AND c.primary_evidence_id=me.evidence_id
                 WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.memory_id=?3
                   AND c.claim=?4
                 ORDER BY c.created_at LIMIT 1",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id, claim],
                |r| r.get(0),
            )
            .optional()?;
        Ok(reason.flatten())
    }

    fn explain_relations(
        &self,
        scope: &ScopeKey,
        memory_id: &str,
    ) -> Result<ExplainRelations, StoreError> {
        let superseded_by: Option<String> = self
            .conn()
            .query_row(
                "SELECT from_memory_id FROM memory_relations
                 WHERE tenant_id=?1 AND user_id=?2 AND to_memory_id=?3 AND kind='supersedes'
                 ORDER BY rowid LIMIT 1",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        let supersedes: Option<String> = self
            .conn()
            .query_row(
                "SELECT to_memory_id FROM memory_relations
                 WHERE tenant_id=?1 AND user_id=?2 AND from_memory_id=?3 AND kind='supersedes'
                 ORDER BY rowid LIMIT 1",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        let retirement: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT reason_code FROM memory_retirements
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                rusqlite::params![scope.tenant_id, scope.user_id, memory_id],
                |r| r.get(0),
            )
            .optional()?;
        let retired = retirement.is_some();
        Ok(ExplainRelations {
            superseded_by,
            supersedes,
            retired,
            retirement_reason_code: retirement.flatten(),
        })
    }
}

/// 按字节 span 切出逐字 quote。不做空白折叠、不拼接、不由模型重写。
fn slice_quote(content: &str, start: Option<i64>, end: Option<i64>) -> (String, bool) {
    let (Some(s), Some(e)) = (start, end) else {
        return (content.to_string(), false);
    };
    if s < 0 || e <= s {
        return (content.to_string(), false);
    }
    match content
        .as_bytes()
        .get(s as usize..e as usize)
        .and_then(|b| std::str::from_utf8(b).ok())
    {
        Some(q) => (q.to_string(), true),
        None => (content.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_quote_is_verbatim_and_falls_back() {
        let content = "我在杭州做后端开发。";
        // 「杭州」的字节区间（UTF-8：我=3B，在=3B）。
        let (q, exact) = slice_quote(content, Some(6), Some(12));
        assert_eq!(q, "杭州");
        assert!(exact);
        // 无 span → 整条正文，标记不精确。
        let (q, exact) = slice_quote(content, None, None);
        assert_eq!(q, content);
        assert!(!exact);
        // 越界/非字符边界 → 回落到整条，不 panic、不截出半个字符。
        let (q, exact) = slice_quote(content, Some(1), Some(4));
        assert_eq!(q, content);
        assert!(!exact);
    }

    #[test]
    fn speaker_mapping_does_not_invent_identity() {
        assert_eq!(speaker_of("user", "user").as_deref(), Some("user"));
        assert_eq!(
            speaker_of("assistant", "user").as_deref(),
            Some("assistant")
        );
        assert_eq!(speaker_of("tool", "tool").as_deref(), Some("tool"));
        assert_eq!(speaker_of("system", "system"), None);
    }
}
