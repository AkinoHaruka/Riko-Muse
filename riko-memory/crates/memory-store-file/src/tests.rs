use super::*;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn fixture() -> (TempDir, FileStore, ScopeKey, Origin) {
    let temp = TempDir::new().unwrap();
    let store = FileStore::open(temp.path()).unwrap();
    store
        .principal_add_with_token("tenant", "user", "secret-token")
        .unwrap();
    let scope = store.verify_token("secret-token").unwrap().unwrap();
    let origin = Origin {
        host_id: "host".into(),
        agent_id: "agent/../../unsafe".into(),
        session_id: "session".into(),
    };
    (temp, store, scope, origin)
}

fn at(second: u32) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&format!("2026-10-23T12:00:{second:02}Z"))
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn round_trip_evidence_claim_and_soul_preserves_body_bytes() {
    let (_temp, store, scope, origin) = fixture();
    let body = "偏好：保留  双空格\r\n与分解字符 e\u{301}";
    let evidence_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), body)
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        store
            .get_evidence(&scope, &evidence_id)
            .unwrap()
            .unwrap()
            .payload,
        body
    );
    let claim_id = match store
        .remember(&scope, &origin, &evidence_id, body, MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        store.get_claim(&scope, &claim_id).unwrap().unwrap().body,
        body
    );
    let soul = "第一行\r\n第二行 e\u{301}";
    assert_eq!(
        store
            .upsert_soul(&scope, &origin.agent_id, soul, 0, "user_api")
            .unwrap(),
        SoulUpsertOutcome::Created { version: 1 }
    );
    assert_eq!(
        store
            .get_soul(&scope, &origin.agent_id)
            .unwrap()
            .unwrap()
            .body,
        soul
    );
    let overview = store.scope_dir(&scope).unwrap().join("MEMORY.md");
    let parsed =
        memory_file_format::overview::MemoryOverview::parse(&fs::read(overview).unwrap()).unwrap();
    assert_eq!(parsed.curated.len(), 1);
    assert_eq!(parsed.curated[0].memory_id, claim_id);
}

#[test]
fn content_fingerprint_deduplicates_evidence_and_claims() {
    let (_temp, store, scope, origin) = fixture();
    let first = store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "same")
        .unwrap();
    let id = match first {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    assert_eq!(
        store
            .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "same")
            .unwrap(),
        IngestOutcome::AlreadyRecorded(id.clone())
    );
    let created = store
        .remember(&scope, &origin, &id, "same", MemoryKind::Fact)
        .unwrap();
    let claim_id = match created {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    assert_eq!(
        store
            .remember(&scope, &origin, &id, "same", MemoryKind::Fact)
            .unwrap(),
        RememberOutcome::Dedup {
            memory_id: claim_id,
            version: 1
        }
    );
    assert_eq!(
        store
            .all_claims_dir(&store.scope_dir(&scope).unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn supersede_updates_old_and_links_new() {
    let (_temp, store, scope, origin) = fixture();
    let old_evidence = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "旧偏好")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let old_id = match store
        .remember(
            &scope,
            &origin,
            &old_evidence,
            "旧偏好",
            MemoryKind::Preference,
        )
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let new_evidence = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "新偏好")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let new_id = match store
        .supersede(
            &scope,
            &old_id,
            &origin,
            &new_evidence,
            "新偏好",
            MemoryKind::Preference,
        )
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let old = store.get_claim(&scope, &old_id).unwrap().unwrap();
    let new = store.get_claim(&scope, &new_id).unwrap().unwrap();
    assert_eq!(old.front.status, "superseded");
    assert_eq!(old.front.version, 2);
    assert_eq!(new.front.supersedes.as_deref(), Some(old_id.as_str()));
    assert_eq!(new.front.status, "active");
}

#[test]
fn forget_suppresses_reads_and_rewrites() {
    let (_temp, store, scope, origin) = fixture();
    let evidence_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "忘掉这句")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let memory_id = match store
        .remember(&scope, &origin, &evidence_id, "忘掉这句", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    store.forget_claim(&scope, &memory_id).unwrap();
    assert!(store.get_claim(&scope, &memory_id).unwrap().is_none());
    assert!(matches!(
        store.record_evidence(&scope, &origin, 2, "user", "user", &at(2), "忘掉这句"),
        Err(StoreError::Suppressed)
    ));
}

#[test]
fn concurrent_scope_writers_do_not_lose_confirmed_writes() {
    let (_temp, store, scope, origin) = fixture();
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(17));
    let mut joins = Vec::new();
    for i in 0..16 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let scope = scope.clone();
        let origin = origin.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            store
                .record_evidence(
                    &scope,
                    &origin,
                    i + 1,
                    "user",
                    "user",
                    &at((i + 1) as u32),
                    &format!("message-{i}"),
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for join in joins {
        join.join().unwrap();
    }
    assert_eq!(
        store
            .list_daily_evidence(&scope, "2026-10-23")
            .unwrap()
            .len(),
        16
    );
}

#[test]
fn token_is_only_stored_as_digest_and_agent_path_is_encoded() {
    let (_temp, store, scope, origin) = fixture();
    let tree = walk_files(store.root());
    assert!(!tree
        .iter()
        .any(|(_, bytes)| bytes.windows(12).any(|w| w == b"secret-token")));
    store
        .upsert_soul(&scope, &origin.agent_id, "soul", 0, "user_cli")
        .unwrap();
    let scope_dir = store.scope_dir(&scope).unwrap();
    assert!(!scope_dir.join("agents").join(&origin.agent_id).exists());
    assert!(soul_dir(&scope_dir, &origin.agent_id)
        .unwrap()
        .join("SOUL.md")
        .exists());
}

#[test]
fn scopes_are_isolated_without_user_controlled_path_components() {
    let (_temp, store, scope, origin) = fixture();
    store
        .principal_add_with_token("../other-tenant", "../../other-user", "other-token")
        .unwrap();
    let other = store.verify_token("other-token").unwrap().unwrap();
    let id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "private")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    assert!(store.get_evidence(&other, &id).unwrap().is_none());
    assert!(!store.root().join("other-tenant").exists());
    assert!(!store.root().join("other-user").exists());
}

