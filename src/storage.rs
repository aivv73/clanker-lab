#[cfg(test)]
mod tests;

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

const MIGRATION_1: &str = "
    CREATE TABLE inventory_snapshots (
        id INTEGER PRIMARY KEY,
        published_at TEXT NOT NULL
    ) STRICT;
";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct MigrationVersion(u32);

impl MigrationVersion {
    const EMPTY: Self = Self(0);
    const CURRENT: Self = Self(1);

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

    pub(crate) fn has_published_snapshot(&self) -> Result<bool, StorageError> {
        self.connection
            .query_row(
                "SELECT 1 FROM inventory_snapshots ORDER BY id DESC LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(StorageError::Sqlite)
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
                MigrationVersion::CURRENT.as_database_value(),
            )
            .map_err(StorageError::Sqlite)?;
    }

    transaction
        .prepare("SELECT id, published_at FROM inventory_snapshots LIMIT 0")
        .map_err(StorageError::InvalidSchema)?;

    transaction.commit().map_err(StorageError::Sqlite)
}
