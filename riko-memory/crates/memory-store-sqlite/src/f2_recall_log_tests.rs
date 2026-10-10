use std::path::PathBuf;

use crate::Store;
use memory_domain::ScopeKey;

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("migrations")
}

fn setup(tag: &str) -> (Store, ScopeKey) {
    let mut store = Store::open_in_memory(&migrations_dir()).unwrap();
    let dir = std::env::temp_dir().join(format!("am-f2-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    store.principal_add("t", "u", &dir.join("u.token")).unwrap();
    let token = std::fs::read_to_string(dir.join("u.token")).unwrap();
    let scope = store.verify_token(token.trim()).unwrap().unwrap();
    (store, scope)
}

#[test]
fn recall_log_write_and_list() {
    let (mut store, _scope) = setup("write-and-list");
    let id = store
        .recall_log_write("test query", "[\"id1\",\"id2\"]", None, None, None, None)
        .unwrap();
    assert!(id > 0);

    let list = store.recall_log_list(50, None).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].query, "test query");

    let list_filtered = store.recall_log_list(50, Some("test")).unwrap();
    assert_eq!(list_filtered.len(), 1);
    assert_eq!(list_filtered[0].id, id);

    let list_nomatch = store.recall_log_list(50, Some("nomatch")).unwrap();
    assert_eq!(list_nomatch.len(), 0);
}

#[test]
fn recall_log_get() {
    let (mut store, _scope) = setup("get");
    let id = store
        .recall_log_write("test query", "[\"id1\",\"id2\"]", None, None, None, None)
        .unwrap();

    let entry = store.recall_log_get(id).unwrap();
    assert!(entry.is_some());
    let entry = entry.unwrap();
    assert_eq!(entry.id, id);
    assert_eq!(entry.query, "test query");

    let non_existent = store.recall_log_get(99999).unwrap();
    assert!(non_existent.is_none());
}

#[test]
fn recall_log_retention() {
    let (mut store, _scope) = setup("retention");
    let id = store
        .recall_log_write("test query", "[\"id1\",\"id2\"]", None, None, None, None)
        .unwrap();

    store
        .conn_mut()
        .execute(
            "UPDATE recall_log SET ts='2000-01-01T00:00:00Z' WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();

    let deleted = store.recall_log_retention(30).unwrap();
    assert_eq!(deleted, 1);

    let list = store.recall_log_list(50, None).unwrap();
    assert!(list.is_empty());
}