#[test]
fn direct_layout_contains_none_of_the_cut_protocol_layers() {
    let (_temp, store, scope, _origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();
    for cut in [
        "CURRENT",
        "generations",
        "staging",
        "MANIFEST.json",
        "receipts",
        "fences",
        "domains",
    ] {
        assert!(!scope_dir.join(cut).exists(), "cut protocol reappeared: {cut}");
    }
    assert!(scope_dir.join("LOCK").is_file());
    assert!(scope_dir.join("memory/claims").is_dir());
    assert!(scope_dir.join("agents").is_dir());
}

#[cfg(unix)]
#[test]
fn rejects_symlink_jump() {
    use std::os::unix::fs::symlink;
    let (_temp, store, scope, _origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();
    let claims = scope_dir.join("memory/claims");
    fs::remove_dir(&claims).unwrap();
    symlink("/tmp", &claims).unwrap();
    assert!(matches!(
        store.all_claims_dir(&scope_dir),
        Err(StoreError::UnsafePath(_))
    ));
}

#[test]
fn soul_history_and_version_checks_are_direct_files() {
    let (_temp, store, scope, _origin) = fixture();
    assert_eq!(
        store
            .upsert_soul(&scope, "a", "one", 0, "user_cli")
            .unwrap(),
        SoulUpsertOutcome::Created { version: 1 }
    );
    assert_eq!(
        store
            .upsert_soul(&scope, "a", "two", 1, "user_cli")
            .unwrap(),
        SoulUpsertOutcome::Updated { version: 2 }
    );
    assert_eq!(
        store
            .get_soul_revision(&scope, "a", 1)
            .unwrap()
            .unwrap()
            .body,
        "one"
    );
    assert_eq!(store.get_soul(&scope, "a").unwrap().unwrap().body, "two");
}

#[test]
fn atomic_replace_failure_before_rename_keeps_old_target() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("target");
    fs::write(&target, b"old").unwrap();
    let result = atomic_replace_with(&target, b"new", || {
        Err(StoreError::Metadata("injected stop".into()))
    });
    assert!(result.is_err());
    assert_eq!(fs::read(target).unwrap(), b"old");
}

/// Child entry selected by `atomic_replace_survives_process_kill`.
#[test]
fn atomic_kill_child() {
    let Ok(root) = std::env::var("RIKO_ATOMIC_KILL_CHILD") else {
        return;
    };
    let root = PathBuf::from(root);
    let target = root.join("target");
    let marker = root.join("ready");
    let _ = atomic_replace_with(&target, b"new", || {
        fs::write(&marker, b"ready")?;
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(unix)]
#[test]
fn atomic_replace_survives_process_kill() {
    use std::os::unix::process::ExitStatusExt;
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("target"), b"old").unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("tests::atomic_kill_child")
        .arg("--nocapture")
        .env("RIKO_ATOMIC_KILL_CHILD", temp.path())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if temp.path().join("ready").exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(temp.path().join("ready").exists());
    unsafe { libc_kill(child.id() as i32, 9) };
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    assert_eq!(fs::read(temp.path().join("target")).unwrap(), b"old");
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, signal)
}

fn walk_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                out.push((entry.path(), fs::read(entry.path()).unwrap()));
            }
        }
    }
    out
}

#[test]
fn forget_claim_is_idempotent_and_does_not_bump_version_again() {
    let (_temp, store, scope, origin) = fixture();
    let evidence_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "待遗忘偏好")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let memory_id = match store
        .remember(&scope, &origin, &evidence_id, "待遗忘偏好", MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    store.forget_claim(&scope, &memory_id).unwrap();
    let path = claim_path(&store.scope_dir(&scope).unwrap(), &memory_id).unwrap();
    let claim_v1 = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(claim_v1.front.status, "forgotten");
    assert_eq!(claim_v1.front.version, 2);

    // Second call must be idempotent and not bump version again
    store.forget_claim(&scope, &memory_id).unwrap();
    let claim_v2 = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(claim_v2.front.status, "forgotten");
    assert_eq!(claim_v2.front.version, 2);
}

#[test]
fn get_overview_and_list_claims_reflect_curated_and_filter_forgotten() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "第一条偏好")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let id1 = match store
        .remember(&scope, &origin, &ev1, "第一条偏好", MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "第二条事实")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let id2 = match store
        .remember(&scope, &origin, &ev2, "第二条事实", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // get_overview reads MEMORY.md
    let overview = store.get_overview(&scope).unwrap().unwrap();
    assert_eq!(overview.curated.len(), 2);
    assert_eq!(overview.curated[0].memory_id, id1);
    assert_eq!(overview.curated[1].memory_id, id2);

    // list_claims lists both
    let claims = store.list_claims(&scope).unwrap();
    assert_eq!(claims.len(), 2);

    // Now forget id1
    store.forget_claim(&scope, &id1).unwrap();

    let overview_after = store.get_overview(&scope).unwrap().unwrap();
    assert_eq!(overview_after.curated.len(), 1);
    assert_eq!(overview_after.curated[0].memory_id, id2);

    let claims_after = store.list_claims(&scope).unwrap();
    assert_eq!(claims_after.len(), 1);
    assert_eq!(claims_after[0].front.id, id2);
}

