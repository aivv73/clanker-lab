use psi::store::Store;
use serde_json::json;

#[test]
fn exact_items_action_and_terminal_boundaries_survive_restart() {
    let root = std::env::temp_dir().join(format!("psi-store-{}", uuid::Uuid::new_v4()));
    let mut store = Store::open(&root).unwrap();
    assert!(Store::open(&root).is_err());
    store.session("s", None, &root).unwrap();
    let command = json!({"op":"turn","request_id":"r","session_id":"s","turn_id":"t"});
    let mut acceptance = json!({"status":"accepted","session_id":"s","turn_id":"t"});
    store.accept(&command, &mut acceptance).unwrap();
    let items = vec![
        json!({"type":"reasoning","encrypted_content":"opaque","id":"rs_1"}),
        json!({"type":"function_call","call_id":"call_1","arguments":"{  \"x\": 1 }"}),
        json!({"type":"function_call_output","call_id":"call_1","output":"exact\nresult"}),
    ];
    store
        .action_intent("a", "s", "t", &json!({"patch":"exact patch"}))
        .unwrap();
    store
        .action_result("a", &json!({"applied":["file"]}))
        .unwrap();
    assert!(store.action_result("a", &json!({})).is_err());
    let mut terminal =
        json!({"session_id":"s","turn_id":"t","request_id":"r","status":"completed"});
    store.finish(&mut terminal, Some(&items)).unwrap();
    store.terminal_done("t").unwrap();
    drop(store);
    let mut store = Store::open(&root).unwrap();
    store.recover("new-runtime").unwrap();
    assert_eq!(store.history("s").unwrap(), items);
    assert_eq!(store.events("s", 0).unwrap().len(), 2);
    assert_eq!(store.receipt("r").unwrap().unwrap().2.unwrap(), terminal);
    store
        .action_intent("uncertain", "s", "t", &json!({"path":"file"}))
        .unwrap();
    drop(store);
    let mut store = Store::open(&root).unwrap();
    store.recover("third-runtime").unwrap();
    assert!(store.blocked("s").unwrap());
    assert!(
        store
            .action_result("uncertain", &json!({"success":true}))
            .is_err()
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn real_restart_and_crash_do_not_repeat_execution() {
    let result = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/durability.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_psi"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn rejects_workspace_aliases_and_corrupt_event_cursors() {
    let root = std::env::temp_dir().join(format!("psi-boundary-{}", uuid::Uuid::new_v4()));
    let directory = root.join("state");
    let mut store = Store::open(&directory).unwrap();
    std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
    assert!(
        store
            .session("s", Some(root.join("alias").to_str().unwrap()), &directory)
            .is_err()
    );
    store.session("s", None, &directory).unwrap();
    assert!(store.events("s", 1).is_err());
    for _ in 0..2 {
        store
            .event(&mut json!({"session_id":"s","status":"streaming"}))
            .unwrap();
    }
    let db = rusqlite::Connection::open(directory.join("history.sqlite")).unwrap();
    db.execute("DELETE FROM events WHERE seq=1", []).unwrap();
    assert!(store.events("s", 0).is_err());
    drop(db);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
