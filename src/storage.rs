#[cfg(test)]
mod tests;

use std::env;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::interrupt;
use crate::inventory::{InstalledPackage, InventoryFingerprint};
use crate::package::provider::{
    AnalysisTarget, AttemptOutcome, EvidenceGrade, ProviderAttempt, RecipeProvenance,
    SourceProvider,
};
use crate::package::{InstallReason, PackageOrigin, SnapshotPackage};

const MIGRATION_1: &str = "
    CREATE TABLE inventory_snapshots (
        id INTEGER PRIMARY KEY,
        published_at TEXT NOT NULL
    ) STRICT;
";

const MIGRATION_2: &str = "
    -- Version 1 snapshots were markers without package metadata.
    DELETE FROM inventory_snapshots;
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
";

const MIGRATION_3: &str = "
    ALTER TABLE inventory_snapshots ADD COLUMN inventory_fingerprint TEXT;
";

/// Migration 4 records source-provider evidence globally rather than per
/// snapshot, so later refreshes reuse successful analyses and snapshot
/// retention never prunes them (ADR 0005, ADR 0007, ADR 0008).
/// `analysis_targets` holds canonical source locations keyed by location,
/// revision, and subdirectory; `provider_attempts` holds one row per
/// package name, package version, and provider, preserving its latest outcome
/// including resolved candidates from different providers with no winner selected.
const MIGRATION_4: &str = "
    CREATE TABLE analysis_targets (
        id INTEGER PRIMARY KEY,
        location TEXT NOT NULL,
        revision TEXT NOT NULL,
        subdirectory TEXT NOT NULL,
        UNIQUE (location, revision, subdirectory)
    ) STRICT;
    CREATE TABLE provider_attempts (
        id INTEGER PRIMARY KEY,
        package_name TEXT NOT NULL,
        package_version TEXT NOT NULL,
        provider TEXT NOT NULL,
        outcome TEXT NOT NULL CHECK (outcome IN ('resolved', 'unresolved')),
        target_id INTEGER REFERENCES analysis_targets(id),
        evidence_grade TEXT CHECK (
            evidence_grade IN ('exact', 'reconstructed', 'fallback', 'override')
        ),
        failure_reason TEXT,
        recorded_at TEXT NOT NULL,
        UNIQUE (package_name, package_version, provider),
        CHECK (
            (outcome = 'resolved'
             AND target_id IS NOT NULL
             AND evidence_grade IS NOT NULL
             AND failure_reason IS NULL)
            OR (outcome = 'unresolved'
             AND target_id IS NULL
             AND evidence_grade IS NULL
             AND failure_reason IS NOT NULL)
        )
    ) STRICT;
";

// Recipe provenance is optional for attempts recorded before fetching existed.
const MIGRATION_5: &str = "
    ALTER TABLE provider_attempts ADD COLUMN recipe_location TEXT;
    ALTER TABLE provider_attempts ADD COLUMN recipe_commit TEXT
        CHECK ((recipe_location IS NULL) = (recipe_commit IS NULL));
";

/// Maximum accepted source-location bytes. Recipe locations are short URLs;
/// this stays generous while keeping error reports bounded.
const MAX_LOCATION_BYTES: usize = 4096;

/// Maximum accepted source-revision bytes.
const MAX_REVISION_BYTES: usize = 1024;

/// Maximum accepted target-subdirectory bytes.
const MAX_SUBDIRECTORY_BYTES: usize = 1024;

/// Maximum accepted provider-failure-reason bytes.
const MAX_REASON_BYTES: usize = 4096;

const RETAINED_SNAPSHOT_COUNT: u32 = 10;

const MIGRATION_6: &str = "
    ALTER TABLE provider_attempts ADD COLUMN source_checksum TEXT;
    CREATE TABLE source_acquisitions (
        target_id INTEGER NOT NULL REFERENCES analysis_targets(id),
        checksum TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('acquired', 'failed', 'resource_limited')),
        reason TEXT,
        downloaded_bytes INTEGER NOT NULL,
        extracted_bytes INTEGER NOT NULL,
        files INTEGER NOT NULL,
        verified_revision TEXT,
        content_sha256 TEXT,
        recorded_at TEXT NOT NULL,
        PRIMARY KEY (target_id, checksum)
    ) STRICT;