#[test]
fn suppressed_evidence_cannot_be_remembered() {
    let (_temp, store, scope, origin) = fixture();
    let ev_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "秘密信息")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let mem_id = match store
        .remember(&scope, &origin, &ev_id, "秘密信息", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    store.forget_claim(&scope, &mem_id).unwrap();

    // Now try to remember again from the same evidence
    assert!(matches!(
        store.remember(&scope, &origin, &ev_id, "秘密信息", MemoryKind::Fact),
        Err(StoreError::Suppressed)
    ));
}

#[test]
fn supersede_rejects_already_superseded_or_forgotten_target() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "旧城市：北京")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let old_id = match store
        .remember(&scope, &origin, &ev1, "旧城市：北京", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "新城市：上海")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let new_id = match store
        .supersede(&scope, &old_id, &origin, &ev2, "新城市：上海", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    let ev3 = match store
        .record_evidence(&scope, &origin, 3, "user", "user", &at(3), "第三城市：广州")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };

    // Superseding an already superseded claim must fail with VersionConflict
    assert!(matches!(
        store.supersede(&scope, &old_id, &origin, &ev3, "第三城市：广州", MemoryKind::Fact),
        Err(StoreError::VersionConflict)
    ));

    // Forgetting new_id then attempting to supersede it must fail with VersionConflict
    store.forget_claim(&scope, &new_id).unwrap();
    let ev4 = match store
        .record_evidence(&scope, &origin, 4, "user", "user", &at(4), "第四城市：深圳")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    assert!(matches!(
        store.supersede(&scope, &new_id, &origin, &ev4, "第四城市：深圳", MemoryKind::Fact),
        Err(StoreError::VersionConflict)
    ));
}

#[test]
fn concurrent_mixed_operations_do_not_corrupt_scope() {
    let (_temp, store, scope, origin) = fixture();
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(9));
    let mut joins = Vec::new();

    // 4 threads writing evidence
    for i in 0..4 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let scope = scope.clone();
        let origin = origin.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            store
                .record_evidence(
                    &scope,
                    &origin,
                    i + 1,
                    "user",
                    "user",
                    &at((i + 1) as u32),
                    &format!("mixed-ev-{i}"),
                )
                .unwrap();
        }));
    }

    // 4 threads updating soul
    for i in 0..4 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let scope = scope.clone();
        let agent = format!("agent-{i}");
        joins.push(thread::spawn(move || {
            barrier.wait();
            store
                .upsert_soul(&scope, &agent, &format!("soul-body-{i}"), 0, "user_api")
                .unwrap();
        }));
    }

    barrier.wait();
    for join in joins {
        join.join().unwrap();
    }

    assert_eq!(
        store
            .list_daily_evidence(&scope, "2026-10-23")
            .unwrap()
            .len(),
        4
    );
    for i in 0..4 {
        let soul = store.get_soul(&scope, &format!("agent-{i}")).unwrap().unwrap();
        assert_eq!(soul.body, format!("soul-body-{i}"));
    }
}

#[test]
fn golden_search_lexical_vector_and_hybrid_rrf() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(
            &scope,
            &origin,
            1,
            "user",
            "user",
            &at(1),
            "用户喜欢在晚上九点阅读科技书籍，特别是操作系统与编译器相关的书",
        )
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c1 = match store
        .remember(
            &scope,
            &origin,
            &ev1,
            "用户喜欢在晚上九点阅读科技书籍",
            MemoryKind::Preference,
        )
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    let ev2 = match store
        .record_evidence(
            &scope,
            &origin,
            2,
            "user",
            "user",
            &at(2),
            "Rust memory management provides safety without garbage collection",
        )
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c2 = match store
        .remember(
            &scope,
            &origin,
            &ev2,
            "Rust memory management provides safety without garbage collection",
            MemoryKind::Fact,
        )
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Also a conversation entry in daily evidence
    let _ev3 = store
        .record_evidence(
            &scope,
            &origin,
            3,
            "user",
            "user",
            &at(3),
            "今天早上和朋友一起讨论了分布式存储系统架构",
        )
        .unwrap();

    // 1. Lexical search for Chinese
    let (hits_cn, degraded) = store
        .search_memories(&scope, "科技书籍", 10, false, None)
        .unwrap();
    assert!(!degraded);
    assert!(!hits_cn.is_empty());
    assert_eq!(hits_cn[0].id, c1);
    assert!(hits_cn[0].channels.contains(&"lexical".to_string()));

    // 2. Lexical search for Latin
    let (hits_en, degraded) = store
        .search_memories(&scope, "garbage collection", 10, false, None)
        .unwrap();
    assert!(!degraded);
    assert!(!hits_en.is_empty());
    assert_eq!(hits_en[0].id, c2);
    assert!(hits_en[0].channels.contains(&"lexical".to_string()));

    // 3. Conversation entry search (entries merged into search results)
    let (hits_entry, degraded) = store
        .search_memories(&scope, "分布式存储", 10, false, None)
        .unwrap();
    assert!(!degraded);
    assert!(!hits_entry.is_empty());
    assert_eq!(hits_entry[0].kind, "entry");

    // 4. Vector search with injected embeddings
    let vec1 = vec![0.9, 0.1, 0.0, 0.0];
    let vec2 = vec![0.0, 0.1, 0.9, 0.0];
    store
        .set_vector(
            &scope,
            &c1,
            "claim",
            1,
            &raw_content_hash("用户喜欢在晚上九点阅读科技书籍"),
            "test-emb",
            &vec1,
        )
        .unwrap();
    store
        .set_vector(
            &scope,
            &c2,
            "claim",
            1,
            &raw_content_hash("Rust memory management provides safety without garbage collection"),
            "test-emb",
            &vec2,
        )
        .unwrap();

    let q_vec = vec![0.85, 0.15, 0.0, 0.0];
    // Query that semantically prefers c1
    let (hits_hybrid, degraded) = store
        .search_memories(&scope, "阅读", 10, false, Some(&q_vec))
        .unwrap();
    assert!(!degraded);
    assert!(!hits_hybrid.is_empty());
    assert_eq!(hits_hybrid[0].id, c1);
    assert!(hits_hybrid[0].channels.contains(&"lexical".to_string()));
    assert!(hits_hybrid[0].channels.contains(&"semantic".to_string()));
}

