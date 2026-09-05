use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use super::{Database, MigrationVersion, StorageError};
use crate::inventory::InventoryFingerprint;
use crate::package::provider::{
    AnalysisTarget, AttemptOutcome, EvidenceGrade, ProviderAttempt, SourceProvider,
};

#[test]
fn empty_database_has_no_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(&directory.path().join("paclang.db")).unwrap();

    assert!(database.published_snapshot().unwrap().is_none());
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
    assert!(database.published_snapshot().unwrap().is_none());
}

#[test]
fn malformed_current_database_is_refused_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE marker (id INTEGER); PRAGMA user_version = 5;")
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
    assert_eq!(version, 5);
    assert_eq!(marker_tables, 1);
}

#[test]
fn malformed_version_one_database_is_refused_without_migration_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (id INTEGER PRIMARY KEY) STRICT;
             INSERT INTO inventory_snapshots DEFAULT VALUES;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Database::open(&path),
        Err(StorageError::InvalidSchema(_))
    ));

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let snapshots: u32 = connection
        .query_row("SELECT count(*) FROM inventory_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    let installed_tables: u32 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'installed_packages'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(snapshots, 1);
    assert_eq!(installed_tables, 0);
}

#[test]
fn malformed_version_two_database_is_refused_without_migration_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE installed_packages (
                snapshot_id INTEGER NOT NULL,
                name TEXT NOT NULL
             ) STRICT;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Database::open(&path),
        Err(StorageError::InvalidSchema(_))
    ));

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let fingerprint_columns: u32 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('inventory_snapshots')
             WHERE name = 'inventory_fingerprint'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(fingerprint_columns, 0);
}

#[test]
fn newer_database_is_refused_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE marker (id INTEGER); PRAGMA user_version = 6;")
        .unwrap();
    drop(connection);

    let error = match Database::open(&path) {
        Ok(_) => panic!("newer schema was accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StorageError::UnsupportedSchemaVersion(version)
            if version == MigrationVersion::from_database(6)
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
    assert_eq!(version, 6);
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
        assert!(database.published_snapshot().unwrap().is_none());
    }

    let connection = Connection::open(path.as_ref()).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
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
    assert!(database.published_snapshot().unwrap().is_none());

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let snapshots: u32 = connection
        .query_row("SELECT count(*) FROM inventory_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snapshots, 0);
}

#[test]
fn version_two_snapshot_migrates_with_unknown_staleness() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE installed_packages (
                snapshot_id INTEGER NOT NULL REFERENCES inventory_snapshots(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                package_base TEXT,
                origin TEXT NOT NULL,
                install_reason TEXT NOT NULL CHECK (install_reason IN ('explicit', 'dependency')),
                installed_bytes INTEGER NOT NULL CHECK (installed_bytes >= 0),
                PRIMARY KEY (snapshot_id, name)
             ) STRICT;
             INSERT INTO inventory_snapshots (published_at) VALUES ('old-snapshot');
             INSERT INTO installed_packages (
                snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
             ) VALUES (1, 'alpha', '1.0-1', 'alpha', 'unknown', 'explicit', 42);
             PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(connection);

    let database = Database::open(&path).unwrap();
    let snapshot = database.published_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.packages.len(), 1);
    assert_eq!(snapshot.fingerprint, None);

    let connection = Connection::open(path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
}

fn table_row_count(connection: &Connection, table: &str) -> u32 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
            [table],
            |row| row.get::<_, u32>(0),
        )
        .unwrap()
        > 0
}

fn test_fingerprint() -> InventoryFingerprint {
    InventoryFingerprint::parse(&"0".repeat(64)).unwrap()
}

fn resolved_attempt(provider: &str, grade: EvidenceGrade) -> ProviderAttempt {
    ProviderAttempt {
        package_name: "alpha".to_owned(),
        package_version: "1.0-1".to_owned(),
        provider: SourceProvider::parse(provider).unwrap(),
        recipe: None,
        outcome: AttemptOutcome::Resolved {
            target: AnalysisTarget {
                location: "https://example.invalid/alpha.git".to_owned(),
                revision: Some("v1.0".to_owned()),
                subdirectory: Some("src".to_owned()),
            },
            grade,
        },
    }
}

fn unresolved_attempt(provider: &str) -> ProviderAttempt {
    ProviderAttempt {
        package_name: "alpha".to_owned(),
        package_version: "1.0-1".to_owned(),
        provider: SourceProvider::parse(provider).unwrap(),
        recipe: None,
        outcome: AttemptOutcome::Unresolved {
            reason: "no recipe found".to_owned(),
        },
    }
}

