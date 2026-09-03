#[cfg(test)]
mod tests;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::inventory::InstalledPackage;
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MigrationVersion(u32);

impl MigrationVersion {
    const EMPTY: Self = Self(0);
    const VERSION_1: Self = Self(1);
    const CURRENT: Self = Self(2);

    const fn from_database(value: u32) -> Self {
        Self(value)
    }

    const fn as_database_value(self) -> u32 {
        self.0
    }
}

#[derive(Debug)]
pub(crate) enum StorageError {
    DataDirectoryUnavailable,
    CreateDirectory(std::io::Error),
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
            Self::DataDirectoryUnavailable => formatter.write_str(
                "neither XDG_DATA_HOME nor HOME provides an absolute user data directory",
            ),
            Self::CreateDirectory(error) => {
                write!(formatter, "cannot create the data directory: {error}")
            }
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
}

impl Database {
    pub(crate) fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::CreateDirectory)?;
        }

        let mut connection = Connection::open(path).map_err(StorageError::Sqlite)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(StorageError::Sqlite)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(StorageError::Sqlite)?;
        migrate(&mut connection)?;

        Ok(Self { connection })
    }

    pub(crate) fn published_packages(&self) -> Result<Option<Vec<SnapshotPackage>>, StorageError> {
        let snapshot_id: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM inventory_snapshots ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::Sqlite)?;
        let Some(snapshot_id) = snapshot_id else {
            return Ok(None);
        };

        let mut statement = self
            .connection
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
        let mut packages = Vec::new();
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
        Ok(Some(packages))
    }

    pub(crate) fn publish_snapshot(
        &mut self,
        packages: &[InstalledPackage],
    ) -> Result<(), StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::Sqlite)?;
        transaction
            .execute(
                "INSERT INTO inventory_snapshots (published_at) VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                [],
            )
            .map_err(StorageError::Sqlite)?;
        let snapshot_id = transaction.last_insert_rowid();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO installed_packages (
                        snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(StorageError::Sqlite)?;
            for package in packages {
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
        transaction.commit().map_err(StorageError::Sqlite)
    }
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
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
        transaction
            .pragma_update(
                None,
                "user_version",
                MigrationVersion::CURRENT.as_database_value(),
            )
            .map_err(StorageError::Sqlite)?;
    }

    transaction
        .prepare(
            "SELECT inventory_snapshots.id, inventory_snapshots.published_at,
                    installed_packages.name, installed_packages.version,
                    installed_packages.package_base, installed_packages.origin,
                    installed_packages.install_reason, installed_packages.installed_bytes
             FROM inventory_snapshots
             LEFT JOIN installed_packages ON installed_packages.snapshot_id = inventory_snapshots.id
             LIMIT 0",
        )
        .map_err(StorageError::InvalidSchema)?;

    transaction.commit().map_err(StorageError::Sqlite)
}