#[test]
fn rebuild_index_restores_identical_active_set_versions_sources_and_ranking() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "事实一：Alice 居住在上海")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c1 = match store
        .remember(&scope, &origin, &ev1, "事实一：Alice 居住在上海", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "事实二：Bob 居住在北京")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c2 = match store
        .remember(&scope, &origin, &ev2, "事实二：Bob 居住在北京", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Vector injection for c1 & c2
    store
        .set_vector(
            &scope,
            &c1,
            "claim",
            1,
            &raw_content_hash("事实一：Alice 居住在上海"),
            "model-v1",
            &[1.0, 0.0],
        )
        .unwrap();
    store
        .set_vector(
            &scope,
            &c2,
            "claim",
            1,
            &raw_content_hash("事实二：Bob 居住在北京"),
            "model-v1",
            &[0.0, 1.0],
        )
        .unwrap();

    let (before_hits, before_deg) = store
        .search_memories(&scope, "居住", 10, false, Some(&[0.9, 0.1]))
        .unwrap();
    assert!(!before_deg);
    assert_eq!(before_hits.len(), 4);
    let before_c1_explain = store.explain_memory(&scope, &c1, false).unwrap().unwrap();

    // Delete projection db and manifest (vectors shard kept in index/vectors)
    let scope_dir = store.scope_dir(&scope).unwrap();
    let db_p = ProjectionIndex::db_path(&scope_dir);
    let mf_p = ProjectionIndex::manifest_path(&scope_dir);
    if db_p.exists() {
        fs::remove_file(&db_p).unwrap();
    }
    if mf_p.exists() {
        fs::remove_file(&mf_p).unwrap();
    }
    assert!(!db_p.exists());
    assert!(!mf_p.exists());

    // Rebuild index
    let report = store.rebuild_index(&scope).unwrap();
    assert_eq!(report.claims_indexed, 2);
    assert_eq!(report.vectors_retained, 2);

    // Search after rebuild
    let (after_hits, after_deg) = store
        .search_memories(&scope, "居住", 10, false, Some(&[0.9, 0.1]))
        .unwrap();
    assert!(!after_deg);
    assert_eq!(before_hits.len(), after_hits.len());
    for (b, a) in before_hits.iter().zip(after_hits.iter()) {
        assert_eq!(b.id, a.id);
        assert_eq!(b.version, a.version);
        assert_eq!(b.content, a.content);
        assert_eq!(b.channels, a.channels);
    }

    let after_c1_explain = store.explain_memory(&scope, &c1, false).unwrap().unwrap();
    assert_eq!(before_c1_explain.memory_id, after_c1_explain.memory_id);
    assert_eq!(before_c1_explain.version, after_c1_explain.version);
    assert_eq!(before_c1_explain.evidence.len(), after_c1_explain.evidence.len());
    assert_eq!(
        before_c1_explain.evidence[0].evidence_id,
        after_c1_explain.evidence[0].evidence_id
    );
    assert_eq!(
        before_c1_explain.evidence[0].quote,
        after_c1_explain.evidence[0].quote
    );
}