";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MigrationVersion(u32);

impl MigrationVersion {
    const EMPTY: Self = Self(0);
    const VERSION_1: Self = Self(1);
    const VERSION_2: Self = Self(2);
    const VERSION_3: Self = Self(3);
    const VERSION_4: Self = Self(4);
    const VERSION_5: Self = Self(5);
    const CURRENT: Self = Self(6);

    const fn from_database(value: u32) -> Self {
        Self(value)
    }

    const fn as_database_value(self) -> u32 {
        self.0
    }
}

#[derive(Debug)]
pub(crate) enum StorageError {
    Interrupted,
    DataDirectoryUnavailable,
    CreateDirectory(std::io::Error),
    OpenLockFile(std::io::Error),
    Lock(std::io::Error),
    UnsupportedSchemaVersion(MigrationVersion),
    InvalidSchema(rusqlite::Error),
    InvalidPackageMetadata {
        field: &'static str,
        value: String,
        message: &'static str,
    },
    InvalidProviderAttempt {
        field: &'static str,
        value: String,
        message: &'static str,
    },
    Sqlite(rusqlite::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted => formatter.write_str("inventory refresh was interrupted"),
            Self::DataDirectoryUnavailable => formatter.write_str(
                "neither XDG_DATA_HOME nor HOME provides an absolute user data directory",
            ),
            Self::CreateDirectory(error) => {
                write!(formatter, "cannot create the data directory: {error}")
            }
            Self::OpenLockFile(error) => {
                write!(formatter, "cannot open the snapshot lock file: {error}")
            }
            Self::Lock(error) => write!(formatter, "cannot lock inventory snapshots: {error}"),
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "database schema version {} is newer than supported version {}",
                version.as_database_value(),
                MigrationVersion::CURRENT.as_database_value()
            ),
            Self::InvalidSchema(error) => {
                write!(formatter, "database schema is invalid: {error}")
            }
            Self::InvalidPackageMetadata {
                field,
                value,
                message,
            } => write!(
                formatter,
                "published package has invalid {field} {value:?}: {message}"
            ),
            Self::InvalidProviderAttempt {
                field,
                value,
                message,
            } => write!(
                formatter,
                "provider attempt has invalid {field} {value:?}: {message}"
            ),
            Self::Sqlite(error) => write!(formatter, "SQLite returned an error: {error}"),
        }
    }
}

impl StorageError {
    pub(crate) const fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

pub(crate) fn database_path() -> Result<PathBuf, StorageError> {
    let data_home = absolute_environment_path("XDG_DATA_HOME")
        .or_else(|| absolute_environment_path("HOME").map(|home| home.join(".local/share")))
        .ok_or(StorageError::DataDirectoryUnavailable)?;

    Ok(data_home.join("paclang/paclang.db"))
}

fn absolute_environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

pub(crate) struct Database {
    connection: Connection,
    snapshot_lock_path: PathBuf,
}

pub(crate) struct ProviderPackage {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) attempts: Vec<ProviderAttempt>,
    pub(crate) acquisitions: Vec<Option<crate::acquisition::AcquisitionRecord>>,
}

pub(crate) struct PublishedSnapshot {
    pub(crate) provider_packages: Vec<ProviderPackage>,
    pub(crate) packages: Vec<SnapshotPackage>,
    /// `None` for snapshots published before migration 3 recorded fingerprints.
    pub(crate) fingerprint: Option<InventoryFingerprint>,
    /// Shared sidecar lock guard: must be held through report construction so
    /// retention cannot prune the snapshot being read. Never read by design;
    /// the field exists to extend the guard's lifetime to the report.
    #[allow(dead_code)]
    read_guard: SnapshotLock,
}

pub(crate) struct RefreshTransaction<'connection> {
    transaction: Transaction<'connection>,
    snapshot_lock_path: PathBuf,
}