/// Publish an empty inventory snapshot so the refresh transaction commits.
/// Attempts recorded beforehand commit in the same transaction.
fn publish_empty_snapshot(database: &mut Database) {
    let refresh = database.begin_refresh().unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();
}

#[test]
fn version_three_snapshot_migrates_with_evidence_tables() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL,
                inventory_fingerprint TEXT
             ) STRICT;
             CREATE TABLE installed_packages (
                snapshot_id INTEGER NOT NULL REFERENCES inventory_snapshots(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                package_base TEXT,
                origin TEXT NOT NULL,
                install_reason TEXT NOT NULL CHECK (install_reason IN ('explicit', 'dependency')),
                installed_bytes INTEGER NOT NULL CHECK (installed_bytes >= 0),
                PRIMARY KEY (snapshot_id, name)
             ) STRICT;
             INSERT INTO inventory_snapshots (published_at, inventory_fingerprint)
              VALUES ('old-snapshot', '0000000000000000000000000000000000000000000000000000000000000000');
             INSERT INTO installed_packages (
                snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
             ) VALUES (1, 'alpha', '1.0-1', 'alpha', 'unknown', 'explicit', 42);
             PRAGMA user_version = 3;",
        )
        .unwrap();
    drop(connection);

    let database = Database::open(&path).unwrap();
    let snapshot = database.published_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.packages.len(), 1);
    assert_eq!(snapshot.fingerprint, Some(test_fingerprint()));
    assert_eq!(
        snapshot.packages[0].origin,
        crate::package::PackageOrigin::unknown()
    );
    assert_eq!(
        snapshot.packages[0].install_reason,
        crate::package::InstallReason::Explicit
    );
    assert!(
        database
            .provider_attempts("alpha", "1.0-1")
            .unwrap()
            .is_empty()
    );

    let connection = Connection::open(path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
    assert!(table_exists(&connection, "analysis_targets"));
    assert!(table_exists(&connection, "provider_attempts"));
    let stored = connection.query_row(
        "SELECT snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
         FROM installed_packages", [], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?,
            row.get::<_, String>(5)?, row.get::<_, i64>(6)?
        ))
    ).unwrap();
    assert_eq!(
        stored,
        (
            1,
            "alpha".to_owned(),
            "1.0-1".to_owned(),
            Some("alpha".to_owned()),
            "unknown".to_owned(),
            "explicit".to_owned(),
            42
        )
    );
    let publication = connection
        .query_row(
            "SELECT id, published_at, inventory_fingerprint FROM inventory_snapshots",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        publication,
        (
            1,
            "old-snapshot".to_owned(),
            test_fingerprint().as_str().to_owned()
        )
    );
    assert_eq!(table_row_count(&connection, "analysis_targets"), 0);
    assert_eq!(table_row_count(&connection, "provider_attempts"), 0);
}

#[test]
fn malformed_version_three_database_is_refused_without_migration_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE installed_packages (
                snapshot_id INTEGER NOT NULL,
                name TEXT NOT NULL
             ) STRICT;
             PRAGMA user_version = 3;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        Database::open(&path),
        Err(StorageError::InvalidSchema(_))
    ));

    let connection = Connection::open(&path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    assert!(!table_exists(&connection, "analysis_targets"));
    assert!(!table_exists(&connection, "provider_attempts"));
}

