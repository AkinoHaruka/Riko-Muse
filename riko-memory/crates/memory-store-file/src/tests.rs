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