impl Database {
    pub(crate) fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::CreateDirectory)?;
        }

        let mut connection = Connection::open(path).map_err(StorageError::Sqlite)?;
        connection
            .busy_handler(Some(retry_sqlite_lock))
            .map_err(StorageError::Sqlite)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(StorageError::Sqlite)?;
        migrate(&mut connection)?;
        let snapshot_lock_path = path.with_extension("db.lock");
        SnapshotLock::open(&snapshot_lock_path)?;

        Ok(Self {
            connection,
            snapshot_lock_path,
        })
    }

    pub(crate) fn published_snapshot(&self) -> Result<Option<PublishedSnapshot>, StorageError> {
        self.read_snapshot(false)
    }

    pub(crate) fn published_provider_snapshot(
        &self,
    ) -> Result<Option<PublishedSnapshot>, StorageError> {
        self.read_snapshot(true)
    }

    fn read_snapshot(
        &self,
        include_providers: bool,
    ) -> Result<Option<PublishedSnapshot>, StorageError> {
        let read_lock = SnapshotLock::shared(&self.snapshot_lock_path)?;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(StorageError::Sqlite)?;
        let snapshot: Option<(i64, Option<String>)> = transaction
            .query_row(
                "SELECT id, inventory_fingerprint FROM inventory_snapshots
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StorageError::Sqlite)?;
        let Some((snapshot_id, fingerprint)) = snapshot else {
            transaction.commit().map_err(StorageError::Sqlite)?;
            return Ok(None);
        };

        let mut packages = Vec::new();
        let mut provider_packages = Vec::new();
        {
            let mut statement = transaction
                .prepare(
                    "SELECT origin, install_reason, name, version FROM installed_packages
                     WHERE snapshot_id = ?1 ORDER BY name",
                )
                .map_err(StorageError::Sqlite)?;
            let rows = statement
                .query_map([snapshot_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(StorageError::Sqlite)?;
            for row in rows {
                let (origin, install_reason, name, version) = row.map_err(StorageError::Sqlite)?;
                if include_providers {
                    let attempts = self.provider_attempts(&name, &version)?;
                    let acquisitions = attempts
                        .iter()
                        .map(|attempt| match &attempt.outcome {
                            AttemptOutcome::Resolved { target, .. } => {
                                read_acquisition(&transaction, target)
                            }
                            _ => Ok(None),
                        })
                        .collect::<Result<Vec<_>, StorageError>>()?;
                    provider_packages.push(ProviderPackage {
                        acquisitions,
                        name,
                        version,
                        attempts,
                    });
                }
                let parsed_origin = PackageOrigin::parse(&origin).map_err(|message| {
                    StorageError::InvalidPackageMetadata {
                        field: "origin",
                        value: origin,
                        message,
                    }
                })?;
                let parsed_install_reason =
                    InstallReason::parse(&install_reason).map_err(|message| {
                        StorageError::InvalidPackageMetadata {
                            field: "install reason",
                            value: install_reason,
                            message,
                        }
                    })?;
                packages.push(SnapshotPackage {
                    origin: parsed_origin,
                    install_reason: parsed_install_reason,
                });
            }
        }
        let fingerprint = match fingerprint {
            Some(value) => Some(InventoryFingerprint::parse(&value).map_err(|message| {
                StorageError::InvalidPackageMetadata {
                    field: "inventory fingerprint",
                    value,
                    message,
                }
            })?),
            None => None,
        };
        transaction.commit().map_err(StorageError::Sqlite)?;
        Ok(Some(PublishedSnapshot {
            provider_packages,
            packages,
            fingerprint,
            read_guard: read_lock,
        }))
    }

    /// Every recorded attempt for one installed package version, ordered by
    /// provider name. Attempts are global, so no snapshot lock is needed:
    /// retention prunes snapshots, never attempts.
    pub(crate) fn provider_attempts(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<ProviderAttempt>, StorageError> {
        read_provider_attempts(&self.connection, name, version)
    }

    pub(crate) fn begin_refresh(&mut self) -> Result<RefreshTransaction<'_>, StorageError> {
        let transaction = match self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(transaction) => transaction,
            Err(_) if interrupt::is_requested() => return Err(StorageError::Interrupted),
            Err(error) => return Err(StorageError::Sqlite(error)),
        };
        Ok(RefreshTransaction {
            transaction,
            snapshot_lock_path: self.snapshot_lock_path.clone(),
        })
    }
}

