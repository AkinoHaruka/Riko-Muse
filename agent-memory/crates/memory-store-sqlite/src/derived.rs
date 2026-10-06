//! V2-D1 蒸馏视图（doc7/06）：精炼常驻 compact_memory 与四分面投影。
//!
//! 三个产物：compact_memory、四个分面、只读 Markdown 投影（manifest 入库、正文不入库）。
//! body 一律是来源记忆 claim 的**确定性摘录**，不做概括、不拼接；模型生成的多源综述属 V2-B1。
//! 读路径逐条复核来源，来源一改就立刻不注入，不等下一次重建。

use std::collections::BTreeMap;

use memory_domain::{DomainScope, ScopeKey, USER_MAIN_DOMAIN};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::explain::visible_memory_sql;
use crate::{now_rfc3339, Store, StoreError};

pub const DOC_COMPACT: &str = "compact_memory";
pub const FACET_EXPERIENCE: &str = "facet_experience";
pub const FACET_OPINIONS: &str = "facet_opinions";
pub const FACET_REFLECTIONS: &str = "facet_reflections";
pub const FACET_WORLD: &str = "facet_world";
pub const FACETS: [&str; 4] = [
    FACET_EXPERIENCE,
    FACET_OPINIONS,
    FACET_REFLECTIONS,
    FACET_WORLD,
];

/// 分面规则的版本号（doc7/06 §2）。规则改动必须换版本，不静默改变历史派生结果。
pub const FACET_GENERATOR_VERSION: &str = "facet_v1";
/// compact 选择规则的版本号（doc7/06 §3）。
pub const COMPACT_GENERATOR_VERSION: &str = "compact_v1";
pub const COMPACT_MAX_ITEMS: usize = 24;
pub const COMPACT_MAX_CHARS: usize = 1200;

