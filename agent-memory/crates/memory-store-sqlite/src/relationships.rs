//! V2-R1 关系图谱（doc7/07）：实体、别名、分节条目与延迟读取。
//!
//! 政策边界（doc7/07 §0）：**不**放宽第三人准入，**不**让维护任务自设宽松准入。
//! 实体只从「写域内当前可见的 active 记忆」用确定性规则 \`entity_v1\` 投影出来，逐条带来源。
//! 覆盖受限是政策结果，不是本卡缺陷。

use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::explain::visible_memory_sql;
use crate::{now_rfc3339, Store, StoreError};

pub const ENTITY_GENERATOR_VERSION: &str = "entity_v1";
/// 索引预算（V2 文档 01 §6：每轮注入有预算，不注入全页）。
pub const INDEX_MAX_ENTITIES: usize = 24;
/// summary 的确定性截断长度。
const SUMMARY_MAX_CHARS: usize = 60;
const NAME_MAX_CHARS: usize = 12;
/// 名字下限：1 个汉字太容易撞上代词或量词，宁缺勿滥（doc7/07 §2）。
const NAME_MIN_CHARS: usize = 2;
/// 代词与自称：指向不明就不建实体（V2 文档 01 §3）。
const PRONOUNS: &[&str] = &[
    "他", "她", "它", "祂", "他们", "她们", "它们", "我", "你", "您", "我们", "你们", "ta", "TA",
];

/// 受控关系词表（doc7/07 §2）。按长度降序匹配，避免「妻」抢先匹配掉「妻子」。
pub const RELATION_WORDS: &[&str] = &[
    "未婚妻",
    "未婚夫",
    "女朋友",
    "男朋友",
    "妻子",
    "老婆",
    "夫人",
    "太太",
    "丈夫",
    "老公",
    "伴侣",
    "女友",
    "男友",
    "妈妈",
    "母亲",
    "爸爸",
    "父亲",
    "儿子",
    "女儿",
    "孩子",
    "哥哥",
    "姐姐",
    "弟弟",
    "妹妹",
    "兄弟",
    "姐妹",
    "同事",
    "老板",
    "上司",
    "领导",
    "下属",
    "合伙人",
    "客户",
    "老师",
    "导师",
    "同学",
    "室友",
    "邻居",
    "妈",
    "爸",
];

