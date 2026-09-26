//! D6 Dream/语义执行管线与异步向量索引 worker
//! （doc6/09 §4：exact → 词法/向量召回 → 批量裁决 → Rust 原子应用）+ 异步向量
//! 索引 worker（doc6/02 §4）。
//!
//! 单 worker 串行；模型/embedding 调用在 DB 锁之外。chat 未配置时 Dream/裁决
//! 作业停在队列；embedding 未配置时索引队列为空、向量召回不跑。embedding
//! 不可用时裁决作业保持待处理（doc6/09 §6：首版不允许仅凭 exact/词法结果把
//! 候选创建为 Active——"没搜到相似项"不能解释成"肯定是新记忆"）。

use std::sync::Arc;
use std::time::Duration;

use memory_domain::{claim_sha256, fold_whitespace, MemoryKind, ScopeKey};
use memory_extract::{ExtractError, ExtractModel};
use memory_store_sqlite::adjudication::{
    parse_adjudicate_v1, AdjudicationCandidate, AdjudicationJobRow, AdjudicationProposal,
    AdjudicationRecall, AdjudicationRedecision, ADJUDICATE_V1_PROMPT,
};
use memory_store_sqlite::dream_jobs::{
    locate_quote_span, parse_dream_extract_v1, DreamJobRow, DreamProposal, DREAM_EXTRACT_V1_PROMPT,
    DREAM_POLICY_V1,
};
use memory_store_sqlite::semantic_index::semantic_index_text;
use memory_store_sqlite::StoreError;

use crate::embedding::EmbeddingClient;
use crate::worker::{ModelConfig, OpenAiCompatibleClient};
use crate::AppState;

/// provider 退避（doc4 作业原则：5/15/45s）。
const RETRY_DELAYS_SECS: [i64; 3] = [5, 15, 45];
const LEASE_SECS: u64 = memory_contract::JOB_LEASE_SECS;
const MAX_ATTEMPTS: i64 = memory_contract::JOB_MAX_ATTEMPTS as i64;