#[test]
fn forgotten_and_expired_items_are_invisible_in_search_explain_and_bundle() {
    let (_temp, store, scope, origin) = fixture();
    // 1. Normal active claim
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "正常记忆条目")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c1 = match store
        .remember(&scope, &origin, &ev1, "正常记忆条目", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // 2. Claim to be forgotten
    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "即将遗忘的记忆条目")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c2 = match store
        .remember(&scope, &origin, &ev2, "即将遗忘的记忆条目", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    store.forget_claim(&scope, &c2).unwrap();

    // 3. Expired claim (valid_until in the past)
    let ev3 = match store
        .record_evidence(&scope, &origin, 3, "user", "user", &at(3), "已经过期的记忆条目")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c3 = match store
        .remember(&scope, &origin, &ev3, "已经过期的记忆条目", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    // Modify c3 claim file to have past valid_until
    let scope_dir = store.scope_dir(&scope).unwrap();
    let c3_path = claim_path(&scope_dir, &c3).unwrap();
    let mut c3_doc = ClaimDocument::parse(&fs::read(&c3_path).unwrap()).unwrap();
    c3_doc.front.valid_until = Some("2020-01-01T00:00:00Z".to_string());
    atomic_replace(&c3_path, &c3_doc.to_bytes().unwrap()).unwrap();
    store.rebuild_index(&scope).unwrap();

    // Verification 1: search_memories
    let (hits, _) = store.search_memories(&scope, "记忆条目", 10, false, None).unwrap();
    let hit_ids: Vec<String> = hits.into_iter().map(|h| h.id).collect();
    assert!(hit_ids.contains(&c1));
    assert!(!hit_ids.contains(&c2), "forgotten claim must not appear in search");
    assert!(!hit_ids.contains(&c3), "expired claim must not appear in search");

    // Verification 2: explain_memory & get_memory
    assert!(store.get_memory(&scope, &c1).unwrap().is_some());
    assert!(store.get_memory(&scope, &c2).unwrap().is_none(), "forgotten claim get_memory is None");
    assert!(store.get_memory(&scope, &c3).unwrap().is_none(), "expired claim get_memory is None");

    assert!(store.explain_memory(&scope, &c1, false).unwrap().is_some());
    assert!(store.explain_memory(&scope, &c2, false).unwrap().is_none(), "forgotten claim explain is None");
    assert!(store.explain_memory(&scope, &c3, false).unwrap().is_none(), "expired claim explain without history is None");

    // Verification 3: context_bundle
    let bundle = store
        .context_bundle(&scope, "agent", "记忆条目", None, None)
        .unwrap();
    let bundle_json = serde_json::to_string(&bundle).unwrap();
    assert!(bundle_json.contains(&c1), "active claim c1 must be present in bundle");
    assert!(!bundle_json.contains(&c2), "forgotten claim c2 must not be in bundle");
    assert!(!bundle_json.contains(&c3), "expired claim c3 must not be in bundle");
}

#[test]
fn context_bundle_segments_order_budget_empty_and_deduplication() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "常驻与检索重合的记忆")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c1 = match store
        .remember(&scope, &origin, &ev1, "常驻与检索重合的记忆", MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };
    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "纯检索召回的记忆")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c2 = match store
        .remember(&scope, &origin, &ev2, "纯检索召回的记忆", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // 1. Without alignment or relationships (empty segments present)
    // Pass compact_max_items: Some(1) so c1 is in compact, while c2 overflows to retrieved!
    let bundle_empty = store
        .context_bundle_with_limits(&scope, "agent", "记忆", None, None, Some(1))
        .unwrap();
    let segs = bundle_empty.segments.as_array().unwrap();
    assert_eq!(segs.len(), 4);
    assert_eq!(segs[0]["segment"], "compact");
    assert_eq!(segs[1]["segment"], "alignment");
    assert_eq!(segs[2]["segment"], "relationships");
    assert_eq!(segs[3]["segment"], "retrieved");

    // Empty segments assertions
    assert_eq!(segs[1]["char_count"], 0);
    assert_eq!(segs[1]["complete"], true);
    assert_eq!(segs[2]["char_count"], 0);
    assert_eq!(segs[2]["total"], 0);

    // Deduplication assertion: c1 is in compact (via MEMORY.md), so it must NOT appear in retrieved text!
    let ret_text = segs[3]["text"].as_str().unwrap();
    assert!(ret_text.contains(&c2));
    assert!(!ret_text.contains(&c1), "c1 is covered in compact and must be deduped from retrieved");
    assert_eq!(segs[3]["deduped"], 1, "deduped count must be 1");

    // 2. With alignment and relationships populated
    let align_text = "这是系统的对齐合成指引文本，用于约束行为风格。";
    let rel_data = vec![
        ("e1".to_string(), "璃".to_string(), "我的伙伴".to_string()),
        ("e2".to_string(), "Alice".to_string(), "同事".to_string()),
    ];
    let bundle_full = store
        .context_bundle(&scope, "agent", "记忆", Some(align_text), Some(&rel_data))
        .unwrap();
    let full_segs = bundle_full.segments.as_array().unwrap();
    assert_eq!(full_segs[1]["char_count"], align_text.chars().count());
    assert_eq!(full_segs[1]["body"], align_text);
    assert_eq!(full_segs[2]["total"], 2);

    // Budget checks
    let total_used = bundle_full.budget["used_chars"].as_u64().unwrap();
    let total_budget = bundle_full.budget["total_chars"].as_u64().unwrap();
    assert_eq!(total_budget, 8000);
    assert!(total_used <= 8000);
}

#[test]
fn index_stale_fallback_walks_direct_files_with_degraded_flag() {
    let (_temp, store, scope, origin) = fixture();
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "原始待匹配事实内容")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c1 = match store
        .remember(&scope, &origin, &ev1, "原始待匹配事实内容", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Initially fresh
    assert_eq!(
        store.check_index_freshness(&scope).unwrap(),
        IndexFreshness::Fresh
    );
    let (hits, degraded) = store.search_memories(&scope, "待匹配", 10, false, None).unwrap();
    assert!(!degraded);
    assert_eq!(hits[0].id, c1);

    // Make the index stale artificially: directly edit a claim file without rebuilding index
    let scope_dir = store.scope_dir(&scope).unwrap();
    let c1_path = claim_path(&scope_dir, &c1).unwrap();
    let mut doc = ClaimDocument::parse(&fs::read(&c1_path).unwrap()).unwrap();
    doc.body = "外部修改后的新事实内容包含关键字独角兽".to_string();
    doc.front.claim_sha256 = memory_domain::claim_sha256(MemoryKind::Fact, &doc.body);
    doc.front.normalized_claim = memory_domain::normalize_v1(&doc.body);
    atomic_replace(&c1_path, &doc.to_bytes().unwrap()).unwrap();

    // Now freshness check must detect fingerprint mismatch -> Stale
    assert_eq!(
        store.check_index_freshness(&scope).unwrap(),
        IndexFreshness::Stale
    );

    // Search when stale: must fall back to direct scan, find the updated content, and flag degraded!
    let (stale_hits, degraded) = store.search_memories(&scope, "独角兽", 10, false, None).unwrap();
    assert!(degraded, "search must report degraded=true when index is stale");
    assert!(!stale_hits.is_empty());
    assert_eq!(stale_hits[0].id, c1);
    assert_eq!(stale_hits[0].content, "外部修改后的新事实内容包含关键字独角兽");

    // Bundle also reports index_degraded
    let bundle = store.context_bundle(&scope, "agent", "独角兽", None, None).unwrap();
    assert_eq!(bundle.lexical_status, "index_degraded");

    // Rebuild brings it back to Fresh
    store.rebuild_index(&scope).unwrap();
    assert_eq!(
        store.check_index_freshness(&scope).unwrap(),
        IndexFreshness::Fresh
    );
    let (rebuilt_hits, degraded) = store.search_memories(&scope, "独角兽", 10, false, None).unwrap();
    assert!(!degraded, "search must report degraded=false after rebuild");
    assert_eq!(rebuilt_hits[0].id, c1);
}

