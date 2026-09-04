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

const RETAINED_SNAPSHOT_COUNT: u32 = 10;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MigrationVersion(u32);

impl MigrationVersion {
    const EMPTY: Self = Self(0);
    const VERSION_1: Self = Self(1);
    const VERSION_2: Self = Self(2);
    const CURRENT: Self = Self(3);

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

pub(crate) struct PublishedSnapshot {
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
        {
            let mut statement = transaction
                .prepare(
                    "SELECT origin, install_reason FROM installed_packages
                     WHERE snapshot_id = ?1 ORDER BY name",
                )
                .map_err(StorageError::Sqlite)?;
            let rows = statement
                .query_map([snapshot_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(StorageError::Sqlite)?;
            for row in rows {
                let (origin, install_reason) = row.map_err(StorageError::Sqlite)?;
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
            packages,
            fingerprint,
            read_guard: read_lock,
        }))
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

impl RefreshTransaction<'_> {
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
    connection
        .prepare(
            "SELECT inventory_snapshots.id, inventory_snapshots.published_at,
                    inventory_snapshots.inventory_fingerprint,
                    installed_packages.name, installed_packages.version,
                    installed_packages.package_base, installed_packages.origin,
                    installed_packages.install_reason, installed_packages.installed_bytes
             FROM inventory_snapshots
             LEFT JOIN installed_packages ON installed_packages.snapshot_id = inventory_snapshots.id
             LIMIT 0",
        )
        .map_err(StorageError::InvalidSchema)?;
    Ok(())
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