/// preference 之外的偏好表达标记（\`facet_v1\`：instruction 也可能表达偏好）。
const PREFERENCE_MARKERS: [&str; 8] = [
    "喜欢",
    "偏好",
    "讨厌",
    "更愿意",
    "习惯",
    "prefer",
    "favorite",
    "dislike",
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DerivedItem {
    pub id: String,
    pub document_kind: String,
    pub position: i64,
    pub body: String,
    pub observed_or_inferred: String,
    pub generator_version: String,
    pub source_fingerprint: String,
    pub batch_version: i64,
    pub generated_at: String,
    /// (memory_id, memory_version, claim_sha256)
    pub sources: Vec<(String, i64, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DerivedView {
    pub document_kind: String,
    pub batch_version: i64,
    pub items: Vec<DerivedItem>,
    /// 本次读取因来源变化被即时屏蔽的条目数（doc7/06 §4）。
    pub skipped_stale: usize,
    /// compact 专用：本批次装配时的预算，便于调用方按需再截断。
    pub budget_items: usize,
    pub budget_chars: usize,
    pub total_chars: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DerivedRefreshOutcome {
    pub batch_version: i64,
    pub compact_items: usize,
    pub facet_items: usize,
    pub stale_removed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportEntry {
    pub path: String,
    pub document_kind: String,
    pub item_count: usize,
    pub batch_version: i64,
}

/// \`facet_v1\`（doc7/06 §2）：纯函数，只看 (kind, occurred_at, status, 是否有取代边)。
/// 同一条可以进多个分面。**不**把 kind 一对一映射到分面，也**不**把「有 supersedes 链」
/// 直接等于反思——必须有 \`memory_relations\` 取代边。
pub fn facets_of(
    kind: &str,
    claim: &str,
    occurred_at: Option<&str>,
    status: &str,
    has_supersede_edge: bool,
) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |f: &'static str, out: &mut Vec<&'static str>| {
        if !out.contains(&f) {
            out.push(f);
        }
    };
    match kind {
        "episode" => push(FACET_EXPERIENCE, &mut out),
        "fact" => {
            push(FACET_WORLD, &mut out);
            if occurred_at.is_some() {
                push(FACET_EXPERIENCE, &mut out);
            }
        }
        "preference" => push(FACET_OPINIONS, &mut out),
        "instruction" => {
            push(FACET_WORLD, &mut out);
            let lowered = claim.to_lowercase();
            if PREFERENCE_MARKERS
                .iter()
                .any(|m| claim.contains(m) || lowered.contains(m))
            {
                push(FACET_OPINIONS, &mut out);
            }
        }
        _ => {}
    }
    // 反思：只有被取代且确有取代边的旧条目才算（doc7/06 §2）。
    if status == "superseded" && has_supersede_edge {
        push(FACET_REFLECTIONS, &mut out);
    }
    out
}

/// compact 优先级（doc7/06 §3）：instruction > preference > fact > episode。
fn compact_priority(kind: &str) -> i64 {
    match kind {
        "instruction" => 0,
        "preference" => 1,
        "fact" => 2,
        "episode" => 3,
        _ => 4,
    }
}

fn fingerprint(sources: &[(String, i64, String)]) -> String {
    let mut sorted: Vec<String> = sources
        .iter()
        .map(|(id, v, sha)| format!("{id}\u{1}{v}\u{1}{sha}"))
        .collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for s in &sorted {
        hasher.update(s.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

impl Store {
    /// 重建本域（\`dom.write\`）的 compact_memory 与四个分面。
    /// 同一 (tenant,user,domain) 在同一事务内整体替换，batch_version 递增。
    pub fn derived_refresh(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<DerivedRefreshOutcome, StoreError> {
        // 派生正文只归属写域，不跨域合并（doc7/06 §4）。
        let doc_domain = dom.write.clone();
        let single = DomainScope::new(doc_domain.clone(), vec![doc_domain.clone()]);
        let now = now_rfc3339()?;

        // 1. 候选：写域内当前可见的 active 记忆（复用 V2-P1 唯一可见性谓词）。
        let visible_sql = format!(
            "SELECT m.id, m.kind, m.claim, m.status, m.occurred_at, m.version, m.claim_sha256, m.updated_at
             FROM memories m
             WHERE m.tenant_id=? AND m.user_id=? AND m.domain_id=?
               AND ({})",
            visible_memory_sql(false)
        );
        let mut rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
        )> = Vec::new();
        {
            let mut stmt = self.conn().prepare(&visible_sql)?;
            // 绑定序 = SQL 文本出现顺序：tenant、user、domain，然后才是谓词里的
            // now 与 read_json（谓词一律用无名 ?，混用编号会错位）。
            let mapped = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    doc_domain,
                    now,
                    single.read_json()
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
                    ))
                },
            )?;
            for row in mapped {
                rows.push(row?);
            }
        }

        // 2. compact：去重掉 Resident 已 pin 的条目，再按优先级 + 新近度装预算。
        let pinned: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT p.memory_id FROM resident_pins p
                 WHERE p.tenant_id=?1 AND p.user_id=?2 AND p.enabled=1",
            )?;
            let mapped = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| r.get(0))?;
            mapped.collect::<Result<Vec<String>, _>>()?
        };
        let mut compact_candidates: Vec<&(
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
            String,
        )> = rows
            .iter()
            .filter(|(id, ..)| !pinned.iter().any(|p| p == id))
            .collect();
        compact_candidates.sort_by(|a, b| {
            compact_priority(&a.1)
                .cmp(&compact_priority(&b.1))
                .then_with(|| b.7.cmp(&a.7))
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut compact_items: Vec<(String, String, Vec<(String, i64, String)>)> = Vec::new();
        let mut chars = 0usize;
        for (id, _kind, claim, _status, _occ, version, sha, _upd) in compact_candidates {
            if compact_items.len() >= COMPACT_MAX_ITEMS {
                break;
            }
            let add = claim.chars().count();
            if chars + add > COMPACT_MAX_CHARS {
                break; // 不截断单条正文，超预算即停
            }
            chars += add;
            compact_items.push((
                id.clone(),
                claim.clone(),
                vec![(id.clone(), *version, sha.clone())],
            ));
        }

        // 3. 分面：active 的按 kind 归面；superseded 且有取代边的进 reflections。
        let mut facet_items: BTreeMap<String, Vec<(String, String, Vec<(String, i64, String)>)>> =
            BTreeMap::new();
        for (id, kind, claim, status, occurred, version, sha, _upd) in &rows {
            let has_edge = self.has_supersede_edge(scope, id)?;
            for facet in facets_of(kind, claim, occurred.as_deref(), status, has_edge) {
                facet_items.entry(facet.to_string()).or_default().push((
                    id.clone(),
                    claim.clone(),
                    vec![(id.clone(), *version, sha.clone())],
                ));
            }
        }
        // reflections 的来源是被取代的旧条目，必须走历史可见性单独取。
        let superseded_sql = format!(
            "SELECT m.id, m.kind, m.claim, m.status, m.occurred_at, m.version, m.claim_sha256
             FROM memories m
             WHERE m.tenant_id=? AND m.user_id=? AND m.domain_id=?
               AND m.status='superseded'
               AND EXISTS (SELECT 1 FROM memory_relations rel
                   WHERE rel.tenant_id=m.tenant_id AND rel.user_id=m.user_id
                     AND rel.to_memory_id=m.id AND rel.kind='supersedes')
             ORDER BY m.updated_at DESC, m.id"
        );
        {
            let mut stmt = self.conn().prepare(&superseded_sql)?;
            let mapped =
                stmt.query_map(params![scope.tenant_id, scope.user_id, doc_domain], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })?;
            for row in mapped {
                let (id, kind, claim, status, occurred, version, sha) = row?;
                for facet in facets_of(&kind, &claim, occurred.as_deref(), &status, true) {
                    if facet == FACET_REFLECTIONS {
                        let entry = facet_items.entry(facet.to_string()).or_default();
                        if !entry.iter().any(|(eid, ..)| eid == &id) {
                            entry.push((
                                id.clone(),
                                claim.clone(),
                                vec![(id.clone(), version, sha.clone())],
                            ));
                        }
                    }
                }
            }
        }

        // 4. 事务内整体替换。
        let tx = self.conn_mut().transaction()?;
        let prev: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(batch_version),0) FROM derived_items
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let stale_removed: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM derived_items
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "DELETE FROM derived_items WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
            params![scope.tenant_id, scope.user_id, doc_domain],
        )?;
        let batch_version = prev + 1;

        let mut insert_item = |kind: &str,
                               position: i64,
                               body: &str,
                               generator: &str,
                               sources: &[(String, i64, String)]|
         -> Result<(), StoreError> {
            let id = Uuid::now_v7().to_string();
            tx.execute(
                "INSERT INTO derived_items
                   (id, tenant_id, user_id, domain_id, document_kind, position, body,
                    observed_or_inferred, generator_version, source_fingerprint, status,
                    batch_version, generated_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'observed',?8,?9,'active',?10,?11,?11)",
                params![
                    id,
                    scope.tenant_id,
                    scope.user_id,
                    doc_domain,
                    kind,
                    position,
                    body,
                    generator,
                    fingerprint(sources),
                    batch_version,
                    now
                ],
            )?;
            for (mid, ver, sha) in sources {
                tx.execute(
                    "INSERT INTO derived_item_sources
                       (tenant_id, user_id, item_id, memory_id, memory_version, claim_sha256)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![scope.tenant_id, scope.user_id, id, mid, ver, sha],
                )?;
            }
            Ok(())
        };

        let mut compact_count = 0usize;
        for (position, (_, body, sources)) in compact_items.iter().enumerate() {
            insert_item(
                DOC_COMPACT,
                position as i64,
                body,
                COMPACT_GENERATOR_VERSION,
                sources,
            )?;
            compact_count += 1;
        }
        let mut facet_count = 0usize;
        for facet in FACETS {
            let Some(items) = facet_items.get(facet) else {
                continue;
            };
            for (position, (_, body, sources)) in items.iter().enumerate() {
                insert_item(
                    facet,
                    position as i64,
                    body,
                    FACET_GENERATOR_VERSION,
                    sources,
                )?;
                facet_count += 1;
            }
        }
        tx.commit()?;
        Ok(DerivedRefreshOutcome {
            batch_version,
            compact_items: compact_count,
            facet_items: facet_count,
            stale_removed: stale_removed as usize,
        })
    }

    fn has_supersede_edge(&self, scope: &ScopeKey, memory_id: &str) -> Result<bool, StoreError> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM memory_relations
             WHERE tenant_id=?1 AND user_id=?2 AND to_memory_id=?3 AND kind='supersedes'",
            params![scope.tenant_id, scope.user_id, memory_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// 读取一个派生文档：逐条复核来源，来源变化即时屏蔽（doc7/06 §4）。
    pub fn derived_view_readable(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        document_kind: &str,
    ) -> Result<DerivedView, StoreError> {
        let now = now_rfc3339()?;
        let mut stmt = self.conn().prepare(
            "SELECT id, position, body, observed_or_inferred, generator_version,
                    source_fingerprint, batch_version, generated_at
             FROM derived_items
             WHERE tenant_id=?1 AND user_id=?2 AND document_kind=?3 AND status='active'
               AND domain_id IN (SELECT value FROM json_each(?4))
             ORDER BY position, id",
        )?;
        let rows = stmt.query_map(
            params![
                scope.tenant_id,
                scope.user_id,
                document_kind,
                dom.read_json()
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                ))
            },
        )?;
        let raw: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;

        let mut items = Vec::new();
        let mut skipped_stale = 0usize;
        let mut batch_version = 0i64;
        for (id, position, body, observed, generator, fp, batch, generated_at) in raw {
            batch_version = batch_version.max(batch);
            let sources = self.derived_item_sources(scope, &id)?;
            let history_tolerant = document_kind == FACET_REFLECTIONS;
            let mut ok = !sources.is_empty();
            for (mid, ver, sha) in &sources {
                if !self.source_still_matches(scope, dom, mid, *ver, sha, history_tolerant, &now)? {
                    ok = false;
                    break;
                }
            }
            if !ok {
                skipped_stale += 1;
                continue;
            }
            items.push(DerivedItem {
                id,
                document_kind: document_kind.to_string(),
                position,
                body,
                observed_or_inferred: observed,
                generator_version: generator,
                source_fingerprint: fp,
                batch_version: batch,
                generated_at,
                sources,
            });
        }
        let total_chars = items.iter().map(|i| i.body.chars().count()).sum();
        Ok(DerivedView {
            document_kind: document_kind.to_string(),
            batch_version,
            items,
            skipped_stale,
            budget_items: COMPACT_MAX_ITEMS,
            budget_chars: COMPACT_MAX_CHARS,
            total_chars,
        })
    }

    pub fn compact_view(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<DerivedView, StoreError> {
        self.derived_view_readable(scope, dom, DOC_COMPACT)
    }

    pub fn facet_view(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        facet: &str,
    ) -> Result<DerivedView, StoreError> {
        if !FACETS.contains(&facet) {
            return Err(StoreError::InvalidPageField);
        }
        self.derived_view_readable(scope, dom, facet)
    }

    pub fn derived_item_sources(
        &self,
        scope: &ScopeKey,
        item_id: &str,
    ) -> Result<Vec<(String, i64, String)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT memory_id, memory_version, claim_sha256 FROM derived_item_sources
             WHERE tenant_id=?1 AND user_id=?2 AND item_id=?3 ORDER BY memory_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, item_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 来源是否仍然是这条派生正文的有效依据。
    /// \`history_tolerant\`：reflections 的来源按构造就是 superseded，只核版本/哈希与未被遗忘。
    #[allow(clippy::too_many_arguments)]
    fn source_still_matches(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        memory_id: &str,
        version: i64,
        sha: &str,
        history_tolerant: bool,
        now: &str,
    ) -> Result<bool, StoreError> {
        if history_tolerant {
            let n: i64 = self.conn().query_row(
                "SELECT COUNT(*) FROM memories m
                 WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3
                   AND m.version=?4 AND m.claim_sha256=?5
                   AND m.status IN ('active','superseded','expired')
                   AND m.domain_id IN (SELECT value FROM json_each(?6))",
                params![
                    scope.tenant_id,
                    scope.user_id,
                    memory_id,
                    version,
                    sha,
                    dom.read_json()
                ],
                |r| r.get(0),
            )?;
            return Ok(n > 0);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM memories m
             WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3
               AND m.version=?4 AND m.claim_sha256=?5
               AND ({})",
            visible_memory_sql(false)
        );
        let n: i64 = self.conn().query_row(
            &sql,
            params![
                scope.tenant_id,
                scope.user_id,
                memory_id,
                version,
                sha,
                now,
                dom.read_json()
            ],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// purge / forget 之后清理：先删来源行，再删零来源的孤立条目（doc7/06 §4）。
    pub fn derived_prune_orphans(&self, scope: &ScopeKey) -> Result<usize, StoreError> {
        let removed = self.conn().execute(
            "DELETE FROM derived_items
             WHERE tenant_id=?1 AND user_id=?2
               AND NOT EXISTS (SELECT 1 FROM derived_item_sources s
                   WHERE s.tenant_id=derived_items.tenant_id
                     AND s.user_id=derived_items.user_id
                     AND s.item_id=derived_items.id)",
            params![scope.tenant_id, scope.user_id],
        )?;
        Ok(removed)
    }

    /// 删除指定记忆的派生来源行（purge 闭包用），返回删除行数。
    pub fn derived_drop_sources_for_memories(
        &self,
        scope: &ScopeKey,
        memory_ids: &[String],
    ) -> Result<usize, StoreError> {
        let mut total = 0usize;
        for mid in memory_ids {
            total += self.conn().execute(
                "DELETE FROM derived_item_sources
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, mid],
            )?;
        }
        Ok(total)
    }

    /// 导出清单：把当前可读的派生文档列成 (路径, 条目数, 版本)。
    pub fn derived_export_entries(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<(Vec<ExportEntry>, i64), StoreError> {
        let compact = self.compact_view(scope, dom)?;
        let mut entries = vec![ExportEntry {
            path: "COMPACT.md".to_string(),
            document_kind: DOC_COMPACT.to_string(),
            item_count: compact.items.len(),
            batch_version: compact.batch_version,
        }];
        let mut batch = compact.batch_version;
        for facet in FACETS {
            let view = self.facet_view(scope, dom, facet)?;
            batch = batch.max(view.batch_version);
            entries.push(ExportEntry {
                path: format!("bank/{}.md", facet.trim_start_matches("facet_")),
                document_kind: facet.to_string(),
                item_count: view.items.len(),
                batch_version: view.batch_version,
            });
        }
        Ok((entries, batch))
    }

    /// 记一次导出（manifest 入库；正文不入库）。
    pub fn derived_write_manifest(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
        batch_version: i64,
        entries: &[ExportEntry],
        outcome: &str,
    ) -> Result<String, StoreError> {
        let id = Uuid::now_v7().to_string();
        let manifest = serde_json::json!({
            "tenant_id": scope.tenant_id,
            "user_id": scope.user_id,
            "domain_id": dom.write,
            "batch_version": batch_version,
            "outcome": outcome,
            "generated_at": now_rfc3339()?,
            "files": entries,
        });
        self.conn_mut().execute(
            "INSERT INTO derived_exports
               (id, tenant_id, user_id, domain_id, batch_version, manifest_json, outcome, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                scope.tenant_id,
                scope.user_id,
                dom.write,
                batch_version.max(1),
                manifest.to_string(),
                outcome,
                now_rfc3339()?
            ],
        )?;
        Ok(id)
    }
}

/// 供服务端判断某个 \`?facet=\` 取值是否合法。
pub fn is_facet(value: &str) -> bool {
    FACETS.contains(&value)
}

/// 缺省域常量转发（服务端与 CLI 共用，避免各自硬编码字符串）。
pub fn default_domain() -> &'static str {
    USER_MAIN_DOMAIN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_rules_are_cross_kind_and_multi_facet() {
        // episode → experience（不因 kind 名不同而漏）
        assert_eq!(
            facets_of("episode", "去了趟成都", None, "active", false),
            vec![FACET_EXPERIENCE]
        );
        // 有时间的事实 → world + experience（同一条进多个分面）
        let f = facets_of(
            "fact",
            "2026 年搬到成都",
            Some("2026-01-01T00:00:00Z"),
            "active",
            false,
        );
        assert!(f.contains(&FACET_WORLD) && f.contains(&FACET_EXPERIENCE));
        // 无时间的事实 → 只有 world
        assert_eq!(
            facets_of("fact", "用户的猫叫咪咪", None, "active", false),
            vec![FACET_WORLD]
        );
        // preference → opinions
        assert_eq!(
            facets_of("preference", "用户喜欢黑咖啡", None, "active", false),
            vec![FACET_OPINIONS]
        );
        // instruction 表达偏好 → world + opinions
        let f = facets_of(
            "instruction",
            "回答我时更喜欢简短一点",
            None,
            "active",
            false,
        );
        assert!(f.contains(&FACET_WORLD) && f.contains(&FACET_OPINIONS));
        // 普通 instruction → world
        assert_eq!(
            facets_of("instruction", "以后用中文回答", None, "active", false),
            vec![FACET_WORLD]
        );
    }

    #[test]
    fn reflections_require_an_actual_supersede_edge() {
        // doc7/06 §2：有 supersedes 链**不**自动等于反思。
        let no_edge = facets_of("fact", "用户住在杭州", None, "superseded", false);
        assert!(!no_edge.contains(&FACET_REFLECTIONS));
        let with_edge = facets_of("fact", "用户住在杭州", None, "superseded", true);
        assert!(with_edge.contains(&FACET_REFLECTIONS));
        // active 的条目永远不是反思
        let active = facets_of("fact", "用户住在杭州", None, "active", true);
        assert!(!active.contains(&FACET_REFLECTIONS));
    }

    #[test]
    fn compact_priority_order_is_stable() {
        assert!(compact_priority("instruction") < compact_priority("preference"));
        assert!(compact_priority("preference") < compact_priority("fact"));
        assert!(compact_priority("fact") < compact_priority("episode"));
    }
}