#[test]
fn explain_memory_returns_verbatim_spans_speaker_and_relations() {
    let (_temp, store, scope, origin) = fixture();
    let payload = "前缀文本，用户喜欢的书是《计算理论导引》，后缀文本。";
    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), payload)
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let quote = "用户喜欢的书是《计算理论导引》";
    let c1 = match store
        .remember(&scope, &origin, &ev1, quote, MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    let explain = store.explain_memory(&scope, &c1, false).unwrap().unwrap();
    assert_eq!(explain.memory_id, c1);
    assert_eq!(explain.claim, quote);
    assert_eq!(explain.speaker, Some("user".to_string()));
    assert_eq!(explain.subject, None);
    assert_eq!(explain.subject_source, "none");
    assert_eq!(explain.stable_ref, format!("riko://memory/{c1}"));
    assert_eq!(explain.evidence.len(), 1);
    assert_eq!(explain.evidence[0].quote, quote);
    assert_eq!(explain.evidence[0].span_exact, true);
    assert_eq!(explain.evidence[0].evidence_id, ev1);
    assert_eq!(explain.relations.supersedes, None);
    assert_eq!(explain.relations.superseded_by, None);

    // Supersede creates link
    let ev2 = match store
        .record_evidence(
            &scope,
            &origin,
            2,
            "user",
            "user",
            &at(2),
            "纠正：用户喜欢的书是《统计学习方法》",
        )
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c2 = match store
        .supersede(
            &scope,
            &c1,
            &origin,
            &ev2,
            "用户喜欢的书是《统计学习方法》",
            MemoryKind::Preference,
        )
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Old claim explain (with history)
    let old_explain = store.explain_memory(&scope, &c1, true).unwrap().unwrap();
    assert_eq!(old_explain.status, "superseded");
    assert_eq!(old_explain.relations.superseded_by, Some(c2.clone()));

    // New claim explain
    let new_explain = store.explain_memory(&scope, &c2, false).unwrap().unwrap();
    assert_eq!(new_explain.status, "active");
    assert_eq!(new_explain.relations.supersedes, Some(c1));
}

fn compute_tree_hashes(dir: &Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&current).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    let rel = p.strip_prefix(dir).unwrap().to_string_lossy().to_string();
                    if rel == "LOCK" || rel.ends_with("/LOCK") {
                        continue;
                    }
                    let bytes = fs::read(&p).unwrap();
                    map.insert(rel, sha256_hex(&bytes));
                }
            }
        }
    }
    map
}

