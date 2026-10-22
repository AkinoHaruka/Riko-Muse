//! P1 model generator input and citation admission. Model text remains untrusted.
use std::collections::{HashMap, HashSet};

use memory_domain::{DomainScope, ScopeKey};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{alignment::AlignmentSynthesisRow, now_rfc3339, Store, StoreError};

#[derive(Clone, Debug, Serialize)]
pub struct GenerationSource {
    pub reference: String,
    pub text: String,
    pub fingerprint: String,
    pub thread_id: Option<String>,
    pub evidence_id: Option<String>,
    pub start_byte: Option<i64>,
    pub end_byte: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedSentence {
    pub text: String,
    pub refs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedCandidate {
    pub sentences: Vec<GeneratedSentence>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationArtifact {
    pub id: String,
    pub kind: String,
    pub body: String,
    pub created_at: String,
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn valid_sentence(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > 240 || t.contains(['\n', '\r']) { return false; }
    let mut chars = t.chars();
    chars.next_back();
    !chars.any(|c| matches!(c, '。' | '！' | '？' | '.' | '!' | '?'))
}

impl Store {
    fn generation_transaction<T>(&mut self, work: impl FnOnce(&mut Store) -> Result<T, StoreError>) -> Result<T, StoreError> {
        self.conn().execute_batch("BEGIN IMMEDIATE")?;
        let result = work(self);
        match result {
            Ok(value) => { self.conn().execute_batch("COMMIT")?; Ok(value) }
            Err(error) => { let _ = self.conn().execute_batch("ROLLBACK"); Err(error) }
        }
    }
    pub fn generation_rejected_count(&self, scope: &ScopeKey, dom: &DomainScope, task_kind: &str) -> Result<i64, StoreError> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM task_runs WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND task_kind=?4 AND outcome='error' AND detail IN ('citation_invalid','model_json_invalid','model_transport_failed','model_unconfigured')",
            params![scope.tenant_id,scope.user_id,dom.write,task_kind], |r| r.get(0))?)
    }
    pub fn generation_artifact_has(&self, scope: &ScopeKey, dom: &DomainScope, kind: &str, signal: &str) -> Result<bool, StoreError> {
        Ok(self.conn().query_row("SELECT 1 FROM generation_artifacts WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3 AND kind=?4 AND signal_ref=?5",params![scope.tenant_id,scope.user_id,dom.write,kind,signal], |r| r.get::<_,i64>(0)).optional()?.is_some())
    }
    pub fn generation_artifact_visible(&self, scope: &ScopeKey, artifact_id: &str) -> Result<bool, StoreError> {
        let row: Option<(String, String, String, Option<String>)> = self.conn().query_row(
            "SELECT kind,sentence_refs_json,domain_id,thread_id FROM generation_artifacts WHERE tenant_id=?1 AND user_id=?2 AND id=?3",
            params![scope.tenant_id,scope.user_id,artifact_id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        ).optional()?;
        let Some((kind,refs_json,domain,thread)) = row else { return Ok(false) };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&refs_json) else { return Ok(false) };
        let Ok(candidate) = serde_json::from_value::<GeneratedCandidate>(v["candidate"].clone()) else { return Ok(false) };
        let Some(fingerprints) = v["source_fingerprints"].as_object() else { return Ok(false) };
        let snapshot: Vec<_> = fingerprints.iter().filter_map(|(reference,fp)| Some(GenerationSource { reference: reference.clone(), text: String::new(), fingerprint: fp.as_str()?.to_string(), thread_id: None, evidence_id: None, start_byte: None, end_byte: None })).collect();
        let dom = DomainScope::new(domain.clone(), vec![domain]);
        Ok(self.generation_validate(scope, &dom, thread.as_deref(), &snapshot, &candidate, if kind == "reflection_v1" { 2 } else { 1 }).is_ok())
    }
    pub fn generation_artifacts_list(&self, scope: &ScopeKey, dom: &DomainScope) -> Result<Vec<GenerationArtifact>, StoreError> {
        let mut stmt = self.conn().prepare("SELECT id,kind,body,sentence_refs_json,created_at,domain_id,thread_id FROM generation_artifacts WHERE tenant_id=?1 AND user_id=?2 AND domain_id IN (SELECT value FROM json_each(?3)) ORDER BY created_at DESC LIMIT 20")?;
        let rows = stmt.query_map(params![scope.tenant_id,scope.user_id,dom.read_json()], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id,kind,body,refs_json,created_at,row_domain,thread_id) = row?;
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&refs_json) else { continue };
            let Ok(candidate) = serde_json::from_value::<GeneratedCandidate>(v["candidate"].clone()) else { continue };
            let Some(fingerprints) = v["source_fingerprints"].as_object() else { continue };
            let snapshot: Vec<_> = fingerprints.iter().filter_map(|(reference,fp)| Some(GenerationSource { reference: reference.clone(), text: String::new(), fingerprint: fp.as_str()?.to_string(), thread_id: None, evidence_id: None, start_byte: None, end_byte: None })).collect();
            let source_domain = DomainScope::new(row_domain.clone(), vec![row_domain]);
            if self.generation_validate(scope, &source_domain, thread_id.as_deref(), &snapshot, &candidate, if kind == "reflection_v1" { 2 } else { 1 }).is_err() { continue; }
            out.push(GenerationArtifact { id, kind, body, created_at });
        }
        Ok(out)
    }
    /// Bounded, same-domain prompt inputs. The SQL visibility condition is the same predicate
    /// used by get/search; rupture spans are checked against their original event bytes.
    pub fn generation_sources(&self, scope: &ScopeKey, dom: &DomainScope, thread: Option<&str>) -> Result<Vec<GenerationSource>, StoreError> {
        let now = now_rfc3339()?;
        let mut out = Vec::new();
        let sql = format!("SELECT m.id,m.claim,m.version FROM memories m WHERE m.tenant_id=?1 AND m.user_id=?2 AND {} ORDER BY m.updated_at DESC,m.id LIMIT 24", crate::explain::visible_memory_sql(false));
        let mut stmt = self.conn().prepare(&sql)?;
        let write_domain = serde_json::json!([dom.write]).to_string();
        let rows = stmt.query_map(params![scope.tenant_id,scope.user_id,now,write_domain], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?;
        for row in rows {
            let (id, text, version) = row?;
            let evidence: Option<(String, String, Option<i64>, Option<i64>)> = self.conn().query_row(
                "SELECT e.id,e.content,me.start_byte,me.end_byte FROM memory_evidence me
                 JOIN evidence_events e ON e.tenant_id=me.tenant_id AND e.user_id=me.user_id AND e.id=me.evidence_id
                 WHERE me.tenant_id=?1 AND me.user_id=?2 AND me.memory_id=?3
                   AND e.role='user' AND e.source_kind='user'
                   AND NOT EXISTS(SELECT 1 FROM suppressed_sources ss WHERE ss.tenant_id=me.tenant_id AND ss.user_id=me.user_id AND ss.evidence_id=me.evidence_id AND ss.domain_id=?4)
                 ORDER BY e.received_at DESC,e.id LIMIT 1",
                params![scope.tenant_id,scope.user_id,id,dom.write],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
            ).optional()?;
            if let Some((evidence_id, content, start_byte, end_byte)) = evidence {
                let quote = match (start_byte,end_byte) {
                    (Some(start),Some(end)) => content.get(start as usize..end as usize),
                    _ => Some(content.as_str()),
                };
                let Some(quote) = quote else { continue };
                out.push(GenerationSource { reference: format!("memory:{id}"), fingerprint: sha(&format!("{version}:{text}:{evidence_id}:{content}:{start_byte:?}:{end_byte:?}")), text: format!("{text} | 用户原话：{quote}"), thread_id: None, evidence_id: Some(evidence_id), start_byte, end_byte });
            }
        }
        let mut stmt = self.conn().prepare("SELECT r.id,r.thread_id,e.content,r.start_byte,r.end_byte,e.id FROM rupture_events r JOIN evidence_events e ON e.tenant_id=r.tenant_id AND e.user_id=r.user_id AND e.id=r.evidence_id JOIN repair_threads t ON t.tenant_id=r.tenant_id AND t.user_id=r.user_id AND t.id=r.thread_id AND t.status='open' WHERE r.tenant_id=?1 AND r.user_id=?2 AND r.domain_id=?3 AND (?4 IS NULL OR r.thread_id=?4) AND NOT EXISTS(SELECT 1 FROM suppressed_sources s WHERE s.tenant_id=r.tenant_id AND s.user_id=r.user_id AND s.evidence_id=r.evidence_id AND s.domain_id=r.domain_id) ORDER BY r.detected_at DESC,r.id LIMIT 12")?;
        let rows = stmt.query_map(params![scope.tenant_id,scope.user_id,dom.write,thread], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?)))?;
        for row in rows {
            let (id, thread_id, content, start, end, evidence_id) = row?;
            if content.get(start as usize..end as usize).is_none() { continue }
            out.push(GenerationSource { reference: format!("rupture:{id}"), text: content[start as usize..end as usize].to_owned(), fingerprint: sha(&format!("{content}:{start}:{end}")), thread_id, evidence_id: Some(evidence_id), start_byte: Some(start), end_byte: Some(end) });
        }
        if thread.is_some() {
            let rupture_evidence: HashSet<_> = out.iter().filter(|s| s.reference.starts_with("rupture:")).filter_map(|s| s.evidence_id.clone()).collect();
            out.retain(|s| s.reference.starts_with("rupture:") || s.evidence_id.as_ref().is_some_and(|id| rupture_evidence.contains(id)));
        }
        Ok(out)
    }

    /// Reject entire candidate if any sentence or citation fails. The source snapshot is
    /// re-read immediately before commit, including current visibility and source hash.
    pub fn generation_validate(&self, scope: &ScopeKey, dom: &DomainScope, thread: Option<&str>, snapshot: &[GenerationSource], candidate: &GeneratedCandidate, min_sources: usize) -> Result<String, StoreError> {
        if candidate.sentences.is_empty() || candidate.sentences.len() > 8 { return Err(StoreError::StaleInput); }
        let current_sources = self.generation_sources(scope, dom, thread)?;
        let current: HashMap<_,_> = current_sources.iter().map(|s| (s.reference.as_str(), s.fingerprint.as_str())).collect();
        let allowed: HashMap<_,_> = snapshot.iter().map(|s| (s.reference.as_str(), s.fingerprint.as_str())).collect();
        let mut distinct = HashSet::new();
        let mut body = String::new();
        for sentence in &candidate.sentences {
            if !valid_sentence(&sentence.text) || sentence.refs.is_empty() || sentence.refs.len() > 6 { return Err(StoreError::StaleInput); }
            body.push_str(sentence.text.trim());
            for reference in &sentence.refs {
                if allowed.get(reference.as_str()).copied() != current.get(reference.as_str()).copied() || !allowed.contains_key(reference.as_str()) { return Err(StoreError::StaleInput); }
                distinct.insert(reference.as_str());
                body.push_str(&format!(" [{reference}]"));
            }
            body.push('\n');
        }
        if distinct.len() < min_sources { return Err(StoreError::StaleInput); }
        if let Some(thread_id) = thread {
            if !current_sources.iter().any(|s| s.thread_id.as_deref() == Some(thread_id) && distinct.contains(s.reference.as_str())) {
                return Err(StoreError::StaleInput);
            }
        }
        Ok(body)
    }

    pub fn generation_publish_synthesis(&mut self, scope: &ScopeKey, dom: &DomainScope, base: &AlignmentSynthesisRow, snapshot: &[GenerationSource], candidate: &GeneratedCandidate) -> Result<Option<AlignmentSynthesisRow>, StoreError> {
        self.generation_transaction(|store| {
            let body = store.generation_validate(scope, dom, None, snapshot, candidate, 2)?;
            let write_domain = DomainScope::new(dom.write.clone(), vec![dom.write.clone()]);
            let current = store.alignment_synthesis_latest(scope, &write_domain)?;
            if current.as_ref().is_none_or(|r| r.version != base.version) { return Ok(None); }
            let next_version: i64 = store.conn().query_row("SELECT COALESCE(MAX(version),0)+1 FROM alignment_synthesis WHERE tenant_id=?1 AND user_id=?2 AND domain_id=?3", params![scope.tenant_id,scope.user_id,dom.write], |r| r.get(0))?;
            let cited: HashSet<_> = candidate.sentences.iter().flat_map(|s| s.refs.iter()).collect();
            let refs = serde_json::json!({"generator_version":"synthesis_v2","base_version":base.version,"sentences":candidate.sentences,"source_fingerprints":snapshot.iter().filter(|s| cited.contains(&s.reference)).map(|s| (&s.reference,&s.fingerprint)).collect::<HashMap<_,_>>()});
            store.conn().execute("INSERT INTO alignment_synthesis (tenant_id,user_id,domain_id,version,window_since,window_until,rupture_turns,user_turns,correction_free_rate,open_repair_threads,body,source_refs_json,generated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)", params![scope.tenant_id,scope.user_id,dom.write,next_version,base.window_since,base.window_until,base.rupture_turns,base.user_turns,base.correction_free_rate,base.open_repair_threads,body,refs.to_string(),now_rfc3339()?])?;
            store.ledger_record(scope, dom, "alignment", &format!("synthesis_v2:{}:{}",dom.write,base.version), 1)?;
            store.alignment_synthesis_latest(scope, &write_domain)
        })
    }

    pub fn generation_publish_artifact(&mut self, scope: &ScopeKey, dom: &DomainScope, kind: &str, signal: &str, thread: Option<&str>, snapshot: &[GenerationSource], candidate: &GeneratedCandidate) -> Result<Option<String>, StoreError> {
        if kind != "reflection_v1" { return Err(StoreError::StateConflict); }
        self.generation_transaction(|store| {
            let mut body = store.generation_validate(scope, dom, thread, snapshot, candidate, if kind == "reflection_v1" { 2 } else { 1 })?;
            if kind == "reflection_v1" { body = format!("日期：{}\n{body}", chrono::Utc::now().date_naive()); }
            let id = Uuid::now_v7().to_string();
            let cited: HashSet<_> = candidate.sentences.iter().flat_map(|s| s.refs.iter()).collect();
            let refs = serde_json::json!({"candidate":candidate,"source_fingerprints":snapshot.iter().filter(|s| cited.contains(&s.reference)).map(|s| (&s.reference,&s.fingerprint)).collect::<HashMap<_,_>>()});
            let inserted = store.conn().execute("INSERT OR IGNORE INTO generation_artifacts (id,tenant_id,user_id,domain_id,kind,signal_ref,body,sentence_refs_json,created_at,thread_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![id,scope.tenant_id,scope.user_id,dom.write,kind,signal,body,refs.to_string(),now_rfc3339()?,thread])?;
            if inserted == 0 { return Ok(None); }
            let task_kind = if signal.starts_with("reflection_v1:nightly:") { "nightly" } else if signal.starts_with("reflection_v1:quiet:") { "quiet" } else { return Err(StoreError::StateConflict) };
            store.ledger_record(scope, dom, task_kind, signal, 1)?;
            Ok(Some(id))
        })
    }

    /// Store the cited action and its proposed workflow row as one unit.
    pub fn generation_publish_repair_action(&mut self, scope: &ScopeKey, dom: &DomainScope, signal: &str, thread: &str, snapshot: &[GenerationSource], candidate: &GeneratedCandidate) -> Result<Option<String>, StoreError> {
        if candidate.sentences.len() != 2 || candidate.sentences.iter().any(|s| s.text.trim().chars().count() < 4 || !s.refs.iter().any(|r| r.starts_with("rupture:"))) {
            return Err(StoreError::InvalidRepairAction);
        }
        self.generation_transaction(|store| {
            let body = store.generation_validate(scope, dom, Some(thread), snapshot, candidate, 1)?;
            let cited: HashSet<_> = candidate.sentences.iter().flat_map(|s| s.refs.iter()).collect();
            let refs = serde_json::json!({"candidate":candidate,"source_fingerprints":snapshot.iter().filter(|s| cited.contains(&s.reference)).map(|s| (&s.reference,&s.fingerprint)).collect::<HashMap<_,_>>()});
            let id = Uuid::now_v7().to_string();
            let action_id = Uuid::now_v7().to_string();
            let now = now_rfc3339()?;
            let cited_text = |sentence: &GeneratedSentence| format!("{} {}", sentence.text.trim(), sentence.refs.iter().map(|r| format!("[{r}]")).collect::<Vec<_>>().join(" "));
            let inserted = store.conn().execute("INSERT OR IGNORE INTO generation_artifacts (id,tenant_id,user_id,domain_id,kind,signal_ref,body,sentence_refs_json,created_at,thread_id) VALUES (?1,?2,?3,?4,'repair_action_v1',?5,?6,?7,?8,?9)", params![id,scope.tenant_id,scope.user_id,dom.write,signal,body,refs.to_string(),now,thread])?;
            if inserted == 0 { return Ok(None); }
            store.conn().execute("INSERT INTO repair_actions (id,tenant_id,user_id,thread_id,action,expected_behavior,conditions,counterexamples,status,proposed_by,authorized_by,authorized_at,close_reason,recurrence_count,source_memory_id,generator_version,created_at,updated_at,generation_artifact_id) VALUES (?1,?2,?3,?4,?5,?6,'','','proposed','model',NULL,NULL,NULL,0,NULL,'repair_action_v1',?7,?7,?8)",params![action_id,scope.tenant_id,scope.user_id,thread,cited_text(&candidate.sentences[0]),cited_text(&candidate.sentences[1]),now,id])?;
            store.ledger_record(scope, dom, "repair_action", signal, 1)?;
            Ok(Some(id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memory_domain::{MemoryKind, Origin};

    fn setup() -> (Store, ScopeKey, DomainScope) {
        let migrations = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("migrations");
        let mut store = Store::open_in_memory(&migrations).unwrap();
        let dir = std::env::temp_dir().join(format!("riko-generation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join(format!("{}.token", Uuid::now_v7()));
        store.principal_add("t", "u", &token_file).unwrap();
        let token = std::fs::read_to_string(&token_file).unwrap();
        let scope = store.verify_token(token.trim()).unwrap().unwrap();
        (store, scope, DomainScope::user_main())
    }

    fn add_memory(store: &mut Store, scope: &ScopeKey, dom: &DomainScope, seq: i64, text: &str) -> String {
        let origin = Origin { host_id: "dsh".into(), agent_id: "a".into(), session_id: "s".into() };
        let evidence = match store.record_evidence(scope, &origin, seq, "user", "user", &chrono::Utc::now(), text, dom).unwrap() {
            crate::IngestOutcome::Recorded(id) | crate::IngestOutcome::AlreadyRecorded(id) => id,
        };
        match store.remember(scope, &origin, &evidence, text, MemoryKind::Fact, dom).unwrap() {
            crate::RememberOutcome::Created { memory_id, .. } => memory_id,
            other => panic!("unexpected remember outcome: {other:?}"),
        }
    }

    #[test]
    fn citations_reject_missing_and_stale_sources_and_fallback_to_template() {
        let (mut store, scope, dom) = setup();
        let a = add_memory(&mut store, &scope, &dom, 1, "我喜欢安静的回答");
        let b = add_memory(&mut store, &scope, &dom, 2, "我希望先听结论");
        let base = store.alignment_synthesis_refresh(&scope, &dom).unwrap();
        let sources = store.generation_sources(&scope, &dom, None).unwrap();
        let reference_a = format!("memory:{a}");
        let reference_b = format!("memory:{b}");
        let missing = GeneratedCandidate { sentences: vec![GeneratedSentence { text: "先给结论。".into(), refs: vec![] }] };
        assert!(store.generation_publish_synthesis(&scope, &dom, &base, &sources, &missing).is_err());
        assert_eq!(store.alignment_synthesis_latest(&scope, &dom).unwrap().unwrap().version, base.version);
        let valid = GeneratedCandidate { sentences: vec![GeneratedSentence { text: "先给结论，再用安静的语气解释。".into(), refs: vec![reference_a,reference_b] }] };
        let model = store.generation_publish_synthesis(&scope, &dom, &base, &sources, &valid).unwrap().unwrap();
        assert_eq!(model.version, base.version + 1);
        assert!(model.body.contains("[memory:"));
        // The old template job still sees its template window, not a double-counted model row.
        assert_eq!(store.alignment_synthesis_refresh(&scope, &dom).unwrap().version, base.version);
        store.conn().execute("UPDATE memories SET status='superseded' WHERE id=?1",params![a]).unwrap();
        let latest = store.alignment_synthesis_latest(&scope, &dom).unwrap().unwrap();
        assert_eq!(latest.version, base.version);
        assert!(!latest.body.contains("[memory:"));
        // A later template refresh must allocate after the model row in the shared chain.
        store.conn().execute("UPDATE alignment_synthesis SET window_until='2000-01-01T00:00:00Z' WHERE version=?1",params![base.version]).unwrap();
        let next = store.alignment_synthesis_refresh(&scope, &dom).unwrap();
        assert_eq!(next.version, model.version + 1);
    }

    #[test]
    fn sentence_shape_and_unknown_reference_fail_as_whole() {
        let (mut store, scope, dom) = setup();
        add_memory(&mut store, &scope, &dom, 1, "我喜欢短句");
        add_memory(&mut store, &scope, &dom, 2, "请先说结论");
        let sources = store.generation_sources(&scope, &dom, None).unwrap();
        for sentence in [
            GeneratedSentence { text: "先说结论。再解释。".into(), refs: sources.iter().map(|s| s.reference.clone()).collect() },
            GeneratedSentence { text: "先说结论。".into(), refs: vec!["memory:invented".into()] },
        ] {
            assert!(store.generation_validate(&scope, &dom, None, &sources, &GeneratedCandidate { sentences: vec![sentence] }, 2).is_err());
        }
        let cross_domain = DomainScope::new("side_a", vec!["user_main".into(), "side_a".into()]);
        let candidate = GeneratedCandidate { sentences: vec![GeneratedSentence { text: "先说结论。".into(), refs: sources.iter().map(|s| s.reference.clone()).collect() }] };
        assert!(store.generation_validate(&scope, &cross_domain, None, &sources, &candidate, 2).is_err());
    }

    #[test]
    fn rejected_count_and_signal_ledger_are_scoped_and_idempotent() {
        let (mut store, scope, dom) = setup();
        assert_eq!(store.generation_rejected_count(&scope, &dom, "alignment").unwrap(), 0);
        store.task_run_record(&scope, &dom, "alignment", "error", 0, "citation_invalid").unwrap();
        store.task_run_record(&scope, &dom, "alignment", "error", 0, "model_json_invalid").unwrap();
        store.task_run_record(&scope, &dom, "alignment", "ok", 1, "model_generation").unwrap();
        assert_eq!(store.generation_rejected_count(&scope, &dom, "alignment").unwrap(), 2);
        assert_eq!(store.generation_rejected_count(&scope, &dom, "quiet").unwrap(), 0);
        assert!(store.ledger_record(&scope, &dom, "alignment", "synthesis_v2:1", 1).unwrap());
        assert!(!store.ledger_record(&scope, &dom, "alignment", "synthesis_v2:1", 1).unwrap());
    }

    #[test]
    fn repair_action_is_proposed_once_with_live_thread_citation() {
        let (mut store, scope, dom) = setup();
        add_memory(&mut store, &scope, &dom, 1, "你记错了，我说的是周五十点。");
        store.rupture_scan(&scope, &dom).unwrap();
        let thread: String = store.conn().query_row("SELECT id FROM repair_threads LIMIT 1",[],|r| r.get(0)).unwrap();
        let sources = store.generation_sources(&scope, &dom, Some(&thread)).unwrap();
        let rupture = sources.iter().find(|s| s.reference.starts_with("rupture:")).unwrap().reference.clone();
        let candidate = GeneratedCandidate { sentences: vec![
            GeneratedSentence { text: "先核对用户给出的日期。".into(), refs: vec![rupture.clone()] },
            GeneratedSentence { text: "行动前复述周五十点。".into(), refs: vec![rupture] },
        ]};
        assert!(store.generation_publish_repair_action(&scope, &dom, "repair_action_v1:one", &thread, &sources, &candidate).unwrap().is_some());
        assert!(store.generation_publish_repair_action(&scope, &dom, "repair_action_v1:one", &thread, &sources, &candidate).unwrap().is_none());
        let (count,status,authorized): (i64,String,Option<String>) = store.conn().query_row(
            "SELECT COUNT(*), status, authorized_by FROM repair_actions",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((count,status.as_str(),authorized), (1,"proposed",None));
        let listed = store.repair_action_list(&scope, Some(&thread)).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].action.contains("[rupture:"));
        assert!(listed[0].expected_behavior.contains("[rupture:"));
        store.conn().execute("UPDATE repair_threads SET status='closed' WHERE id=?1",params![thread]).unwrap();
        assert!(store.repair_action_list(&scope, Some(&thread)).unwrap().is_empty());
    }
}