fn read_acquisition(
    connection: &Connection,
    target: &AnalysisTarget,
) -> Result<Option<crate::acquisition::AcquisitionRecord>, StorageError> {
    connection.query_row("SELECT state, reason, downloaded_bytes, extracted_bytes, files, verified_revision, content_sha256
        FROM source_acquisitions JOIN analysis_targets ON analysis_targets.id = source_acquisitions.target_id
        WHERE location = ?1 AND revision = ?2 AND subdirectory = ?3 AND checksum = ?4",
        rusqlite::params![target.location, optional_text(target.revision.as_deref()), optional_text(target.subdirectory.as_deref()), optional_text(target.checksum.as_deref())],
        |row| Ok(crate::acquisition::AcquisitionRecord { state: row.get(0)?, reason: row.get(1)?, downloaded_bytes: row.get(2)?, extracted_bytes: row.get(3)?, files: row.get(4)?, verified_revision: row.get(5)?, content_sha256: row.get(6)? }))
        .optional().map_err(StorageError::Sqlite)
}

fn read_provider_attempts(
    connection: &Connection,
    package_name: &str,
    package_version: &str,
) -> Result<Vec<ProviderAttempt>, StorageError> {
    let mut attempts = Vec::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT provider_attempts.provider, provider_attempts.outcome,
                            analysis_targets.location, analysis_targets.revision,
                            analysis_targets.subdirectory,
                            provider_attempts.evidence_grade, provider_attempts.failure_reason,
                            provider_attempts.recipe_location, provider_attempts.recipe_commit,
                            provider_attempts.source_checksum
                     FROM provider_attempts
                     LEFT JOIN analysis_targets
                       ON analysis_targets.id = provider_attempts.target_id
                     WHERE provider_attempts.package_name = ?1
                       AND provider_attempts.package_version = ?2
                     ORDER BY provider_attempts.provider",
            )
            .map_err(StorageError::Sqlite)?;
        let rows = statement
            .query_map([package_name, package_version], |row| {
                Ok(StoredProviderAttempt {
                    provider: row.get(0)?,
                    outcome: row.get(1)?,
                    location: row.get(2)?,
                    revision: row.get(3)?,
                    subdirectory: row.get(4)?,
                    grade: row.get(5)?,
                    reason: row.get(6)?,
                    recipe_location: row.get(7)?,
                    recipe_commit: row.get(8)?,
                    checksum: row.get(9)?,
                })
            })
            .map_err(StorageError::Sqlite)?;
        for row in rows {
            attempts.push(
                row.map_err(StorageError::Sqlite)?
                    .into_attempt(package_name, package_version)?,
            );
        }
    }
    Ok(attempts)
}

