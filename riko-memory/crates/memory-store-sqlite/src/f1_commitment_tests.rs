use std::path::PathBuf;

use crate::{Store, StoreError};
use memory_domain::{DomainScope, ScopeKey};

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-f1-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    (store, scope)
}

fn dom() -> DomainScope {
    DomainScope::user_main()
}

fn insert_memory(store: &mut Store, scope: &ScopeKey, memory_id: &str, kind: &str) {
    let now = "2026-10-09T00:00:00Z";
    store
        .conn_mut()
        .execute(
            "INSERT INTO memories (
                id, tenant_id, user_id, kind, claim, normalized_claim, claim_sha256,
                source_class, status, version, origin_host_id, origin_agent_id,
                domain_id, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, 'test instruction', 'test instruction', 'hash123',
                      'user_explicit', 'active', 1, 'dsh', 'a',
                      'user_main', ?5, ?5)",
            rusqlite::params![memory_id, scope.tenant_id, scope.user_id, kind, now],
        )
        .unwrap();
}

#[test]
fn commitment_four_state_lifecycle() {
    let (mut store, scope) = setup("four-state-lifecycle");
    let mem_id = "mem-lifecycle-1";

    // 1. 创建 memory（Instruction kind）
    insert_memory(&mut store, &scope, mem_id, "instruction");

    // 2. 创建 commitment（kind=deadline, due_at=未来时间）
    let c1 = store
        .commitment_create(
            &scope,
            mem_id,
            "deadline",
            Some("2099-01-01T00:00:00Z"),
        )
        .unwrap();

    // 3. 验证 status=pending
    let list = store.commitment_list(&scope, None, None).unwrap();
    let item = list.iter().find(|c| c.id == c1).expect("commitment c1 exists");
    assert_eq!(item.status, "pending");
    assert_eq!(item.kind, "deadline");
    assert_eq!(item.due_at.as_deref(), Some("2099-01-01T00:00:00Z"));
    assert!(item.fulfilled_at.is_none());

    // 4. fulfill → status=fulfilled
    let fulfilled = store.commitment_fulfill(&scope, &c1).unwrap();
    assert!(fulfilled);
    let list = store.commitment_list(&scope, Some("fulfilled"), None).unwrap();
    let item = list.iter().find(|c| c.id == c1).expect("commitment c1 is fulfilled");
    assert_eq!(item.status, "fulfilled");
    assert!(item.fulfilled_at.is_some());

    // 5. 再创建一个，cancel → status=cancelled
    let c2 = store
        .commitment_create(
            &scope,
            mem_id,
            "reminder",
            Some("2099-06-01T00:00:00Z"),
        )
        .unwrap();
    let cancelled = store.commitment_cancel(&scope, &c2).unwrap();
    assert!(cancelled);
    let list = store.commitment_list(&scope, Some("cancelled"), None).unwrap();
    let item = list.iter().find(|c| c.id == c2).expect("commitment c2 is cancelled");
    assert_eq!(item.status, "cancelled");

    // 6. exists 与 get 验证
    assert!(store.commitment_exists(&scope, &c1).unwrap());
    assert_eq!(store.commitment_get(&scope, &c1).unwrap().unwrap().status, "fulfilled");
    assert!(store.commitment_exists(&scope, &c2).unwrap());
    assert_eq!(store.commitment_get(&scope, &c2).unwrap().unwrap().status, "cancelled");
    assert!(!store.commitment_exists(&scope, "non-existent-id").unwrap());
    assert!(store.commitment_get(&scope, "non-existent-id").unwrap().is_none());
}

#[test]
fn commitment_overdue_tick() {
    let (mut store, scope) = setup("overdue-tick");
    let mem_id = "mem-overdue-1";

    // 1. 创建 memory
    insert_memory(&mut store, &scope, mem_id, "instruction");

    // 2. 创建 commitment，due_at=过去时间（如 2000-01-01T00:00:00Z）
    let c1 = store
        .commitment_create(
            &scope,
            mem_id,
            "deadline",
            Some("2000-01-01T00:00:00Z"),
        )
        .unwrap();

    // 3. 调用 store.commitment_tick_overdue()
    let updated = store.commitment_tick_overdue().unwrap();
    assert!(updated >= 1);

    // 4. 验证 status 变为 overdue
    let list = store.commitment_list(&scope, Some("overdue"), None).unwrap();
    let item = list.iter().find(|c| c.id == c1).expect("commitment c1 is overdue");
    assert_eq!(item.status, "overdue");

    // 5. 原 memory 不受影响
    let mem_status: String = store
        .conn()
        .query_row(
            "SELECT status FROM memories WHERE tenant_id = ?1 AND user_id = ?2 AND id = ?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mem_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mem_status, "active");
}

#[test]
fn commitment_cascade_delete() {
    let (mut store, scope) = setup("cascade-delete");
    let mem_id = "mem-cascade-1";

    // 1. 创建 memory
    insert_memory(&mut store, &scope, mem_id, "instruction");

    // 2. 创建 commitment 关联它
    let comm_id = store
        .commitment_create(
            &scope,
            mem_id,
            "deadline",
            Some("2030-01-01T00:00:00Z"),
        )
        .unwrap();

    let count_before: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM commitments WHERE id = ?1",
            rusqlite::params![comm_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_before, 1);

    // 3. 删除 memory（找到删除 memory 的方法，可能是 purge 或 delete）
    let (token, _) = store
        .purge_preview(&scope, mem_id, "idem-cascade", &dom())
        .unwrap();
    store
        .purge_confirm(&scope, &token, "idem-cascade", &dom())
        .unwrap();

    let mem_count: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE id = ?1",
            rusqlite::params![mem_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mem_count, 0);

    // 4. 验证 commitments 表中该记录已级联删除
    let count_after: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM commitments WHERE id = ?1",
            rusqlite::params![comm_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_after, 0);

    // 附加验证：直接 DELETE FROM memories 亦可触发外键级联删除
    let mem_id2 = "mem-cascade-2";
    insert_memory(&mut store, &scope, mem_id2, "instruction");
    let comm_id2 = store
        .commitment_create(
            &scope,
            mem_id2,
            "reminder",
            Some("2030-01-01T00:00:00Z"),
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "DELETE FROM memories WHERE tenant_id = ?1 AND user_id = ?2 AND id = ?3",
            rusqlite::params![scope.tenant_id, scope.user_id, mem_id2],
        )
        .unwrap();
    let count_after2: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM commitments WHERE id = ?1",
            rusqlite::params![comm_id2],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count_after2, 0);
}

#[test]
fn commitment_invalid_kind_rejected() {
    let (mut store, scope) = setup("invalid-kind");
    let mem_id = "mem-invalid-kind";
    insert_memory(&mut store, &scope, mem_id, "instruction");

    // 创建 commitment 时 kind 传非法值，期望返回 StoreError::StateConflict
    let err = store
        .commitment_create(&scope, mem_id, "invalid_kind", None)
        .unwrap_err();
    assert!(matches!(err, StoreError::StateConflict));
}