#[test]
fn forget_closure_full_invisibility_derived_and_index_cleared_reextract_blocked() {
    let (_temp, store, scope, origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();

    // 1. Ingest evidence and remember
    let ev_text = "用户特别喜欢在夜晚散步，思考哲学与宇宙";
    let ev_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), ev_text)
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c_id = match store
        .remember(&scope, &origin, &ev_id, ev_text, MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Before forget: visible in search, explain, bundle, MEMORY.md, and bank/opinions.md
    let (hits, _) = store.search_memories(&scope, "散步", 10, false, None).unwrap();
    assert!(hits.iter().any(|h| h.id == c_id));
    assert!(store.explain_memory(&scope, &c_id, false).unwrap().is_some());
    let bundle = store.context_bundle(&scope, "a", "散步", None, None).unwrap();
    let compact_items = bundle.segments[0]["items"].as_array().unwrap();
    assert!(compact_items.iter().any(|it| it["body"].as_str().unwrap().contains("散步")));

    let bank_view = store.get_bank_facet(&scope, "opinions").unwrap();
    assert!(bank_view.items.iter().any(|it| it.memory_id == c_id));

    // 2. Full forget closure
    store.forget_claim(&scope, &c_id).unwrap();

    // Assertion 1: Three read paths are absolutely invisible
    let (hits_after, _) = store.search_memories(&scope, "散步", 10, false, None).unwrap();
    assert!(!hits_after.iter().any(|h| h.id == c_id));
    assert_eq!(store.explain_memory(&scope, &c_id, false).unwrap().map(|e| e.claim), None);
    assert_eq!(store.get_claim(&scope, &c_id).unwrap().map(|c| c.body), None);
    assert_eq!(store.get_memory(&scope, &c_id).unwrap().map(|c| c.body), None);

    let bundle_after = store.context_bundle(&scope, "a", "散步", None, None).unwrap();
    let compact_after = bundle_after.segments[0]["items"].as_array().unwrap();
    assert!(!compact_after.iter().any(|it| it["body"].as_str().unwrap().contains("散步")));

    // Assertion 2: Derived files (MEMORY.md and bank/*.md) do not contain the item
    let overview = store.get_overview(&scope).unwrap();
    if let Some(ov) = overview {
        assert!(!ov.curated.iter().any(|r| r.memory_id == c_id));
    }
    let bank_view_after = store.get_bank_facet(&scope, "opinions").unwrap();
    assert!(!bank_view_after.items.iter().any(|it| it.memory_id == c_id));

    // Assertion 3: Projection index SQLite db has no residue of this claim
    let db_path = ProjectionIndex::db_path(&scope_dir);
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let count_proj: i64 = conn
        .query_row(
            "SELECT count(*) FROM projected_items WHERE object_id = ?1",
            [&c_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_proj, 0, "projected_items must not contain forgotten claim");

    let count_fts: i64 = conn
        .query_row(
            "SELECT count(*) FROM fts_items WHERE object_id = ?1",
            [&c_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_fts, 0, "fts_items must not contain forgotten claim");

    let count_grams: i64 = conn
        .query_row(
            "SELECT count(*) FROM cjk_grams WHERE object_id = ?1",
            [&c_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_grams, 0, "cjk_grams must not contain forgotten claim");

    // Assertion 4: Extraction-side and Direct-write suppression checks block re-entry
    // Doc8/06 rule 7: check extraction blocked
    let is_suppressed_extract = store
        .check_extraction_suppressed(&scope, &ev_id, MemoryKind::Preference, ev_text)
        .unwrap();
    assert!(is_suppressed_extract, "extraction side must block suppressed evidence");

    // Direct write blocked
    let rewrite_res = store.remember(&scope, &origin, &ev_id, ev_text, MemoryKind::Preference);
    assert!(matches!(rewrite_res, Err(StoreError::Suppressed)));
}

#[test]
fn upkeep_processes_ledger_idempotently_and_second_run_is_zero_diff() {
    let (_temp, store, scope, origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();

    // Ingest two evidence blocks
    let ev1 = store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "第一条日常事件记录")
        .unwrap();
    let ev2 = store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "第二条日常事件记录")
        .unwrap();
    let id1 = match ev1 { IngestOutcome::Recorded(id) => id, _ => unreachable!() };
    let id2 = match ev2 { IngestOutcome::Recorded(id) => id, _ => unreachable!() };

    // Remember one claim
    let _claim = store
        .remember(&scope, &origin, &id2, "第二条日常事件记录", MemoryKind::Fact)
        .unwrap();

    // First upkeep: should process new evidences and write ledger
    let report1 = store.upkeep(&scope).unwrap();
    assert_eq!(report1.new_evidence_processed, 2);
    assert!(report1.files_modified > 0);

    // Verify ledger exists and contains both IDs
    let ledger_path = scope_dir.join("memory/LEDGER");
    assert!(ledger_path.exists());
    let ledger_text = fs::read_to_string(&ledger_path).unwrap();
    assert!(ledger_text.contains(&id1));
    assert!(ledger_text.contains(&id2));

    // Capture file tree hashes after first upkeep
    let tree_before = compute_tree_hashes(&scope_dir);

    // Run second upkeep immediately: must be strictly idempotent with zero modifications
    let report2 = store.upkeep(&scope).unwrap();
    assert_eq!(report2.new_evidence_processed, 0);
    assert_eq!(report2.expired_materialized, 0);
    assert_eq!(report2.derived_refreshed, false);
    assert_eq!(report2.index_rebuilt, false);
    assert_eq!(report2.files_modified, 0, "second upkeep must have 0 files modified");

    // Capture file tree hashes after second upkeep
    let tree_after = compute_tree_hashes(&scope_dir);

    // Assert exact equality of entire file tree hashes
    assert_eq!(tree_before, tree_after, "entire file tree must be byte-for-byte identical on second upkeep");
}

#[test]
fn upkeep_materializes_expired_claims_to_expired_status_and_invisible() {
    let (_temp, store, scope, origin) = fixture();

    let ev_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "即将过期的短期偏好")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!()
    };

    let claim_id = match store
        .remember(&scope, &origin, &ev_id, "即将过期的短期偏好", MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!()
    };

    // Mutate claim front matter to set valid_until in the past
    let scope_dir = store.scope_dir(&scope).unwrap();
    let path = scope_dir.join("memory/claims").join(format!("{claim_id}.md"));
    let mut claim_doc = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    claim_doc.front.valid_until = Some("2026-01-01T00:00:00.000000Z".to_string());
    fs::write(&path, claim_doc.to_bytes().unwrap()).unwrap();

    // Verify claim is still active in file before upkeep
    let claim_pre = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(claim_pre.front.status, "active");

    // Run upkeep to materialize expiration
    let report = store.upkeep(&scope).unwrap();
    assert_eq!(report.expired_materialized, 1);

    // Verify claim file now has status = "expired" and bumped version
    let claim_post = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(claim_post.front.status, "expired");
    assert_eq!(claim_post.front.version, 2);
    assert_eq!(claim_post.front.reason_code, "expired");

    // Verify invisible in read paths
    assert_eq!(store.get_memory(&scope, &claim_id).unwrap(), None);
    let (hits, _) = store.search_memories(&scope, "短期偏好", 10, false, None).unwrap();
    assert!(!hits.iter().any(|h| h.id == claim_id));
    let bundle = store.context_bundle(&scope, "a", "短期偏好", None, None).unwrap();
    let compact_items = bundle.segments[0]["items"].as_array().unwrap();
    assert!(!compact_items.iter().any(|it| it["body"].as_str().unwrap().contains("短期偏好")));
}