impl RefreshTransaction<'_> {
    pub(crate) fn provider_attempts(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<ProviderAttempt>, StorageError> {
        read_provider_attempts(&self.transaction, name, version)
    }

    pub(crate) fn record_acquisition(
        &self,
        target: &AnalysisTarget,
        result: &crate::acquisition::AcquisitionRecord,
    ) -> Result<(), StorageError> {
        self.transaction.execute(
            "INSERT INTO source_acquisitions (target_id, checksum, state, reason, downloaded_bytes, extracted_bytes, files, verified_revision, content_sha256, recorded_at)
             SELECT id, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, strftime('%Y-%m-%dT%H:%M:%fZ', 'now') FROM analysis_targets
             WHERE location = ?1 AND revision = ?2 AND subdirectory = ?3
             ON CONFLICT(target_id, checksum) DO UPDATE SET state=excluded.state, reason=excluded.reason,
             downloaded_bytes=excluded.downloaded_bytes, extracted_bytes=excluded.extracted_bytes, files=excluded.files,
             verified_revision=excluded.verified_revision, content_sha256=excluded.content_sha256, recorded_at=excluded.recorded_at",
            rusqlite::params![target.location, optional_text(target.revision.as_deref()), optional_text(target.subdirectory.as_deref()),
                optional_text(target.checksum.as_deref()), result.state, result.reason, result.downloaded_bytes, result.extracted_bytes,
                result.files, result.verified_revision, result.content_sha256]
        ).map_err(StorageError::Sqlite)?;
        Ok(())
    }

    pub(crate) fn publish(
        self,
        packages: &[InstalledPackage],
        fingerprint: &InventoryFingerprint,
    ) -> Result<(), StorageError> {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        self.transaction
            .execute(
                "INSERT INTO inventory_snapshots (published_at, inventory_fingerprint)
                 VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?1)",
                [fingerprint.as_str()],
            )
            .map_err(StorageError::Sqlite)?;
        let snapshot_id = self.transaction.last_insert_rowid();
        {
            let mut statement = self
                .transaction
                .prepare(
                    "INSERT INTO installed_packages (
                        snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(StorageError::Sqlite)?;
            for package in packages {
                if interrupt::is_requested() {
                    return Err(StorageError::Interrupted);
                }
                statement
                    .execute((
                        snapshot_id,
                        package.name(),
                        package.version(),
                        package.package_base(),
                        package.origin(),
                        package.install_reason(),
                        package.installed_bytes(),
                    ))
                    .map_err(StorageError::Sqlite)?;
            }
        }
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        let _retention_lock = SnapshotLock::exclusive(&self.snapshot_lock_path)?;
        self.transaction
            .execute(
                "DELETE FROM inventory_snapshots
                 WHERE id NOT IN (
                    SELECT id FROM inventory_snapshots ORDER BY id DESC LIMIT ?1
                 )",
                [RETAINED_SNAPSHOT_COUNT],
            )
            .map_err(StorageError::Sqlite)?;
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        self.transaction.commit().map_err(StorageError::Sqlite)
    }

    /// Whether a resolved attempt is already recorded for one installed
    /// package version and provider. Recipe refresh skips fetching for such
    /// members and reuses the stored evidence; unresolved attempts are
    /// retried so transient failures and newly published recipes resolve.
    pub(crate) fn has_resolved_attempt(
        &self,
        package_name: &str,
        package_version: &str,
        provider: &SourceProvider,
    ) -> Result<bool, StorageError> {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        let outcome: Option<String> = self
            .transaction
            .query_row(
                "SELECT outcome FROM provider_attempts
                 WHERE package_name = ?1 AND package_version = ?2 AND provider = ?3",
                (package_name, package_version, provider.as_str()),
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::Sqlite)?;
        Ok(outcome.as_deref() == Some("resolved"))
    }

    /// Record one provider attempt in the refresh transaction. Re-recording
    /// the same package name, package version, and provider replaces the
    /// previous outcome, so later refreshes reuse successful analyses by
    /// overwriting them unchanged. The row commits or rolls back with the
    /// surrounding refresh transaction.
    pub(crate) fn record_provider_attempt(
        &self,
        attempt: &ProviderAttempt,
    ) -> Result<(), StorageError> {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        validate_provider_attempt(attempt)?;
        let (target_id, grade, reason) = match &attempt.outcome {
            AttemptOutcome::Resolved { target, grade } => {
                self.transaction
                    .execute(
                        "INSERT INTO analysis_targets (location, revision, subdirectory)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT (location, revision, subdirectory) DO NOTHING",
                        (
                            target.location.as_str(),
                            optional_text(target.revision.as_deref()),
                            optional_text(target.subdirectory.as_deref()),
                        ),
                    )
                    .map_err(StorageError::Sqlite)?;
                let target_id: i64 = self
                    .transaction
                    .query_row(
                        "SELECT id FROM analysis_targets
                         WHERE location = ?1 AND revision = ?2 AND subdirectory = ?3",
                        (
                            target.location.as_str(),
                            optional_text(target.revision.as_deref()),
                            optional_text(target.subdirectory.as_deref()),
                        ),
                        |row| row.get(0),
                    )
                    .map_err(StorageError::Sqlite)?;
                (Some(target_id), Some(grade.as_str()), None)
            }
            AttemptOutcome::Unresolved { reason } => (None, None, Some(reason.as_str())),
        };
        self.transaction
            .execute(
                "INSERT INTO provider_attempts (
                    package_name, package_version, provider, outcome,
                    target_id, evidence_grade, failure_reason, recorded_at, recipe_location, recipe_commit, source_checksum
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?8, ?9, ?10
                 )
                 ON CONFLICT (package_name, package_version, provider) DO UPDATE SET
                    outcome = excluded.outcome,
                    target_id = excluded.target_id,
                    evidence_grade = excluded.evidence_grade,
                    failure_reason = excluded.failure_reason,
                    recorded_at = excluded.recorded_at,
                    recipe_location = excluded.recipe_location,
                    recipe_commit = excluded.recipe_commit,
                    source_checksum = excluded.source_checksum",
                (
                    attempt.package_name.as_str(),
                    attempt.package_version.as_str(),
                    attempt.provider.as_str(),
                    match &attempt.outcome {
                        AttemptOutcome::Resolved { .. } => "resolved",
                        AttemptOutcome::Unresolved { .. } => "unresolved",
                    },
                    target_id,
                    grade,
                    reason,
                    attempt.recipe.as_ref().map(RecipeProvenance::location),
                    attempt.recipe.as_ref().map(RecipeProvenance::commit),
                    match &attempt.outcome { AttemptOutcome::Resolved { target, .. } => target.checksum.as_deref(), _ => None },
                ),
            )
            .map_err(StorageError::Sqlite)?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map(MigrationVersion::from_database)
        .map_err(StorageError::Sqlite)?;

    if version > MigrationVersion::CURRENT {
        return Err(StorageError::UnsupportedSchemaVersion(version));
    }
    if version < MigrationVersion::CURRENT {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::Sqlite)?;
        let version = transaction
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .map(MigrationVersion::from_database)
            .map_err(StorageError::Sqlite)?;
        if version > MigrationVersion::CURRENT {
            return Err(StorageError::UnsupportedSchemaVersion(version));
        }
        if version == MigrationVersion::EMPTY {
            transaction
                .execute_batch(MIGRATION_1)
                .map_err(StorageError::Sqlite)?;
            transaction
                .pragma_update(
                    None,
                    "user_version",
                    MigrationVersion::VERSION_1.as_database_value(),
                )
                .map_err(StorageError::Sqlite)?;
        }

        if version <= MigrationVersion::VERSION_1 {
            transaction
                .execute_batch(MIGRATION_2)
                .map_err(StorageError::Sqlite)?;
        }
        if version <= MigrationVersion::VERSION_2 {
            transaction
                .execute_batch(MIGRATION_3)
                .map_err(StorageError::Sqlite)?;
        }
        if version <= MigrationVersion::VERSION_3 {
            transaction
                .execute_batch(MIGRATION_4)
                .map_err(StorageError::Sqlite)?;
        }
        if version <= MigrationVersion::VERSION_4 {
            transaction
                .execute_batch(MIGRATION_5)
                .map_err(StorageError::Sqlite)?;
        }
        if version <= MigrationVersion::VERSION_5 {
            transaction
                .execute_batch(MIGRATION_6)
                .map_err(StorageError::Sqlite)?;
        }
        transaction
            .pragma_update(
                None,
                "user_version",
                MigrationVersion::CURRENT.as_database_value(),
            )
            .map_err(StorageError::Sqlite)?;
        validate_schema(&transaction)?;
        transaction.commit().map_err(StorageError::Sqlite)?;
    } else {
        validate_schema(connection)?;
    }

    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<(), StorageError> {
    connection.prepare("SELECT target_id, checksum, state, reason, downloaded_bytes, extracted_bytes, files, verified_revision, content_sha256, recorded_at FROM source_acquisitions LIMIT 0").map_err(StorageError::InvalidSchema)?;
    connection
        .prepare(
            "SELECT inventory_snapshots.id, inventory_snapshots.published_at,
                    inventory_snapshots.inventory_fingerprint,
                    installed_packages.name, installed_packages.version,
                    installed_packages.package_base, installed_packages.origin,
                    installed_packages.install_reason, installed_packages.installed_bytes,
                    analysis_targets.id, analysis_targets.location, analysis_targets.revision,
                    analysis_targets.subdirectory,
                    provider_attempts.package_name, provider_attempts.package_version,
                    provider_attempts.provider, provider_attempts.outcome,
                    provider_attempts.target_id, provider_attempts.evidence_grade,
                    provider_attempts.failure_reason, provider_attempts.recorded_at,
                    provider_attempts.recipe_location, provider_attempts.recipe_commit, provider_attempts.source_checksum
             FROM inventory_snapshots
             LEFT JOIN installed_packages ON installed_packages.snapshot_id = inventory_snapshots.id
             LEFT JOIN analysis_targets ON 1 = 0
             LEFT JOIN provider_attempts ON 1 = 0
             LIMIT 0",
        )
        .map_err(StorageError::InvalidSchema)?;
    Ok(())
}