/// 启动 Dream 管线 + 语义索引 worker。chat 未配置：Dream/裁决停队列；
/// embedding 未配置：索引不跑、裁决停队列（见模块注释）。
pub fn spawn_dream_pipeline(
    state: AppState,
    chat: Option<ModelConfig>,
    embedding: Option<Arc<EmbeddingClient>>,
    dream_enabled: bool,
) {
    let chat_client = chat.and_then(|cfg| match OpenAiCompatibleClient::new(cfg) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            eprintln!("[dream] chat 模型客户端初始化失败，Dream/裁决停队列: {e}");
            None
        }
    });
    tokio::spawn(async move {
        loop {
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
            // 心跳：Dream 启用时续租；关闭配置不得延长旧 job 的 running lease。
            if dream_enabled {
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_renew_running_leases(&now, LEASE_SECS);
            }
            // 1. Dream extract（chat 可用才领）。
            if dream_enabled {
                if let (Some(client), Some(_emb)) = (&chat_client, &embedding) {
                    let claimed = {
                        let mut g = state.store.lock().unwrap();
                        g.dream_claim_next(&now, LEASE_SECS).ok().flatten()
                    };
                    if let Some((scope, job)) = claimed {
                        process_dream_extract(&state, client.as_ref(), &scope, &job).await;
                        continue;
                    }

                    // 裁决作业只有 chat 与 embedding 都可用时才领取；否则输入与账本保持原状。
                    let claimed = {
                        let mut g = state.store.lock().unwrap();
                        g.adjudication_claim(&now, LEASE_SECS).ok().flatten()
                    };
                    if let Some((scope, job)) = claimed {
                        process_adjudication(&state, client.as_ref(), &scope, &job).await;
                        continue;
                    }
                }
            }
            // 3. 语义索引作业（embedding 可用才领）。
            if let Some(emb) = &embedding {
                let claimed = {
                    let mut g = state.store.lock().unwrap();
                    g.semantic_job_claim(&now, LEASE_SECS).ok().flatten()
                };
                if let Some((scope, job)) = claimed {
                    process_semantic_index(&state, emb, &scope, &job).await;
                    continue;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

fn retry_delay_for(attempts: i64) -> Option<i64> {
    RETRY_DELAYS_SECS
        .get((attempts - 1).clamp(0, 2) as usize)
        .copied()
}

fn kind_of(s: &str) -> Option<MemoryKind> {
    match s {
        "preference" => Some(MemoryKind::Preference),
        "instruction" => Some(MemoryKind::Instruction),
        "episode" => Some(MemoryKind::Episode),
        "fact" => Some(MemoryKind::Fact),
        _ => None,
    }
}

/// provider/transport 失败：attempts 未达上限 → provider_wait 退避；达上限 → dead。
/// 裁决作业失败同步传导到其 Dream job（证据保持 assigned，doc6/10 §5）。
async fn handle_provider_error(
    state: &AppState,
    scope: &ScopeKey,
    job: &DreamJobRow,
    e: &ExtractError,
) {
    let code = match e {
        ExtractError::Timeout => "MODEL_TIMEOUT",
        _ => "MODEL_TRANSPORT",
    };
    let mut g = state.store.lock().unwrap();
    if job.attempts >= MAX_ATTEMPTS {
        let _ = g.dream_dead(scope, &job.id, job.claim_generation, code);
    } else {
        let _ = g.dream_provider_wait(
            scope,
            &job.id,
            job.claim_generation,
            code,
            retry_delay_for(job.attempts.max(1)),
        );
    }
}

/// 阶段 A（doc6/09 §4.A/§4.B.1）：冻结输入 → 抽取模型 → 严格解析 + Rust span
/// 定位 → dream_submit_candidates → 召回计算并冻结裁决作业。
async fn process_dream_extract<M: ExtractModel>(
    state: &AppState,
    client: &M,
    scope: &ScopeKey,
    job: &DreamJobRow,
) {
    if job.purpose == "redecision" {
        match freeze_manual_redecision(state, scope, job).await {
            // 留在 running，待后续 adjudication job 成功应用后统一推进终态。
            Ok(()) => {}
            Err(FreezeError::EmbeddingUnavailable) => {
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_provider_wait(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "EMBEDDING_UNAVAILABLE",
                    retry_delay_for(job.attempts.max(1)),
                );
            }
            Err(FreezeError::StaleInput) => {
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_stale_input(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "REDECISION_SOURCE_STALE",
                );
            }
            Err(FreezeError::Store(e)) => {
                eprintln!("[dream] 重裁 {} 冻结失败: {e}", job.id);
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_dead(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "REDECISION_FREEZE_FAILED",
                );
            }
        }
        return;
    }
    // 冻结输入序列化（锁内读）。
    let user_prompt = {
        let g = state.store.lock().unwrap();
        match g.dream_frozen_inputs(scope, &job.id) {
            Ok(inputs) => {
                let items: Vec<serde_json::Value> = inputs
                    .iter()
                    .map(|(id, role, content)| {
                        serde_json::json!({"evidence_id": id, "role": role, "content": content})
                    })
                    .collect();
                serde_json::json!({"events": items}).to_string()
            }
            Err(e) => {
                eprintln!("[dream] job {} 冻结输入读取失败: {e}", job.id);
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_dead(scope, &job.id, job.claim_generation, "FROZEN_INPUT_MISSING");
                return;
            }
        }
    };
    // 模型调用（锁外）。
    let out = match client.extract(DREAM_EXTRACT_V1_PROMPT, &user_prompt).await {
        Ok(o) => o,
        Err(e) => {
            handle_provider_error(state, scope, job, &e).await;
            return;
        }
    };
    // 解析 + span 定位（Rust 核验，doc6/10 §6）。
    let proposals: Vec<DreamProposal> = match parse_dream_extract_v1(&out.content) {
        Ok(cands) => {
            let contents: std::collections::HashMap<String, String> = {
                let g = state.store.lock().unwrap();
                g.dream_frozen_inputs(scope, &job.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, _role, content)| (id, content))
                    .collect()
            };
            let mut ps = Vec::new();
            for c in cands {
                let Some(text) = contents.get(&c.evidence_id) else {
                    continue; // 非冻结输入引用 → 拒绝（不入提案）
                };
                let Some((sb, eb)) = locate_quote_span(text, &c.quote) else {
                    continue; // quote 非逐字 → 拒绝
                };
                ps.push(DreamProposal {
                    kind: c.kind,
                    claim: c.claim,
                    quote: c.quote,
                    evidence_id: c.evidence_id,
                    start_byte: sb,
                    end_byte: eb,
                    status: "candidate".into(),
                    reason_code: None,
                    occurred_at: c.occurred_at,
                });
            }
            ps
        }
        Err(_) => {
            // 坏 JSON/未知 kind = 确定性失败：dead 不重试（doc6/09 §7）。
            let mut g = state.store.lock().unwrap();
            let _ = g.dream_dead(scope, &job.id, job.claim_generation, "BAD_JSON");
            eprintln!("[dream] job {} 抽取输出坏 JSON → dead", job.id);
            return;
        }
    };
    // 提交候选（唯一键幂等；重放安全）。
    let accepted = {
        let mut g = state.store.lock().unwrap();
        match g.dream_submit_candidates(
            scope,
            &job.id,
            job.claim_generation,
            DREAM_POLICY_V1,
            &proposals,
        ) {
            Ok((a, _r)) => a,
            Err(StoreError::StaleClaim) => return,
            Err(e) => {
                eprintln!("[dream] job {} 候选提交失败: {e}", job.id);
                let _ = g.dream_dead(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "CANDIDATE_SUBMIT_FAILED",
                );
                return;
            }
        }
    };
    if accepted == 0 {
        // 无候选：直接完成（证据 processed）。
        let mut g = state.store.lock().unwrap();
        let _ = g.dream_succeed(
            scope,
            &job.id,
            job.claim_generation,
            Some("dream"),
            out.input_tokens,
            out.output_tokens,
        );
        return;
    }
    // 裁决冻结；embedding 瞬时故障 → provider_wait 稍后重试（候选唯一键幂等）。
    match freeze_adjudication(state, scope, job).await {
        Ok(()) => {}
        Err(FreezeError::EmbeddingUnavailable) => {
            if state.embedding.is_some() {
                // 配置了但此刻失败：退避重试（重新 extract；候选唯一键防重复）。
                let mut g = state.store.lock().unwrap();
                let _ = g.dream_provider_wait(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "EMBEDDING_UNAVAILABLE",
                    retry_delay_for(job.attempts.max(1)),
                );
            }
            // 未配置 embedding：裁决作业已带 NULL model 冻结，保持待处理
            // （doc6/09 §6：新 Dream 语义自动应用保持待处理并报告 disabled）。
        }
        Err(FreezeError::StaleInput) => {
            let mut g = state.store.lock().unwrap();
            let _ = g.dream_stale_input(
                scope,
                &job.id,
                job.claim_generation,
                "DREAM_SOURCE_STALE",
            );
        }
        Err(FreezeError::Store(e)) => {
            eprintln!("[dream] job {} 裁决冻结失败: {e}", job.id);
            let mut g = state.store.lock().unwrap();
            let _ = g.dream_dead(
                scope,
                &job.id,
                job.claim_generation,
                "ADJUDICATION_FREEZE_FAILED",
            );
        }
    }
    eprintln!("[dream] job {} extract 完成：候选 {accepted}", job.id);
}

enum FreezeError {
    EmbeddingUnavailable,
    StaleInput,
    Store(StoreError),
}

/// 显式重裁只消费 trigger 时冻结的 Held ID 与证据 span；不重新抽取、不创建
/// second candidate，也不改变原 evidence 的 processed/pending 水位。
async fn freeze_manual_redecision(
    state: &AppState,
    scope: &ScopeKey,
    dream_job: &DreamJobRow,
) -> Result<(), FreezeError> {
    let (candidates, records) = {
        let g = state.store.lock().unwrap();
        (
            g.dream_redecision_candidates(scope, &dream_job.id)
                .map_err(FreezeError::Store)?,
            g.dream_redecision_records(scope, &dream_job.id)
                .map_err(FreezeError::Store)?,
        )
    };
    if candidates.is_empty() || records.is_empty() {
        return Err(FreezeError::StaleInput);
    }
    let embedding = state
        .embedding
        .clone()
        .ok_or(FreezeError::EmbeddingUnavailable)?;
    let claims: Vec<String> = candidates.iter().map(|(c, _)| c.claim.clone()).collect();
    let vectors = embedding.embed(&claims).await.map_err(|e| {
        eprintln!("[dream] 显式重裁 embedding 不可用: {e}");
        FreezeError::EmbeddingUnavailable
    })?;
    let mut inputs = Vec::new();
    for (candidate, spans) in &candidates {
        for (evidence_id, start_byte, end_byte) in spans {
            inputs.push(AdjudicationCandidate {
                candidate_id: candidate.candidate_id.clone(),
                kind: candidate.kind.clone(),
                claim: candidate.claim.clone(),
                quote: candidate.quote.clone(),
                status: "held".into(),
                evidence_id: evidence_id.clone(),
                start_byte: *start_byte,
                end_byte: *end_byte,
            });
        }
    }
    let redecisions: Vec<AdjudicationRedecision> = records
        .iter()
        .map(|r| AdjudicationRedecision {
            candidate_id: r.candidate_id.clone(),
            evidence_id: r.evidence_id.clone(),
            redecision_kind: r.redecision_kind.clone(),
            strategy_fingerprint: r.strategy_fingerprint.clone(),
        })
        .collect();
    let k = memory_contract::ADJUDICATE_RECALL_TOP_K;
    let mut recalls = Vec::new();
    {
        let g = state.store.lock().unwrap();
        for (index, (candidate, _)) in candidates.iter().enumerate() {
            let Some(kind) = kind_of(&candidate.kind) else {
                continue;
            };
            let hash = claim_sha256(kind, &fold_whitespace(&candidate.claim));
            for (id, _) in g
                .memories_by_exact_hash(scope, kind.as_str(), &hash, k)
                .map_err(FreezeError::Store)?
            {
                if let Some(memory) = g.get_memory(scope, &id).map_err(FreezeError::Store)? {
                    recalls.push(AdjudicationRecall {
                        candidate_id: candidate.candidate_id.clone(),
                        target_memory_id: id,
                        target_version: memory.version,
                        channel: "exact".into(),
                    });
                }
            }
            if let Ok((hits, _)) = g.search_memories(scope, &candidate.claim, k, false) {
                recalls.extend(hits.into_iter().map(|hit| AdjudicationRecall {
                    candidate_id: candidate.candidate_id.clone(),
                    target_memory_id: hit.memory_id,
                    target_version: hit.version,
                    channel: "lexical".into(),
                }));
            }
            if let Some(vector) = vectors.get(index) {
                if let Ok((hits, _)) = g.semantic_scan(scope, "memory", embedding.model_id(), vector, k) {
                    for (id, _) in hits {
                        if let Some(memory) = g.get_memory(scope, &id).map_err(FreezeError::Store)? {
                            recalls.push(AdjudicationRecall {
                                candidate_id: candidate.candidate_id.clone(),
                                target_memory_id: id,
                                target_version: memory.version,
                                channel: "semantic".into(),
                            });
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    recalls.retain(|r| seen.insert((r.candidate_id.clone(), r.target_memory_id.clone())));
    let mut g = state.store.lock().unwrap();
    g.adjudication_create_with_redecisions(
        scope,
        &dream_job.id,
        memory_contract::ADMISSION_VERSION_V3,
        memory_store_sqlite::adjudication::ADJUDICATE_V1,
        Some(embedding.model_id()),
        &inputs,
        &recalls,
        &redecisions,
    )
    .map_err(FreezeError::Store)?
    .ok_or(FreezeError::StaleInput)?;
    Ok(())
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm = left.iter().map(|v| v * v).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !dot.is_finite() || left_norm <= 0.0 || right_norm <= 0.0 {
        return None;
    }
    let score = dot / (left_norm * right_norm);
    score.is_finite().then_some(score)
}

/// 召回计算与裁决作业冻结（doc6/09 §4.B.1/§4.B.2）：exact → 词法 top-K →
/// 向量 top-K；旧 Held 仅在新证据 claim cosine 达到冻结阈值时加入本批；
/// 每个 (candidate,evidence,strategy) 的重裁关系与 job 输入原子持久化。
async fn freeze_adjudication(
    state: &AppState,
    scope: &ScopeKey,
    dream_job: &DreamJobRow,
) -> Result<(), FreezeError> {
    let (accepted, held) = {
        let g = state.store.lock().unwrap();
        (
            g.dream_accepted_candidates(scope, &dream_job.id)
                .map_err(FreezeError::Store)?,
            g.dream_held_candidates(scope)
                .map_err(FreezeError::Store)?,
        )
    };
    if accepted.is_empty() {
        return Ok(());
    }
    // 裁决输入候选：每 (候选, evidence span) 一条。
    let mut cand_inputs: Vec<AdjudicationCandidate> = Vec::new();
    for (c, spans) in &accepted {
        for (eid, sb, eb) in spans {
            cand_inputs.push(AdjudicationCandidate {
                candidate_id: c.candidate_id.clone(),
                kind: c.kind.clone(),
                claim: c.claim.clone(),
                quote: c.quote.clone(),
                status: c.status.clone(),
                evidence_id: eid.clone(),
                start_byte: *sb,
                end_byte: *eb,
            });
        }
    }
    let emb = state
        .embedding
        .clone()
        .ok_or(FreezeError::EmbeddingUnavailable)?;
    let strategy_fingerprint = format!(
        "{}:{}:{}",
        memory_contract::ADMISSION_VERSION_V3,
        memory_store_sqlite::adjudication::ADJUDICATE_V1,
        emb.model_id()
    );
    let mut redecisions: Vec<AdjudicationRedecision> = Vec::new();
    let mut held_inputs = Vec::new();
    let mut held_similarity_vectors: Option<Vec<Vec<f32>>> = None;
    if !held.is_empty() {
        let mut texts: Vec<String> = accepted.iter().map(|(c, _)| c.claim.clone()).collect();
        texts.extend(held.iter().map(|(c, _)| c.claim.clone()));
        let vectors = emb.embed(&texts).await.map_err(|e| {
            eprintln!("[dream] Held 重裁相关性 embedding 不可用: {e}");
            FreezeError::EmbeddingUnavailable
        })?;
        let fresh_count = accepted.len();
        let mut linked_pairs = std::collections::HashSet::new();
        for (held_index, (held_candidate, held_spans)) in held.iter().enumerate() {
            let mut include_held = false;
            for (fresh_index, (_fresh_candidate, fresh_spans)) in accepted.iter().enumerate() {
                let related = cosine_similarity(
                    &vectors[fresh_index],
                    &vectors[fresh_count + held_index],
                )
                .is_some_and(|score| score >= memory_contract::HELD_REDECISION_MIN_COSINE);
                if !related {
                    continue;
                }
                for (evidence_id, _, _) in fresh_spans {
                    if !linked_pairs.insert((held_candidate.candidate_id.clone(), evidence_id.clone())) {
                        continue;
                    }
                    let already_seen = {
                        let g = state.store.lock().unwrap();
                        g.dream_candidate_redecision_seen(
                            scope,
                            &held_candidate.candidate_id,
                            evidence_id,
                            &strategy_fingerprint,
                        )
                        .map_err(FreezeError::Store)?
                    };
                    if already_seen {
                        continue;
                    }
                    include_held = true;
                    redecisions.push(AdjudicationRedecision {
                        candidate_id: held_candidate.candidate_id.clone(),
                        evidence_id: evidence_id.clone(),
                        redecision_kind: "related_evidence".into(),
                        strategy_fingerprint: strategy_fingerprint.clone(),
                    });
                }
            }
            if include_held {
                for (evidence_id, start_byte, end_byte) in held_spans {
                    held_inputs.push(AdjudicationCandidate {
                        candidate_id: held_candidate.candidate_id.clone(),
                        kind: held_candidate.kind.clone(),
                        claim: held_candidate.claim.clone(),
                        quote: held_candidate.quote.clone(),
                        status: "held".into(),
                        evidence_id: evidence_id.clone(),
                        start_byte: *start_byte,
                        end_byte: *end_byte,
                    });
                }
            }
        }
        held_similarity_vectors = Some(vectors);
        cand_inputs.extend(held_inputs);
    }
    let k = memory_contract::ADJUDICATE_RECALL_TOP_K;
    let mut recalls: Vec<AdjudicationRecall> = Vec::new();
    // exact 快速路径（scope 内同 kind+hash 的 active 记忆；doc6/09 §4.B.1）。
    {
        let g = state.store.lock().unwrap();
        for (c, _) in &accepted {
            let Some(kind) = kind_of(&c.kind) else {
                continue;
            };
            let hash = claim_sha256(kind, &fold_whitespace(&c.claim));
            let hits = g
                .memories_by_exact_hash(scope, kind.as_str(), &hash, k)
                .map_err(FreezeError::Store)?;
            for (mid, _v) in hits {
                if let Some(v) = g.get_memory(scope, &mid).map_err(FreezeError::Store)? {
                    recalls.push(AdjudicationRecall {
                        candidate_id: c.candidate_id.clone(),
                        target_memory_id: mid.clone(),
                        target_version: v.version,
                        channel: "exact".into(),
                    });
                }
            }
        }
    }
    // 向量召回：批量 embed 候选 claim（锁外）。配置了但失败 → EmbeddingUnavailable。
    let query_vecs = if let Some(vectors) = held_similarity_vectors {
        vectors.into_iter().take(accepted.len()).collect::<Vec<_>>()
    } else {
        let texts: Vec<String> = accepted.iter().map(|(c, _)| c.claim.clone()).collect();
        emb.embed(&texts).await.map_err(|e| {
            eprintln!("[dream] 候选向量召回不可用: {e}");
            FreezeError::EmbeddingUnavailable
        })?
    };
    // 词法 + 向量逐候选召回（锁内）。
    {
        let g = state.store.lock().unwrap();
        for (i, (c, _)) in accepted.iter().enumerate() {
            if let Ok((hits, _)) = g.search_memories(scope, &c.claim, k, false) {
                for h in hits.iter().take(k) {
                    recalls.push(AdjudicationRecall {
                        candidate_id: c.candidate_id.clone(),
                        target_memory_id: h.memory_id.clone(),
                        target_version: h.version,
                        channel: "lexical".into(),
                    });
                }
            }
            if let Some(qv) = query_vecs.get(i) {
                if let Ok((vhits, _n)) =
                    g.semantic_scan(scope, "memory", emb.model_id(), qv, k)
                {
                    for (mid, _sim) in vhits {
                        if let Some(v) = g.get_memory(scope, &mid).map_err(FreezeError::Store)? {
                            recalls.push(AdjudicationRecall {
                                candidate_id: c.candidate_id.clone(),
                                target_memory_id: mid.clone(),
                                target_version: v.version,
                                channel: "semantic".into(),
                            });
                        }
                    }
                }
            }
        }
    }
    // 去重：候选+target 唯一，保留第一条（优先级 exact > lexical > semantic 由插入序决定）。
    let mut seen = std::collections::HashSet::new();
    recalls.retain(|r| seen.insert((r.candidate_id.clone(), r.target_memory_id.clone())));
    // 冻结（embedding_model_id 仅在向量召回实际可用时记录）。
    let mut g = state.store.lock().unwrap();
    g.adjudication_create_with_redecisions(
        scope,
        &dream_job.id,
        memory_contract::ADMISSION_VERSION_V3,
        memory_store_sqlite::adjudication::ADJUDICATE_V1,
        Some(emb.model_id()),
        &cand_inputs,
        &recalls,
        &redecisions,
    )
    .map_err(FreezeError::Store)?;
    Ok(())
}

/// 阶段 B（doc6/09 §4.B.3/§4.B.4）：裁决作业 → prompt（候选+冻结召回 target）→
/// 批量裁决 → Rust 原子应用 → 终态传播到 Dream job；应用的 L1 异步入向量索引。
async fn process_adjudication<M: ExtractModel>(
    state: &AppState,
    client: &M,
    scope: &ScopeKey,
    job: &AdjudicationJobRow,
) {
    // 冻结输入 + 召回 target 上下文（锁内读；读取时复核 target 同 scope/active）。
    let (cands, recalls) = {
        let g = state.store.lock().unwrap();
        g.adjudication_inputs(scope, &job.id).unwrap_or_default()
    };
    if cands.is_empty() {
        // 冻结输入为空（不应发生）：按成功收尾并把 Dream job 完结，避免悬挂。
        let mut g = state.store.lock().unwrap();
        let dream_gen = g
            .dream_get(scope, &job.dream_job_id)
            .ok()
            .flatten()
            .map(|j| j.claim_generation)
            .unwrap_or(-1);
        let _ = g.adjudication_finish(
            scope,
            &job.id,
            job.claim_generation,
            "succeeded",
            Some("NO_INPUTS"),
            None,
            None,
            None,
            None,
        );
        let _ = g.dream_succeed(scope, &job.dream_job_id, dream_gen, None, None, None);
        return;
    }
    let dream_generation = {
        let g = state.store.lock().unwrap();
        match g.dream_get(scope, &job.dream_job_id) {
            Ok(Some(parent)) => parent.claim_generation,
            _ => return,
        }
    };
    let mut targets_json: Vec<serde_json::Value> = Vec::new();
    let mut target_scope_ok: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let g = state.store.lock().unwrap();
        for r in &recalls {
            if let Some(m) = g.get_memory(scope, &r.target_memory_id).unwrap_or(None) {
                target_scope_ok.insert(r.target_memory_id.clone());
                targets_json.push(serde_json::json!({
                    "candidate_id": r.candidate_id,
                    "target_memory_id": r.target_memory_id,
                    "kind": m.kind,
                    "claim": m.claim,
                    "version": m.version,
                }));
            }
        }
    }
    let user_prompt = serde_json::json!({
        "candidates": cands.iter().map(|c| serde_json::json!({
            "candidate_id": c.candidate_id,
            "kind": c.kind,
            "claim": c.claim,
            "quote": c.quote,
        })).collect::<Vec<_>>(),
        "recalled_targets": targets_json,
    })
    .to_string();
    // 模型调用（锁外）。
    let out = match client.extract(ADJUDICATE_V1_PROMPT, &user_prompt).await {
        Ok(o) => o,
        Err(e) => {
            let code = match e {
                ExtractError::Timeout => "MODEL_TIMEOUT",
                _ => "MODEL_TRANSPORT",
            };
            let mut g = state.store.lock().unwrap();
            if job.attempts >= MAX_ATTEMPTS {
                let _ = g.adjudication_finish(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "dead",
                    Some(code),
                    None,
                    None,
                    None,
                    None,
                );
                let _ = g.dream_dead(scope, &job.dream_job_id, dream_generation, code);
            } else {
                let _ = g.adjudication_finish(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "provider_wait",
                    Some(code),
                    None,
                    None,
                    None,
                    retry_delay_for(job.attempts.max(1)),
                );
                let _ =
                    g.dream_provider_wait(scope, &job.dream_job_id, dream_generation, code, None);
            }
            eprintln!("[adjudicate] job {} 模型失败: {code}", job.id);
            return;
        }
    };
    let items = match parse_adjudicate_v1(&out.content) {
        Ok(v) => v,
        Err(_) => {
            // 坏 JSON = 确定性失败：dead，不重复调用同一坏输出（doc6/09 §7）。
            let mut g = state.store.lock().unwrap();
            let _ = g.adjudication_finish(
                scope,
                &job.id,
                job.claim_generation,
                "dead",
                Some("BAD_JSON"),
                None,
                None,
                None,
                None,
            );
            let _ = g.dream_dead(
                scope,
                &job.dream_job_id,
                dream_generation,
                "ADJUDICATION_BAD_JSON",
            );
            eprintln!("[adjudicate] job {} 输出坏 JSON → dead", job.id);
            return;
        }
    };
    let proposals: Vec<AdjudicationProposal> = items
        .into_iter()
        .map(|i| AdjudicationProposal {
            candidate_id: i.candidate_id,
            durability: i.durability,
            action: i.action,
            reason_code: i.reason_code,
            target_memory_id: i.target_memory_id,
            expected_target_version: i.expected_target_version,
            model_confidence: i.model_confidence,
            valid_until: i.valid_until,
        })
        .collect();
    // Rust 原子应用；父 Dream job generation 在下方按数据库当前值复核。
    let applied_ids: Vec<String>;
    let apply_result = {
        let mut g = state.store.lock().unwrap();
        // 取 dream job 当前 generation（裁决续作时 dream 行可能已是 provider_wait）。
        let dream_gen = g
            .dream_get(scope, &job.dream_job_id)
            .ok()
            .flatten()
            .map(|j| j.claim_generation)
            .unwrap_or(-1);
        match g.adjudication_apply(scope, &job.id, job.claim_generation, &proposals) {
            Ok(outcome) => {
                applied_ids = outcome
                    .rows
                    .iter()
                    .filter(|(_, st, _, _)| st == "applied")
                    .filter_map(|(_, _, mid, _)| mid.clone())
                    .collect();
                Ok((dream_gen, outcome.applied, outcome.rejected, outcome.held))
            }
            Err(StoreError::StaleInput) => {
                // 任一召回 target 在模型运行中变化：整批 stale，不部分提交。
                let _ = g.adjudication_finish(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "stale_input",
                    Some("INPUT_DRIFT"),
                    None,
                    None,
                    None,
                    None,
                );
                let _ = g.dream_stale_input(
                    scope,
                    &job.dream_job_id,
                    dream_gen,
                    "ADJUDICATION_INPUT_DRIFT",
                );
                eprintln!("[adjudicate] job {} 输入漂移 → stale_input", job.id);
                return;
            }
            Err(e) => {
                let _ = g.adjudication_finish(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "dead",
                    Some("APPLY_FAILED"),
                    None,
                    None,
                    None,
                    None,
                );
                let _ = g.dream_dead(
                    scope,
                    &job.dream_job_id,
                    dream_gen,
                    "ADJUDICATION_APPLY_FAILED",
                );
                eprintln!("[adjudicate] job {} 应用失败: {e}", job.id);
                return;
            }
        }
    };
    let (_dream_gen, applied, rejected, held) = match apply_result {
        Ok(v) => v,
        Err(()) => return,
    };
    // 裁决成功：Dream job 完成（证据 processed，含 not_memory/defer；doc6/09 §7）。
    {
        let mut g = state.store.lock().unwrap();
        let dream_gen = g
            .dream_get(scope, &job.dream_job_id)
            .ok()
            .flatten()
            .map(|j| j.claim_generation)
            .unwrap_or(-1);
        let _ = g.adjudication_finish(
            scope,
            &job.id,
            job.claim_generation,
            "succeeded",
            None,
            Some("dream"),
            out.input_tokens,
            out.output_tokens,
            None,
        );
        let _ = g.dream_succeed(
            scope,
            &job.dream_job_id,
            dream_gen,
            Some("dream"),
            None,
            None,
        );
    }
    // 应用成功的 L1 异步入向量索引（索引失败不回滚证据，doc6/09 §6）。
    if let Some(emb) = &state.embedding {
        let model_id = emb.model_id().to_string();
        let mut g = state.store.lock().unwrap();
        for mid in &applied_ids {
            let _ = g.semantic_enqueue(scope, "memory", mid, &model_id);
        }
    }
    eprintln!(
        "[adjudicate] job {} 完成：applied={applied} rejected={rejected} held={held}",
        job.id
    );
}

/// 异步向量索引（doc6/02 §4）：读对象文本（核版本/哈希）→ embed → 存向量。
async fn process_semantic_index(
    state: &AppState,
    emb: &EmbeddingClient,
    scope: &ScopeKey,
    job: &memory_store_sqlite::semantic_index::SemanticJobRow,
) {
    // 读对象文本（版本/哈希由 save 再核）。
    let text = {
        let mut g = state.store.lock().unwrap();
        match job.object_kind.as_str() {
            "memory" => match g.get_memory(scope, &job.object_id) {
                Ok(Some(m)) => semantic_index_text("memory", None, &m.claim),
                _ => {
                    let _ = g.semantic_job_finish(
                        scope,
                        &job.id,
                        job.claim_generation,
                        "stale_input",
                        Some("OBJECT_GONE"),
                        None,
                    );
                    return;
                }
            },
            "page" => match g.get_page(scope, &job.object_id, &now_rfc()) {
                Ok(Some(p)) => semantic_index_text("page", Some(&p.title), &p.body_md),
                _ => {
                    let _ = g.semantic_job_finish(
                        scope,
                        &job.id,
                        job.claim_generation,
                        "stale_input",
                        Some("OBJECT_GONE"),
                        None,
                    );
                    return;
                }
            },
            _ => {
                let _ = g.semantic_job_finish(
                    scope,
                    &job.id,
                    job.claim_generation,
                    "dead",
                    Some("BAD_KIND"),
                    None,
                );
                return;
            }
        }
    };
    // embedding（锁外）。
    let vecs = match emb.embed(&[text]).await {
        Ok(v) => v,
        Err(e) => {
            let (status, code) = match e {
                crate::embedding::EmbeddingError::Timeout => ("provider_wait", "EMBED_TIMEOUT"),
                crate::embedding::EmbeddingError::RateLimited => ("provider_wait", "EMBED_429"),
                _ => ("retryable_failed", "EMBED_FAILED"),
            };
            let mut g = state.store.lock().unwrap();
            let delay = retry_delay_for(job.attempts.max(1));
            let status = if job.attempts >= MAX_ATTEMPTS {
                "dead"
            } else {
                status
            };
            let _ = g.semantic_job_finish(
                scope,
                &job.id,
                job.claim_generation,
                status,
                Some(code),
                delay,
            );
            eprintln!("[semantic] job {} embed 失败: {e}", job.id);
            return;
        }
    };
    let Some(v) = vecs.into_iter().next() else {
        return;
    };
    // 保存（核冻结版本/哈希；维度不符已在客户端核过）。
    let mut g = state.store.lock().unwrap();
    match g.semantic_vector_save(
        scope,
        &job.object_kind,
        &job.object_id,
        &job.model_id,
        job.source_version,
        &job.content_sha256,
        &v,
    ) {
        Ok(()) => {
            let _ = g.semantic_job_finish(
                scope,
                &job.id,
                job.claim_generation,
                "succeeded",
                None,
                None,
            );
        }
        Err(StoreError::StaleInput) => {
            let _ = g.semantic_job_finish(
                scope,
                &job.id,
                job.claim_generation,
                "stale_input",
                Some("VERSION_DRIFT"),
                None,
            );
        }
        Err(e) => {
            eprintln!("[semantic] job {} 保存失败: {e}", job.id);
            let _ = g.semantic_job_finish(
                scope,
                &job.id,
                job.claim_generation,
                "dead",
                Some("SAVE_FAILED"),
                None,
            );
        }
    }
}

fn now_rfc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