#[test]
fn upkeep_zero_signal_zero_action_preserves_tree_hash() {
    let (_temp, store, scope, origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();

    // Settle into steady state
    let ev_id = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "长期稳定事实：地球围绕太阳旋转")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    store
        .remember(&scope, &origin, &ev_id, "长期稳定事实：地球围绕太阳旋转", MemoryKind::Fact)
        .unwrap();

    // Initial upkeep to establish steady state
    store.upkeep(&scope).unwrap();

    // Record hashes before dry run
    let hashes_before = compute_tree_hashes(&scope_dir);

    // Dry run upkeep with zero signals
    let report = store.upkeep(&scope).unwrap();
    assert_eq!(report.new_evidence_processed, 0);
    assert_eq!(report.expired_materialized, 0);
    assert_eq!(report.derived_refreshed, false);
    assert_eq!(report.index_rebuilt, false);
    assert_eq!(report.files_modified, 0);

    // Record hashes after dry run
    let hashes_after = compute_tree_hashes(&scope_dir);

    assert_eq!(hashes_before, hashes_after, "dry run upkeep must not touch any files in scope directory");
}

#[test]
fn supersede_atomicity_and_consistency_no_conflicting_window() {
    let (_temp, store, scope, origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();

    let ev1 = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "旧事实：用户现居北京")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c_old = match store
        .remember(&scope, &origin, &ev1, "旧事实：用户现居北京", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    let ev2 = match store
        .record_evidence(&scope, &origin, 2, "user", "user", &at(2), "新事实：用户搬家现居杭州")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };

    // Supersede in single lock
    let c_new = match store
        .supersede(&scope, &c_old, &origin, &ev2, "新事实：用户搬家现居杭州", MemoryKind::Fact)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Assert states are consistent
    let old_doc = store.read_claim_file(&scope_dir, &c_old).unwrap().unwrap();
    let new_doc = store.read_claim_file(&scope_dir, &c_new).unwrap().unwrap();

    assert_eq!(old_doc.front.status, "superseded");
    assert_eq!(old_doc.front.version, 2);
    assert_eq!(new_doc.front.status, "active");
    assert_eq!(new_doc.front.version, 1);
    assert_eq!(new_doc.front.supersedes, Some(c_old.clone()));

    // Visibility: old is invisible without history, new is visible
    assert_eq!(store.get_memory(&scope, &c_old).unwrap(), None);
    assert_eq!(store.get_memory(&scope, &c_new).unwrap().unwrap().body, "新事实：用户搬家现居杭州");

    // Curated MEMORY.md only contains new claim
    let overview = store.get_overview(&scope).unwrap().unwrap();
    assert!(overview.curated.iter().any(|r| r.memory_id == c_new));
    assert!(!overview.curated.iter().any(|r| r.memory_id == c_old));

    // Reflections bank facet contains old claim because it has an actual supersede edge
    let refl_view = store.get_bank_facet(&scope, "reflections").unwrap();
    assert!(refl_view.items.iter().any(|it| it.memory_id == c_old));
}

#[test]
fn bank_derived_views_instant_invalidation_before_refresh() {
    let (_temp, store, scope, origin) = fixture();
    let scope_dir = store.scope_dir(&scope).unwrap();

    let ev = match store
        .record_evidence(&scope, &origin, 1, "user", "user", &at(1), "用户偏好每天早起慢跑5公里")
        .unwrap()
    {
        IngestOutcome::Recorded(id) => id,
        _ => unreachable!(),
    };
    let c_id = match store
        .remember(&scope, &origin, &ev, "用户偏好每天早起慢跑5公里", MemoryKind::Preference)
        .unwrap()
    {
        RememberOutcome::Created { memory_id, .. } => memory_id,
        _ => unreachable!(),
    };

    // Ensure bank file exists
    let view1 = store.get_bank_facet(&scope, "opinions").unwrap();
    assert_eq!(view1.items.len(), 1);
    assert_eq!(view1.items[0].memory_id, c_id);
    assert_eq!(view1.skipped_stale, 0);

    // Directly mutate claim on disk to expired without running upkeep/refresh
    let path = scope_dir.join("memory/claims").join(format!("{c_id}.md"));
    let mut claim_doc = ClaimDocument::parse(&fs::read(&path).unwrap()).unwrap();
    claim_doc.front.valid_until = Some("2026-01-01T00:00:00.000000Z".to_string());
    fs::write(&path, claim_doc.to_bytes().unwrap()).unwrap();

    // Read view: source verification triggers instant invalidation before upkeep
    let view2 = store.get_bank_facet(&scope, "opinions").unwrap();
    assert_eq!(view2.items.len(), 0, "stale claim must be instantly skipped");
    assert_eq!(view2.skipped_stale, 1, "skipped_stale must record the invalidated claim");

    // Now run upkeep to physically reconcile the bank file
    let report = store.upkeep(&scope).unwrap();
    assert_eq!(report.expired_materialized, 1);
    assert_eq!(report.derived_refreshed, true);

    let view3 = store.get_bank_facet(&scope, "opinions").unwrap();
    assert_eq!(view3.items.len(), 0);
    assert_eq!(view3.skipped_stale, 0, "after refresh bank file matches live state with 0 stale");
}