/// A 句的引导词：\`<REL><引导词><NAME>\`。
const A_LEADS: &[&str] = &["名字是", "叫做", "名叫", "叫"];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityMention {
    pub display_name: String,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityIndexEntry {
    pub entity_id: String,
    pub entity_kind: String,
    pub display_name: String,
    pub relation: Option<String>,
    pub aliases: Vec<String>,
    pub summary: Option<String>,
    pub version: i64,
    pub rank_source: String,
    pub closeness_rank: Option<i64>,
    pub updated_at: String,
    /// 详情引用（服务端生成，模型不得自造）。
    pub detail_ref: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntityItem {
    pub item_id: String,
    pub section: String,
    pub body: String,
    pub observed_or_inferred: String,
    pub version: i64,
    pub sources: Vec<(String, i64, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityIndex {
    pub domain_id: String,
    pub batch_version: i64,
    pub total: usize,
    pub omitted: usize,
    pub entities: Vec<EntityIndexEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub entity: EntityIndexEntry,
    pub items: Vec<EntityItem>,
    pub skipped_stale: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "resolution", rename_all = "lowercase")]
pub enum Resolution {
    None,
    One(Box<EntityIndexEntry>),
    Ambiguous(Vec<EntityIndexEntry>),
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityRefreshOutcome {
    pub batch_version: i64,
    pub entities: usize,
    pub items: usize,
    pub previous_entities: usize,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn is_relation_word(s: &str) -> bool {
    RELATION_WORDS.contains(&s)
}

/// \`entity_v1\`（doc7/07 §2）：确定性、保守、宁缺勿滥。
///
/// 只识别两种句式；一句里出现多个候选、名字含标点、名字本身就是关系词、
/// 或名字里嵌了关系词，都**整条跳过**——不猜。
pub fn extract_mentions(claim: &str) -> Vec<EntityMention> {
    let mut found: Vec<EntityMention> = Vec::new();
    let mut push = |m: EntityMention, found: &mut Vec<EntityMention>| {
        if !found.iter().any(|x| x == &m) {
            found.push(m);
        }
    };

    // 句式 A：<REL><引导词><NAME>
    let mut rels: Vec<&str> = RELATION_WORDS.to_vec();
    rels.sort_by_key(|r| std::cmp::Reverse(r.chars().count()));
    for rel in rels {
        let mut from = 0usize;
        while let Some(idx) = claim[from..].find(rel).map(|i| i + from) {
            from = idx + rel.len();
            let after = &claim[from..];
            if let Some(lead) = A_LEADS.iter().find(|l| after.starts_with(**l)) {
                let name_start = from + lead.len();
                if let Some(name) = take_name(&claim[name_start..]) {
                    push(
                        EntityMention {
                            display_name: name,
                            relation: rel.to_string(),
                        },
                        &mut found,
                    );
                }
            }
        }
    }

    // 句式 B：<NAME>是[我|我的]<REL>
    let mut from = 0usize;
    while let Some(idx) = claim[from..].find('是').map(|i| i + from) {
        // 必须按 UTF-8 字节长度前进；'是' 是 3 字节，写 +1 会切在字符中间 panic。
        from = idx + '是'.len_utf8();
        let after = &claim[from..];
        let tail = after
            .strip_prefix("我的")
            .or_else(|| after.strip_prefix('我'))
            .unwrap_or(after);
        let Some(rel) = RELATION_WORDS
            .iter()
            .filter(|r| tail.starts_with(**r))
            .max_by_key(|r| r.chars().count())
        else {
            continue;
        };
        // 名字是「是」之前那一段连续的名字字符。
        let before = &claim[..idx];
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_name_char(*c))
            .last()
            .map(|(i, _)| i);
        let Some(start) = start else { continue };
        let name = &before[start..];
        if !is_acceptable_name(name) {
            continue; // 代词、关系词、过长过短都跳过
        }
        push(
            EntityMention {
                display_name: name.to_string(),
                relation: rel.to_string(),
            },
            &mut found,
        );
    }

    // 宁缺勿滥：一句里给出多个不同候选 → 整条跳过。
    let mut names: Vec<&str> = found.iter().map(|m| m.display_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    if names.len() > 1 {
        return Vec::new();
    }
    if found.len() > 1 {
        // 同一名字但关系词不同：保留最早的，避免一句里跨关系串味。
        found.truncate(1);
    }
    found
}

/// 名字是否可接受：长度 2—12、不是关系词、不是代词、内部不嵌关系词
/// （「我妻子是我的同事」这类不算）。
fn is_acceptable_name(name: &str) -> bool {
    let n = name.chars().count();
    if n < NAME_MIN_CHARS || n > NAME_MAX_CHARS {
        return false;
    }
    if is_relation_word(name) || PRONOUNS.contains(&name) {
        return false;
    }
    // 「我妻子」这种「自称/代词 + 关系词」不是名字。
    // 注意不能一概拒绝含关系词的名字：「王老师」「李医生」是正常称呼（老师是关系词）。
    for r in RELATION_WORDS {
        if let Some(prefix) = name.strip_suffix(r) {
            if PRONOUNS.contains(&prefix) || prefix.is_empty() {
                return false;
            }
        }
    }
    true
}

/// 取一段连续名字字符；太长或不可接受都返回 None。
fn take_name(rest: &str) -> Option<String> {
    let mut out = String::new();
    for c in rest.chars() {
        if !is_name_char(c) {
            break;
        }
        if out.chars().count() >= NAME_MAX_CHARS {
            return None; // 太长，不像名字
        }
        out.push(c);
    }
    if !is_acceptable_name(&out) {
        return None;
    }
    Some(out)
}

/// slug：规范化显示名（去空白、ASCII 小写）。同名即同一实体（doc7/07 §2 如实记录为限制）。
pub fn entity_slug(display_name: &str) -> String {
    display_name
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
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

fn summary_of(body: &str) -> String {
    let mut out: String = body.chars().take(SUMMARY_MAX_CHARS).collect();
    if body.chars().count() > SUMMARY_MAX_CHARS {
        out.push('…');
    }
    out
}

fn rank_of(rank_source: &str) -> i64 {
    match rank_source {
        "user_explicit" => 0,
        "verified_role" => 1,
        _ => 2,
    }
}

impl Store {
    /// 重建本域（\`dom.write\`）的关系实体与条目（doc7/07 §2、§4）。
    pub fn relationship_refresh(
        &mut self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<EntityRefreshOutcome, StoreError> {
        let doc_domain = dom.write.clone();
        let single = DomainScope::new(doc_domain.clone(), vec![doc_domain.clone()]);
        let now = now_rfc3339()?;

        // 候选：写域内当前可见的 active 记忆（复用 V2-P1 唯一可见性谓词）。
        let sql = format!(
            "SELECT m.id, m.claim, m.version, m.claim_sha256, m.occurred_at
             FROM memories m
             WHERE m.tenant_id=? AND m.user_id=? AND m.domain_id=?
               AND ({})",
            visible_memory_sql(false)
        );
        let mut memories: Vec<(String, String, i64, String, Option<String>)> = Vec::new();
        {
            let mut stmt = self.conn().prepare(&sql)?;
            let mapped = stmt.query_map(
                params![
                    scope.tenant_id,
                    scope.user_id,
                    doc_domain,
                    now,
                    single.read_json()
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            for row in mapped {
                memories.push(row?);
            }
        }

        // 投影：实体 → (relation, aliases, items)
        struct Pending {
            display_name: String,
            relation: String,
            aliases: Vec<(String, String, String)>, // (alias, kind, memory_id)
            items: Vec<(String, String, Option<String>, Vec<(String, i64, String)>)>,
        }
        let mut pending: Vec<Pending> = Vec::new();
        for (mid, claim, version, sha, occurred) in &memories {
            for m in extract_mentions(claim) {
                let src = vec![(mid.clone(), *version, sha.clone())];
                match pending
                    .iter_mut()
                    .find(|p| p.display_name == m.display_name)
                {
                    Some(p) => {
                        if !p
                            .aliases
                            .iter()
                            .any(|(a, k, _)| a == &m.display_name && k == "name")
                        {
                            p.aliases.push((
                                m.display_name.clone(),
                                "name".to_string(),
                                mid.clone(),
                            ));
                        }
                        p.aliases
                            .push((m.relation.clone(), "role".to_string(), mid.clone()));
                        p.items.push((
                            "relationship".to_string(),
                            claim.clone(),
                            occurred.clone(),
                            src,
                        ));
                    }
                    None => {
                        let mut aliases =
                            vec![(m.display_name.clone(), "name".to_string(), mid.clone())];
                        aliases.push((m.relation.clone(), "role".to_string(), mid.clone()));
                        pending.push(Pending {
                            display_name: m.display_name.clone(),
                            relation: m.relation.clone(),
                            aliases,
                            items: vec![(
                                "relationship".to_string(),
                                claim.clone(),
                                occurred.clone(),
                                src,
                            )],
                        });
                    }
                }
            }
        }
        // facts：claim 里出现该实体名、且没有承载关系词的其它可见记忆。
        for p in pending.iter_mut() {
            for (mid, claim, version, sha, occurred) in &memories {
                if !claim.contains(&p.display_name) {
                    continue;
                }
                if p.items.iter().any(|(_, body, _, _)| body == claim) {
                    continue;
                }
                p.items.push((
                    "facts".to_string(),
                    claim.clone(),
                    occurred.clone(),
                    vec![(mid.clone(), *version, sha.clone())],
                ));
            }
        }

        // 事务内整体替换。
        let tx = self.conn_mut().transaction()?;
        let previous: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM relationship_entities
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let prev_batch: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(batch_version),0) FROM relationship_entities
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, doc_domain],
                |r| r.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "DELETE FROM relationship_entities WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
            params![scope.tenant_id, scope.user_id, doc_domain],
        )?;
        let batch_version = prev_batch + 1;
        let mut entity_count = 0usize;
        let mut item_count = 0usize;
        for p in &pending {
            let entity_id = Uuid::now_v7().to_string();
            let rank_source = if RELATION_WORDS.contains(&p.relation.as_str()) {
                "verified_role"
            } else {
                "unranked"
            };
            let summary = p.items.first().map(|(_, body, _, _)| summary_of(body));
            tx.execute(
                "INSERT INTO relationship_entities
                   (id, tenant_id, user_id, domain_id, entity_kind, slug, display_name, relation,
                    summary, closeness_rank, rank_source, version, status, generator_version,
                    batch_version, generated_at, updated_at)
                 VALUES (?1,?2,?3,?4,'person',?5,?6,?7,?8,NULL,?9,1,'active',?10,?11,?12,?12)",
                params![
                    entity_id,
                    scope.tenant_id,
                    scope.user_id,
                    doc_domain,
                    entity_slug(&p.display_name),
                    p.display_name,
                    p.relation,
                    summary,
                    rank_source,
                    ENTITY_GENERATOR_VERSION,
                    batch_version,
                    now
                ],
            )?;
            entity_count += 1;
            let mut seen_alias: Vec<(String, String)> = Vec::new();
            for (alias, kind, mid) in &p.aliases {
                if seen_alias.iter().any(|(a, k)| a == alias && k == kind) {
                    continue;
                }
                seen_alias.push((alias.clone(), kind.clone()));
                tx.execute(
                    "INSERT OR IGNORE INTO entity_aliases
                       (tenant_id, user_id, entity_id, alias, alias_kind, memory_id)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![scope.tenant_id, scope.user_id, entity_id, alias, kind, mid],
                )?;
            }
            for (section, body, occurred, sources) in &p.items {
                let item_id = Uuid::now_v7().to_string();
                tx.execute(
                    "INSERT INTO relationship_items
                       (id, tenant_id, user_id, entity_id, section, body, observed_or_inferred,
                        occurred_at, valid_until, version, status, generator_version,
                        source_fingerprint, batch_version, generated_at, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,'observed',?7,NULL,1,'active',?8,?9,?10,?11,?11)",
                    params![
                        item_id,
                        scope.tenant_id,
                        scope.user_id,
                        entity_id,
                        section,
                        body,
                        occurred,
                        ENTITY_GENERATOR_VERSION,
                        fingerprint(sources),
                        batch_version,
                        now
                    ],
                )?;
                for (mid, ver, sha) in sources {
                    tx.execute(
                        "INSERT OR IGNORE INTO relationship_sources
                           (tenant_id, user_id, item_id, memory_id, memory_version, claim_sha256)
                         VALUES (?1,?2,?3,?4,?5,?6)",
                        params![scope.tenant_id, scope.user_id, item_id, mid, ver, sha],
                    )?;
                }
                item_count += 1;
            }
        }
        tx.commit()?;
        Ok(EntityRefreshOutcome {
            batch_version,
            entities: entity_count,
            items: item_count,
            previous_entities: previous as usize,
        })
    }

    /// 索引（doc7/07 §3）：有预算、带省略计数；stale/removed 实体不进索引。
    pub fn relationship_index(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        limit: usize,
    ) -> Result<EntityIndex, StoreError> {
        let all = self.entity_entries(scope, dom)?;
        let total = all.len();
        let mut entities = all;
        let omitted = if entities.len() > limit {
            entities.len() - limit
        } else {
            0
        };
        entities.truncate(limit);
        let batch_version = self.relationship_batch_version(scope, dom)?;
        Ok(EntityIndex {
            domain_id: dom.write.clone(),
            batch_version,
            total,
            omitted,
            entities,
        })
    }

    fn relationship_batch_version(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<i64, StoreError> {
        let v: Option<i64> = self
            .conn()
            .query_row(
                "SELECT MAX(batch_version) FROM relationship_entities
                 WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3",
                params![scope.tenant_id, scope.user_id, dom.write],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(v.unwrap_or(0))
    }

    fn entity_entries(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
    ) -> Result<Vec<EntityIndexEntry>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, entity_kind, display_name, relation, summary, version, rank_source,
                    closeness_rank, updated_at
             FROM relationship_entities
             WHERE tenant_id=?1 AND user_id=?2 AND status='active'
               AND domain_id IN (SELECT value FROM json_each(?3))",
        )?;
        let rows = stmt.query_map(
            params![scope.tenant_id, scope.user_id, dom.read_json()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )?;
        let raw: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for (id, kind, name, relation, summary, version, rank_source, rank, updated_at) in raw {
            let aliases = self.entity_aliases(scope, &id)?;
            out.push(EntityIndexEntry {
                detail_ref: format!(
                    "riko://entity/{}/{}/{}/{}",
                    scope.tenant_id, scope.user_id, dom.write, id
                ),
                entity_id: id,
                entity_kind: kind,
                display_name: name,
                relation,
                aliases,
                summary,
                version,
                rank_source,
                closeness_rank: rank,
                updated_at,
            });
        }
        // 分级排序：user_explicit > verified_role > unranked；同级按名称、再按 id。
        out.sort_by(|a, b| {
            rank_of(&a.rank_source)
                .cmp(&rank_of(&b.rank_source))
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.entity_id.cmp(&b.entity_id))
        });
        Ok(out)
    }

    pub fn entity_aliases(
        &self,
        scope: &ScopeKey,
        entity_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT alias FROM entity_aliases
             WHERE tenant_id=?1 AND user_id=?2 AND entity_id=?3 ORDER BY alias",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, entity_id], |r| {
            r.get::<_, String>(0)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 详情（doc7/07 §3）：逐条复核来源，来源一改即时屏蔽。
    pub fn relationship_get(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        entity_id: &str,
        expected_version: Option<i64>,
    ) -> Result<Option<EntityDetail>, StoreError> {
        let entry = self
            .entity_entries(scope, dom)?
            .into_iter()
            .find(|e| e.entity_id == entity_id);
        let Some(entry) = entry else {
            return Ok(None);
        };
        if let Some(expected) = expected_version {
            if expected != entry.version {
                return Err(StoreError::VersionConflict);
            }
        }
        let now = now_rfc3339()?;
        let mut stmt = self.conn().prepare(
            "SELECT id, section, body, observed_or_inferred, version
             FROM relationship_items
             WHERE tenant_id=?1 AND user_id=?2 AND entity_id=?3 AND status='active'
             ORDER BY CASE section WHEN 'relationship' THEN 0 ELSE 1 END, id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, entity_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let raw: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
        let mut items = Vec::new();
        let mut skipped_stale = 0usize;
        for (item_id, section, body, observed, version) in raw {
            let sources = self.relationship_item_sources(scope, &item_id)?;
            let mut ok = !sources.is_empty();
            for (mid, ver, sha) in &sources {
                let sql = format!(
                    "SELECT COUNT(*) FROM memories m
                     WHERE m.tenant_id=?1 AND m.user_id=?2 AND m.id=?3
                       AND m.version=?4 AND m.claim_sha256=?5 AND ({})",
                    visible_memory_sql(false)
                );
                let n: i64 = self.conn().query_row(
                    &sql,
                    params![
                        scope.tenant_id,
                        scope.user_id,
                        mid,
                        ver,
                        sha,
                        now,
                        dom.read_json()
                    ],
                    |r| r.get(0),
                )?;
                if n == 0 {
                    ok = false;
                    break;
                }
            }
            if !ok {
                skipped_stale += 1;
                continue;
            }
            items.push(EntityItem {
                item_id,
                section,
                body,
                observed_or_inferred: observed,
                version,
                sources,
            });
        }
        Ok(Some(EntityDetail {
            entity: entry,
            items,
            skipped_stale,
        }))
    }

    pub fn relationship_item_sources(
        &self,
        scope: &ScopeKey,
        item_id: &str,
    ) -> Result<Vec<(String, i64, String)>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT memory_id, memory_version, claim_sha256 FROM relationship_sources
             WHERE tenant_id=?1 AND user_id=?2 AND item_id=?3 ORDER BY memory_id",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id, item_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 名称/别名解析（doc7/07 §3）：0/1/多三态，多结果返回歧义，**不猜**。
    pub fn relationship_resolve(
        &self,
        scope: &ScopeKey,
        dom: &DomainScope,
        query: &str,
    ) -> Result<Resolution, StoreError> {
        let needle = entity_slug(query);
        if needle.is_empty() {
            return Ok(Resolution::None);
        }
        let entries = self.entity_entries(scope, dom)?;
        let mut hits: Vec<EntityIndexEntry> = Vec::new();
        for e in entries {
            let mut matched = entity_slug(&e.display_name) == needle
                || e.aliases.iter().any(|a| entity_slug(a) == needle);
            if !matched {
                // 未命中的实体若挂在该名字下也算（别名表交叉）。
                let ids = self.entity_ids_by_alias(scope, &needle)?;
                matched = ids.contains(&e.entity_id);
            }
            if matched {
                hits.push(e);
            }
        }
        Ok(match hits.len() {
            0 => Resolution::None,
            1 => Resolution::One(Box::new(hits.remove(0))),
            _ => Resolution::Ambiguous(hits),
        })
    }

    fn entity_ids_by_alias(&self, scope: &ScopeKey, slug: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT entity_id FROM entity_aliases WHERE tenant_id=?1 AND user_id=?2",
        )?;
        let rows = stmt.query_map(params![scope.tenant_id, scope.user_id], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            let id = row?;
            let aliases = self.entity_aliases(scope, &id)?;
            if aliases.iter().any(|a| entity_slug(a) == slug) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// purge / forget 之后清理：先删来源行与别名行，再删零来源的条目与实体（doc7/07 §4）。
    pub fn relationship_prune_orphans(&self, scope: &ScopeKey) -> Result<usize, StoreError> {
        self.conn().execute(
            "DELETE FROM relationship_items
             WHERE tenant_id=?1 AND user_id=?2
               AND NOT EXISTS (SELECT 1 FROM relationship_sources s
                   WHERE s.tenant_id=relationship_items.tenant_id
                     AND s.user_id=relationship_items.user_id
                     AND s.item_id=relationship_items.id)",
            params![scope.tenant_id, scope.user_id],
        )?;
        let removed = self.conn().execute(
            "DELETE FROM relationship_entities
             WHERE tenant_id=?1 AND user_id=?2
               AND NOT EXISTS (SELECT 1 FROM relationship_items i
                   WHERE i.tenant_id=relationship_entities.tenant_id
                     AND i.user_id=relationship_entities.user_id
                     AND i.entity_id=relationship_entities.id)",
            params![scope.tenant_id, scope.user_id],
        )?;
        Ok(removed)
    }

    /// 删除指定记忆的关系来源行与别名行（purge 闭包用）。
    pub fn relationship_drop_for_memories(
        &self,
        scope: &ScopeKey,
        memory_ids: &[String],
    ) -> Result<usize, StoreError> {
        let mut total = 0usize;
        for mid in memory_ids {
            total += self.conn().execute(
                "DELETE FROM relationship_sources
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, mid],
            )?;
            total += self.conn().execute(
                "DELETE FROM entity_aliases
                 WHERE tenant_id=?1 AND user_id=?2 AND memory_id=?3",
                params![scope.tenant_id, scope.user_id, mid],
            )?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_both_supported_forms() {
        let a = extract_mentions("我妻子叫小雨，她喜欢园艺");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].display_name, "小雨");
        assert_eq!(a[0].relation, "妻子");

        let b = extract_mentions("老王是我的同事，做后端");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].display_name, "老王");
        assert_eq!(b[0].relation, "同事");

        let c = extract_mentions("我妈妈名字是李芳");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].display_name, "李芳");
        assert_eq!(c[0].relation, "妈妈");
    }

    #[test]
    fn skips_pronouns_punctuation_and_multiple_candidates() {
        // 代词不建实体：没有名字。
        assert!(extract_mentions("他是我同事").is_empty());
        // 名字含标点。
        assert!(extract_mentions("我同事叫小王，很厉害")
            .iter()
            .all(|m| m.display_name == "小王"));
        // 一句里多个不同候选 → 整条跳过。
        assert!(extract_mentions("我妻子叫小雨，我同事叫老王").is_empty());
        // 关系词不在词表。
        assert!(extract_mentions("我邻居叫小明").len() == 1); // 邻居在词表
        assert!(extract_mentions("我房东叫张叔").is_empty()); // 房东不在词表
    }

    #[test]
    fn slug_normalises_and_does_not_merge_similar_names() {
        assert_eq!(entity_slug(" 小雨 "), "小雨");
        assert_eq!(entity_slug("Alice"), "alice");
        assert_ne!(entity_slug("老王"), entity_slug("王老师"));
    }
}