#[test]
fn provider_attempt_roundtrip_preserves_resolved_and_unresolved() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = Database::open(&directory.path().join("paclang.db")).unwrap();

    let resolved = resolved_attempt("arch", EvidenceGrade::Exact);
    let unresolved = unresolved_attempt("aur");
    let refresh = database.begin_refresh().unwrap();
    refresh.record_provider_attempt(&resolved).unwrap();
    refresh.record_provider_attempt(&unresolved).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    let attempts = database.provider_attempts("alpha", "1.0-1").unwrap();
    assert_eq!(attempts, [resolved, unresolved]);
    assert!(
        database
            .provider_attempts("alpha", "9.9-9")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .provider_attempts("other", "1.0-1")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shared_targets_normalize_absent_fields_and_preserve_attempt_grades() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let mut database = Database::open(&path).unwrap();
    let mut arch = resolved_attempt("arch", EvidenceGrade::Exact);
    if let AttemptOutcome::Resolved { target, .. } = &mut arch.outcome {
        target.revision = None;
        target.subdirectory = None;
    }
    let mut aur = arch.clone();
    aur.provider = SourceProvider::parse("aur").unwrap();
    if let AttemptOutcome::Resolved { target, grade } = &mut aur.outcome {
        target.revision = Some(String::new());
        target.subdirectory = Some(String::new());
        *grade = EvidenceGrade::Fallback;
    }
    let refresh = database.begin_refresh().unwrap();
    refresh.record_provider_attempt(&arch).unwrap();
    refresh.record_provider_attempt(&aur).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();
    if let AttemptOutcome::Resolved { target, .. } = &mut aur.outcome {
        target.revision = None;
        target.subdirectory = None;
    }
    assert_eq!(
        database.provider_attempts("alpha", "1.0-1").unwrap(),
        [arch, aur]
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(table_row_count(&connection, "analysis_targets"), 1);
    assert_eq!(table_row_count(&connection, "provider_attempts"), 2);
}

#[test]
fn abandoned_refresh_rolls_back_attempt_replacement_and_new_targets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let mut database = Database::open(&path).unwrap();
    let original = unresolved_attempt("arch");
    let refresh = database.begin_refresh().unwrap();
    refresh.record_provider_attempt(&original).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    let refresh = database.begin_refresh().unwrap();
    refresh
        .record_provider_attempt(&resolved_attempt("arch", EvidenceGrade::Exact))
        .unwrap();
    refresh
        .record_provider_attempt(&resolved_attempt("aur", EvidenceGrade::Fallback))
        .unwrap();
    drop(refresh);
    drop(database);

    let database = Database::open(&path).unwrap();
    assert_eq!(
        database.provider_attempts("alpha", "1.0-1").unwrap(),
        [original]
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(table_row_count(&connection, "analysis_targets"), 0);
    assert_eq!(table_row_count(&connection, "inventory_snapshots"), 1);
    assert_eq!(
        database.published_snapshot().unwrap().unwrap().fingerprint,
        Some(test_fingerprint())
    );
}

#[test]
fn ambiguous_mapping_preserves_all_candidates_without_a_winner() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = Database::open(&directory.path().join("paclang.db")).unwrap();

    let arch = resolved_attempt("arch", EvidenceGrade::Reconstructed);
    let mut aur = resolved_attempt("aur", EvidenceGrade::Fallback);
    if let AttemptOutcome::Resolved { target, .. } = &mut aur.outcome {
        target.location = "https://example.invalid/alpha-aur.git".to_owned();
    }
    let refresh = database.begin_refresh().unwrap();
    refresh.record_provider_attempt(&arch).unwrap();
    refresh.record_provider_attempt(&aur).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    let attempts = database.provider_attempts("alpha", "1.0-1").unwrap();
    assert_eq!(attempts, [arch, aur]);
}

#[test]
fn rerecording_an_attempt_replaces_the_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = Database::open(&directory.path().join("paclang.db")).unwrap();

    let refresh = database.begin_refresh().unwrap();
    refresh
        .record_provider_attempt(&resolved_attempt("arch", EvidenceGrade::Exact))
        .unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    let refresh = database.begin_refresh().unwrap();
    let unresolved = ProviderAttempt {
        outcome: AttemptOutcome::Unresolved {
            reason: "recipe removed".to_owned(),
        },
        ..resolved_attempt("arch", EvidenceGrade::Exact)
    };
    refresh.record_provider_attempt(&unresolved).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    let attempts = database.provider_attempts("alpha", "1.0-1").unwrap();
    assert_eq!(attempts, [unresolved]);

    let connection = Connection::open(directory.path().join("paclang.db")).unwrap();
    assert_eq!(table_row_count(&connection, "provider_attempts"), 1);
}

#[test]
fn invalid_attempts_are_rejected_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = Database::open(&directory.path().join("paclang.db")).unwrap();

    let invalid_name = ProviderAttempt {
        package_name: "bad name".to_owned(),
        ..unresolved_attempt("arch")
    };
    let empty_location = ProviderAttempt {
        outcome: AttemptOutcome::Resolved {
            target: AnalysisTarget {
                location: String::new(),
                revision: None,
                subdirectory: None,
            },
            grade: EvidenceGrade::Exact,
        },
        ..unresolved_attempt("arch")
    };
    let control_reason = ProviderAttempt {
        outcome: AttemptOutcome::Unresolved {
            reason: "bad\x07reason".to_owned(),
        },
        ..unresolved_attempt("arch")
    };
    let oversized_location = ProviderAttempt {
        outcome: AttemptOutcome::Resolved {
            target: AnalysisTarget {
                location: "x".repeat(4097),
                revision: None,
                subdirectory: None,
            },
            grade: EvidenceGrade::Exact,
        },
        ..unresolved_attempt("arch")
    };
    for attempt in [
        &invalid_name,
        &empty_location,
        &control_reason,
        &oversized_location,
    ] {
        let refresh = database.begin_refresh().unwrap();
        let error = refresh.record_provider_attempt(attempt).unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidProviderAttempt { .. }),
            "unexpected error: {error}"
        );
    }

    assert!(
        database
            .provider_attempts("alpha", "1.0-1")
            .unwrap()
            .is_empty()
    );
    assert!(
        database
            .provider_attempts("bad name", "1.0-1")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn attempts_survive_snapshot_retention_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let mut database = Database::open(&path).unwrap();

    let refresh = database.begin_refresh().unwrap();
    let expected = resolved_attempt("arch", EvidenceGrade::Override);
    refresh.record_provider_attempt(&expected).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();

    for _ in 0..11 {
        publish_empty_snapshot(&mut database);
    }

    // Retention keeps ten snapshots; the count report still reads the latest.
    let snapshot = database.published_snapshot().unwrap().unwrap();
    assert!(snapshot.packages.is_empty());
    assert_eq!(
        database.provider_attempts("alpha", "1.0-1").unwrap(),
        std::slice::from_ref(&expected)
    );

    drop(database);
    let database = Database::open(&path).unwrap();
    assert_eq!(
        database.provider_attempts("alpha", "1.0-1").unwrap(),
        [expected]
    );
}