/// Absent revision and subdirectory persist as empty strings; the read path
/// maps them back to `None`.
fn optional_text(value: Option<&str>) -> &str {
    value.unwrap_or("")
}

/// Validate a provider attempt before writing: package names and versions
/// use the same alphabet as the inventory capture, while locations,
/// revisions, subdirectories, and failure reasons must be bounded and free
/// of control characters so reports can render them safely.
fn validate_provider_attempt(attempt: &ProviderAttempt) -> Result<(), StorageError> {
    crate::expac_parser::validate_package_name(&attempt.package_name).map_err(|message| {
        StorageError::InvalidProviderAttempt {
            field: "package name",
            value: attempt.package_name.clone(),
            message,
        }
    })?;
    crate::expac_parser::validate_package_version(&attempt.package_version).map_err(|message| {
        StorageError::InvalidProviderAttempt {
            field: "package version",
            value: attempt.package_version.clone(),
            message,
        }
    })?;
    match &attempt.outcome {
        AttemptOutcome::Resolved { target, .. } => {
            if let Some(checksum) = &target.checksum {
                crate::acquisition::validate_checksum(checksum).map_err(|_| {
                    StorageError::InvalidProviderAttempt {
                        field: "checksum",
                        value: checksum.clone(),
                        message: "invalid source checksum",
                    }
                })?;
            }
            validate_bounded_text(&target.location, "location", MAX_LOCATION_BYTES, false)?;
            if let Some(revision) = &target.revision {
                validate_bounded_text(revision, "revision", MAX_REVISION_BYTES, true)?;
            }
            if let Some(subdirectory) = &target.subdirectory {
                validate_bounded_text(subdirectory, "subdirectory", MAX_SUBDIRECTORY_BYTES, true)?;
            }
        }
        AttemptOutcome::Unresolved { reason } => {
            validate_bounded_text(reason, "failure reason", MAX_REASON_BYTES, false)?;
        }
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    field: &'static str,
    limit: usize,
    allow_empty: bool,
) -> Result<(), StorageError> {
    let invalid = |message: &'static str| StorageError::InvalidProviderAttempt {
        field,
        value: value.to_owned(),
        message,
    };
    if value.is_empty() {
        if allow_empty {
            return Ok(());
        }
        return Err(invalid("value is empty"));
    }
    if value.len() > limit {
        return Err(invalid("value exceeds the length limit"));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid("value contains a control character"));
    }
    Ok(())
}

