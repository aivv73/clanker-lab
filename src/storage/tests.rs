use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use super::{Database, MigrationVersion, StorageError};

#[test]
fn empty_database_has_no_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("paclang.db")).unwrap();

    assert_eq!(database.published_package_count().unwrap(), None);
}

#[test]
fn database_connections_enforce_foreign_keys() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("paclang.db")).unwrap();

    let enabled: bool = database
        .connection
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();
    assert!(enabled);
}

#[test]
fn current_database_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    drop(Database::open(&path).unwrap());

    let database = Database::open(&path).unwrap();
    assert_eq!(database.published_package_count().unwrap(), None);
}

#[test]
fn malformed_current_database_is_refused_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE marker (id INTEGER); PRAGMA user_version = 2;")
        .unwrap();
    drop(connection);

    let error = match Database::open(&path) {
        Ok(_) => panic!("malformed schema was accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, StorageError::InvalidSchema(_)));

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let marker_tables: u32 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'marker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(marker_tables, 1);
}

#[test]
fn newer_database_is_refused_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE marker (id INTEGER); PRAGMA user_version = 3;")
        .unwrap();
    drop(connection);

    let error = match Database::open(&path) {
        Ok(_) => panic!("newer schema was accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StorageError::UnsupportedSchemaVersion(version)
            if version == MigrationVersion::from_database(3)
    ));

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let inventory_tables: u32 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'inventory_snapshots'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 3);
    assert_eq!(inventory_tables, 0);
}

#[test]
fn concurrent_first_open_runs_one_complete_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = Arc::new(directory.path().join("paclang.db"));
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                Database::open(&path)
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    for handle in handles {
        let database = handle.join().unwrap().unwrap();
        assert_eq!(database.published_package_count().unwrap(), None);
    }

    let connection = Connection::open(path.as_ref()).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn version_one_database_migrates_to_current() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             INSERT INTO inventory_snapshots (published_at) VALUES ('legacy-marker');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let database = Database::open(&path).unwrap();
    assert_eq!(database.published_package_count().unwrap(), None);

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let snapshots: u32 = connection
        .query_row("SELECT count(*) FROM inventory_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snapshots, 0);
}