#[test]
fn provider_and_grade_roundtrip_through_storage() {
    assert_eq!(SourceProvider::parse("arch").unwrap().as_str(), "arch");
    assert!(SourceProvider::parse("").is_err());
    assert!(SourceProvider::parse("bad\x07provider").is_err());
    for (text, grade) in [
        ("exact", EvidenceGrade::Exact),
        ("reconstructed", EvidenceGrade::Reconstructed),
        ("fallback", EvidenceGrade::Fallback),
        ("override", EvidenceGrade::Override),
    ] {
        assert_eq!(EvidenceGrade::parse(text).unwrap(), grade);
        assert_eq!(grade.as_str(), text);
    }
    assert!(EvidenceGrade::parse("confident").is_err());
}

fn create_version_four(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    for migration in [
        super::MIGRATION_1,
        super::MIGRATION_2,
        super::MIGRATION_3,
        super::MIGRATION_4,
    ] {
        connection.execute_batch(migration).unwrap();
    }
    connection.execute_batch("INSERT INTO inventory_snapshots VALUES (1, 'old', NULL);
        INSERT INTO analysis_targets VALUES (1, 'https://example.org/a.git', 'tag:v1', '');
        INSERT INTO provider_attempts VALUES (1, 'alpha', '1.0-1', 'arch', 'resolved', 1, 'reconstructed', NULL, 'old');
        PRAGMA user_version = 4;").unwrap();
    connection
}

#[test]
fn version_four_migration_preserves_attempts_and_adds_optional_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    drop(create_version_four(&path));
    let mut database = Database::open(&path).unwrap();
    let old = database.provider_attempts("alpha", "1.0-1").unwrap();
    assert_eq!(old.len(), 1);
    assert_eq!(old[0].recipe, None);
    assert_eq!(
        old[0].outcome,
        AttemptOutcome::Resolved {
            target: AnalysisTarget {
                location: "https://example.org/a.git".to_owned(),
                revision: Some("tag:v1".to_owned()),
                subdirectory: None
            },
            grade: EvidenceGrade::Reconstructed,
        }
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT recorded_at FROM provider_attempts", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "old"
    );
    assert_eq!(
        connection
            .query_row("SELECT published_at FROM inventory_snapshots", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "old"
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        5
    );
    let mut new = old[0].clone();
    new.recipe = Some(
        crate::package::provider::RecipeProvenance::new(
            "https://example.org/recipe",
            &"a".repeat(40),
        )
        .unwrap(),
    );
    let refresh = database.begin_refresh().unwrap();
    refresh.record_provider_attempt(&new).unwrap();
    refresh.publish(&[], &test_fingerprint()).unwrap();
    drop(database);
    let database = Database::open(&path).unwrap();
    assert_eq!(database.provider_attempts("alpha", "1.0-1").unwrap(), [new]);
}

#[test]
fn malformed_version_four_migration_rolls_back_new_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paclang.db");
    let connection = create_version_four(&path);
    connection
        .execute_batch("ALTER TABLE provider_attempts DROP COLUMN recorded_at;")
        .unwrap();
    drop(connection);
    assert!(matches!(
        Database::open(&path),
        Err(StorageError::InvalidSchema(_))
    ));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        4
    );
    assert!(
        connection
            .prepare("SELECT recipe_location FROM provider_attempts")
            .is_err()
    );
    assert_eq!(table_row_count(&connection, "provider_attempts"), 1);
}