// Raw SQLite fields are decoded together before entering the typed storage API.
// Raw decoding remains for the future inspection report.
#[allow(dead_code)]
struct StoredProviderAttempt {
    provider: String,
    outcome: String,
    location: Option<String>,
    revision: Option<String>,
    subdirectory: Option<String>,
    grade: Option<String>,
    reason: Option<String>,
    recipe_location: Option<String>,
    recipe_commit: Option<String>,
    checksum: Option<String>,
}

impl StoredProviderAttempt {
    /// Validate stored values with the same rules as the write boundary.
    // Used by the inspection read API, not recipe refresh.
    #[allow(dead_code)]
    fn into_attempt(
        self,
        package_name: &str,
        package_version: &str,
    ) -> Result<ProviderAttempt, StorageError> {
        let Self {
            provider,
            outcome,
            location,
            revision,
            subdirectory,
            grade,
            reason,
            recipe_location,
            recipe_commit,
            checksum,
        } = self;
        let invalid = |field: &'static str, value: String, message: &'static str| {
            StorageError::InvalidProviderAttempt {
                field,
                value,
                message,
            }
        };
        let provider = SourceProvider::parse(&provider)
            .map_err(|message| invalid("provider", provider, message))?;
        let outcome = match outcome.as_str() {
            "resolved" => {
                let (location, revision, subdirectory, grade) =
                    match (location, revision, subdirectory, grade, reason) {
                        (Some(location), Some(revision), Some(subdirectory), Some(grade), None) => {
                            (location, revision, subdirectory, grade)
                        }
                        _ => {
                            return Err(invalid(
                                "outcome",
                                outcome,
                                "a resolved attempt must reference one target and grade",
                            ));
                        }
                    };
                let grade = EvidenceGrade::parse(&grade)
                    .map_err(|message| invalid("evidence grade", grade, message))?;
                AttemptOutcome::Resolved {
                    target: AnalysisTarget {
                        location,
                        revision: (!revision.is_empty()).then_some(revision),
                        subdirectory: (!subdirectory.is_empty()).then_some(subdirectory),
                        checksum,
                    },
                    grade,
                }
            }
            "unresolved" => match (location, revision, subdirectory, grade, reason) {
                (None, None, None, None, Some(reason)) => AttemptOutcome::Unresolved { reason },
                _ => {
                    return Err(invalid(
                        "outcome",
                        outcome,
                        "an unresolved attempt must carry only a failure reason",
                    ));
                }
            },
            _ => {
                return Err(invalid(
                    "outcome",
                    outcome,
                    "outcome is neither resolved nor unresolved",
                ));
            }
        };
        let recipe = match (recipe_location, recipe_commit) {
            (None, None) => None,
            (Some(location), Some(commit)) => Some(
                RecipeProvenance::new(&location, &commit)
                    .map_err(|message| invalid("recipe provenance", location, message))?,
            ),
            _ => {
                return Err(invalid(
                    "recipe provenance",
                    String::new(),
                    "incomplete recipe provenance",
                ));
            }
        };
        let attempt = ProviderAttempt {
            package_name: package_name.to_owned(),
            package_version: package_version.to_owned(),
            provider,
            outcome,
            recipe,
        };
        validate_provider_attempt(&attempt)?;
        Ok(attempt)
    }
}

fn retry_sqlite_lock(attempt: i32) -> bool {
    if interrupt::is_requested() || attempt >= 500 {
        return false;
    }
    std::thread::sleep(Duration::from_millis(10));
    true
}

struct SnapshotLock(File);

impl SnapshotLock {
    fn open(path: &Path) -> Result<File, StorageError> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(StorageError::OpenLockFile)
    }

    fn shared(path: &Path) -> Result<Self, StorageError> {
        Self::acquire(Self::open(path)?, FileExt::try_lock_shared)
    }

    fn exclusive(path: &Path) -> Result<Self, StorageError> {
        Self::acquire(Self::open(path)?, FileExt::try_lock_exclusive)
    }

    fn acquire(
        file: File,
        try_lock: fn(&File) -> std::io::Result<()>,
    ) -> Result<Self, StorageError> {
        loop {
            match try_lock(&file) {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if interrupt::is_requested() {
                        return Err(StorageError::Interrupted);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(StorageError::Lock(error)),
            }
        }
    }
}

impl Drop for SnapshotLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
