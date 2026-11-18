//! Host-side search history persisted in SQLite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pstart_core::{
    limits::{MAX_PATH_UTF16, MAX_QUERY_CHARS},
    FileItem,
};
use rusqlite::{params, Connection, OptionalExtension};

/// Maximum rows returned by [`HistoryStore::recent`].
pub const MAX_HISTORY_RESULTS: u32 = 50;

/// Current SQLite `user_version`.
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V1: &str = "CREATE TABLE IF NOT EXISTS search_history (
        query TEXT PRIMARY KEY NOT NULL,
        uses INTEGER NOT NULL,
        last_used INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS mru (
        path TEXT PRIMARY KEY NOT NULL,
        uses INTEGER NOT NULL,
        last_used INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS aliases (
        name TEXT PRIMARY KEY NOT NULL,
        target TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS settings (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    );";

/// A previous query and its usage count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Query text.
    pub query: String,
    /// Number of times the query was used.
    pub uses: u32,
    /// Unix timestamp of the last use.
    pub last_used: i64,
}

/// A recently used file and its weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MruEntry {
    /// Full item path.
    pub path: String,
    /// Number of successful opens.
    pub uses: u32,
    /// Unix timestamp of the last open.
    pub last_used: i64,
}

fn migrate_v2(connection: &Connection) -> Result<(), HistoryError> {
    let result = connection.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE INDEX IF NOT EXISTS idx_history_last_used
            ON search_history(last_used DESC);
         PRAGMA user_version = 2;
         COMMIT;",
    );
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    result?;
    Ok(())
}

/// History storage failure.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// SQLite failure.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// Query text was empty, too long, or contained NUL.
    #[error("invalid history query: {0}")]
    InvalidQuery(String),
    /// Path was empty, too long, or contained NUL.
    #[error("invalid MRU path")]
    InvalidPath,
    /// Alias name or target was invalid.
    #[error("invalid alias")]
    InvalidAlias,
    /// Setting key or value was invalid.
    #[error("invalid setting")]
    InvalidSetting,
    /// Database schema was written by a newer version.
    #[error("unsupported schema version: {0}")]
    Schema(i64),
    /// Filesystem failure while pruning backups.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Result limit was outside `1..=MAX_HISTORY_RESULTS`.
    #[error("history limit must be in 1..={MAX_HISTORY_RESULTS}")]
    InvalidLimit,
}

/// SQLite-backed query history.
pub struct HistoryStore {
    connection: Connection,
    path: Option<PathBuf>,
}

impl HistoryStore {
    /// Open or create a history database at `path`.
    pub fn open(path: &Path) -> Result<Self, HistoryError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, Some(path.to_path_buf()))
    }

    /// Open a private in-memory database.
    pub fn open_in_memory() -> Result<Self, HistoryError> {
        Self::from_connection(Connection::open_in_memory()?, None)
    }

    /// Filesystem path of this database, or `None` for in-memory stores.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn from_connection(
        connection: Connection,
        path: Option<PathBuf>,
    ) -> Result<Self, HistoryError> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                connection.execute_batch(SCHEMA_V1)?;
                connection.execute_batch("PRAGMA user_version = 1")?;
                migrate_v2(&connection)?;
            }
            1 => migrate_v2(&connection)?,
            SCHEMA_VERSION => {}
            other => return Err(HistoryError::Schema(other)),
        }
        Ok(Self { connection, path })
    }

    /// Restore all tables from a backup database, replacing current rows.
    pub fn restore_from(&mut self, path: &Path) -> Result<(), HistoryError> {
        let source = path.to_str().ok_or(HistoryError::InvalidPath)?;
        self.connection
            .execute("ATTACH DATABASE ?1 AS backup", params![source])?;
        let result = self.copy_from_backup();
        let detach = self.connection.execute("DETACH DATABASE backup", []);
        result?;
        detach?;
        Ok(())
    }

    fn copy_from_backup(&mut self) -> Result<(), HistoryError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA backup.user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(HistoryError::Schema(version));
        }
        let result = self.connection.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM search_history;
             INSERT INTO search_history
                SELECT query, uses, last_used FROM backup.search_history;
             DELETE FROM mru;
             INSERT INTO mru SELECT path, uses, last_used FROM backup.mru;
             DELETE FROM aliases;
             INSERT INTO aliases SELECT name, target FROM backup.aliases;
             DELETE FROM settings;
             INSERT INTO settings SELECT key, value FROM backup.settings;
             COMMIT;",
        );
        if result.is_err() {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
        result?;
        Ok(())
    }

    /// Delete one pre-restore backup belonging to this database.
    pub fn delete_restore_backup(&self, path: &Path) -> Result<(), HistoryError> {
        let database = self.path.as_ref().ok_or(HistoryError::InvalidPath)?;
        let parent = database.parent().expect("database path has a parent");
        let stem = database.file_stem().expect("database path has a stem");
        let prefix = format!("{}.pre-restore-", stem.to_string_lossy());
        let valid = path.parent() == Some(parent)
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix));
        if !valid {
            return Err(HistoryError::InvalidPath);
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// List pre-restore backups, newest first.
    pub fn pre_restore_backups(&self, limit: usize) -> Result<Vec<PathBuf>, HistoryError> {
        let Some(database) = &self.path else {
            return Ok(Vec::new());
        };
        let parent = database.parent().expect("database path has a parent");
        let stem = database.file_stem().expect("database path has a stem");
        let prefix = format!("{}.pre-restore-", stem.to_string_lossy());
        let mut backups = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                backups.push(entry.path());
            }
        }
        backups.sort();
        backups.reverse();
        backups.truncate(limit);
        Ok(backups)
    }

    /// Delete old `<db>.pre-restore-*` backups, keeping the newest `keep`.
    pub fn prune_pre_restore_backups(&self, keep: usize) -> Result<usize, HistoryError> {
        let Some(database) = &self.path else {
            return Ok(0);
        };
        let parent = database.parent().expect("database path has a parent");
        let stem = database.file_stem().expect("database path has a stem");
        let prefix = format!("{}.pre-restore-", stem.to_string_lossy());
        let mut backups = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                backups.push(entry.path());
            }
        }
        backups.sort();
        let remove = backups.len().saturating_sub(keep);
        for path in backups.into_iter().take(remove) {
            std::fs::remove_file(path)?;
        }
        Ok(remove)
    }

    /// Back up the database to a new file using `VACUUM INTO`.
    pub fn backup_to(&self, path: &Path) -> Result<(), HistoryError> {
        let target = path.to_str().ok_or(HistoryError::InvalidPath)?;
        self.connection.execute("VACUUM INTO ?1", params![target])?;
        Ok(())
    }

    /// Record one use of `query` at `now` (Unix seconds).
    pub fn record(&mut self, query: &str, now: i64) -> Result<(), HistoryError> {
        let query = validate_query(query)?;
        self.connection.execute(
            "INSERT INTO search_history(query, uses, last_used) VALUES (?1, 1, ?2)
             ON CONFLICT(query) DO UPDATE SET
                uses = uses + 1,
                last_used = excluded.last_used",
            params![query, now],
        )?;
        Ok(())
    }

    /// Delete one history row. Returns whether a row existed.
    pub fn remove(&mut self, query: &str) -> Result<bool, HistoryError> {
        let query = validate_query(query)?;
        let removed = self.connection.execute(
            "DELETE FROM search_history WHERE query = ?1",
            params![query],
        )?;
        Ok(removed > 0)
    }

    /// Delete all history rows. Returns the number removed.
    pub fn clear_history(&mut self) -> Result<usize, HistoryError> {
        let removed = self.connection.execute("DELETE FROM search_history", [])?;
        Ok(removed)
    }

    /// Record one successful open of `path` at `now`.
    pub fn record_mru(&mut self, path: &str, now: i64) -> Result<(), HistoryError> {
        let path = validate_path(path)?;
        self.connection.execute(
            "INSERT INTO mru(path, uses, last_used) VALUES (?1, 1, ?2)
             ON CONFLICT(path) DO UPDATE SET
                uses = uses + 1,
                last_used = excluded.last_used",
            params![path, now],
        )?;
        Ok(())
    }

    /// Weight for `path`; zero when it has never been opened.
    pub fn mru_score(&self, path: &str) -> Result<u32, HistoryError> {
        let path = validate_path(path)?;
        let score = self
            .connection
            .query_row(
                "SELECT uses FROM mru WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(score.unwrap_or(0))
    }

    /// Most recently used paths, newest first.
    pub fn recent_mru(&self, limit: u32) -> Result<Vec<MruEntry>, HistoryError> {
        if limit == 0 || limit > MAX_HISTORY_RESULTS {
            return Err(HistoryError::InvalidLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT path, uses, last_used FROM mru
             ORDER BY last_used DESC, uses DESC, path ASC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(MruEntry {
                path: row.get(0)?,
                uses: row.get(1)?,
                last_used: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Most recently used queries, newest first.
    pub fn recent(&self, limit: u32) -> Result<Vec<HistoryEntry>, HistoryError> {
        if limit == 0 || limit > MAX_HISTORY_RESULTS {
            return Err(HistoryError::InvalidLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT query, uses, last_used FROM search_history
             ORDER BY last_used DESC, uses DESC, query ASC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(HistoryEntry {
                query: row.get(0)?,
                uses: row.get(1)?,
                last_used: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Batch MRU weights for `paths`; missing paths are omitted.
    pub fn mru_scores(&self, paths: &[String]) -> Result<HashMap<String, u32>, HistoryError> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let mut unique = paths.iter().map(String::as_str).collect::<Vec<_>>();
        unique.sort_unstable();
        unique.dedup();
        let placeholders = vec!["?"; unique.len()].join(",");
        let sql = format!("SELECT path, uses FROM mru WHERE path IN ({placeholders})");
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(unique.iter().copied()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })?;
        let mut scores = HashMap::new();
        for row in rows {
            let (path, uses) = row?;
            scores.insert(path, uses);
        }
        Ok(scores)
    }

    /// Stable-sort `items` by descending MRU weight; ties keep server order.
    pub fn rank_items(&self, items: &mut [FileItem]) -> Result<(), HistoryError> {
        let paths = items
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>();
        let scores = self.mru_scores(&paths)?;
        items.sort_by_key(|item| std::cmp::Reverse(scores.get(&item.path).copied().unwrap_or(0)));
        Ok(())
    }

    /// Create or replace an alias.
    pub fn set_alias(&mut self, name: &str, target: &str) -> Result<(), HistoryError> {
        let name = validate_alias(name)?;
        let target = validate_alias(target)?;
        self.connection.execute(
            "INSERT INTO aliases(name, target) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET target = excluded.target",
            params![name, target],
        )?;
        Ok(())
    }

    /// Resolve an exact alias name.
    pub fn resolve_alias(&self, name: &str) -> Result<Option<String>, HistoryError> {
        let name = validate_alias(name)?;
        self.connection
            .query_row(
                "SELECT target FROM aliases WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Remove an alias. Returns whether it existed.
    pub fn remove_alias(&mut self, name: &str) -> Result<bool, HistoryError> {
        let name = validate_alias(name)?;
        let removed = self
            .connection
            .execute("DELETE FROM aliases WHERE name = ?1", params![name])?;
        Ok(removed > 0)
    }

    /// Aliases ordered by name.
    pub fn aliases(&self, limit: u32) -> Result<Vec<(String, String)>, HistoryError> {
        if limit == 0 || limit > MAX_HISTORY_RESULTS {
            return Err(HistoryError::InvalidLimit);
        }
        let mut statement = self
            .connection
            .prepare("SELECT name, target FROM aliases ORDER BY name ASC LIMIT ?1")?;
        let rows = statement.query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Create or replace a setting.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<(), HistoryError> {
        let key = validate_setting(key)?;
        let value = validate_setting(value)?;
        self.connection.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Read a setting value.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, HistoryError> {
        let key = validate_setting(key)?;
        self.connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Remove a setting. Returns whether it existed.
    pub fn remove_setting(&mut self, key: &str) -> Result<bool, HistoryError> {
        let key = validate_setting(key)?;
        let removed = self
            .connection
            .execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(removed > 0)
    }

    /// Settings ordered by key.
    pub fn settings(&self, limit: u32) -> Result<Vec<(String, String)>, HistoryError> {
        if limit == 0 || limit > MAX_HISTORY_RESULTS {
            return Err(HistoryError::InvalidLimit);
        }
        let mut statement = self
            .connection
            .prepare("SELECT key, value FROM settings ORDER BY key ASC LIMIT ?1")?;
        let rows = statement.query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn validate_query(query: &str) -> Result<&str, HistoryError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS || query.contains('\0') {
        return Err(HistoryError::InvalidQuery(query.to_owned()));
    }
    Ok(query)
}

fn validate_alias(value: &str) -> Result<&str, HistoryError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_QUERY_CHARS || value.contains('\0') {
        return Err(HistoryError::InvalidAlias);
    }
    Ok(value)
}

fn validate_setting(value: &str) -> Result<&str, HistoryError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_QUERY_CHARS || value.contains('\0') {
        return Err(HistoryError::InvalidSetting);
    }
    Ok(value)
}

fn validate_path(path: &str) -> Result<&str, HistoryError> {
    if path.trim().is_empty() || path.encode_utf16().count() > MAX_PATH_UTF16 || path.contains('\0')
    {
        return Err(HistoryError::InvalidPath);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::{FileId, Frn, VolumeId};
    use tempfile::TempDir;

    fn item(path: &str) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(1)),
            name: path.to_owned(),
            path: path.to_owned(),
            attributes: 0,
            is_dir: false,
        }
    }

    #[test]
    fn records_and_orders_history() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.record("alpha", 10).unwrap();
        store.record("beta", 20).unwrap();
        store.record("alpha", 30).unwrap();
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].query, "alpha");
        assert_eq!(recent[0].uses, 2);
        assert_eq!(recent[0].last_used, 30);
        assert_eq!(recent[1].query, "beta");
    }

    #[test]
    fn limit_bounds_entries() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        for index in 0..5 {
            store.record(&format!("q{index}"), index).unwrap();
        }
        assert_eq!(store.recent(2).unwrap().len(), 2);
    }

    #[test]
    fn invalid_queries_and_limits_are_rejected() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        assert!(matches!(
            store.record("   ", 0),
            Err(HistoryError::InvalidQuery(_))
        ));
        assert!(matches!(
            store.record(&"x".repeat(MAX_QUERY_CHARS + 1), 0),
            Err(HistoryError::InvalidQuery(_))
        ));
        assert!(matches!(
            store.record("bad\0query", 0),
            Err(HistoryError::InvalidQuery(_))
        ));
        assert!(matches!(store.recent(0), Err(HistoryError::InvalidLimit)));
        assert!(matches!(
            store.recent(MAX_HISTORY_RESULTS + 1),
            Err(HistoryError::InvalidLimit)
        ));
    }

    #[test]
    fn fresh_database_records_schema_version() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let store = HistoryStore::open(&path).unwrap();
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn future_schema_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version = 3").unwrap();
        drop(connection);
        assert!(matches!(
            HistoryStore::open(&path),
            Err(HistoryError::Schema(3))
        ));
    }

    #[test]
    fn migrates_v1_to_v2_and_keeps_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection.execute_batch("PRAGMA user_version = 1").unwrap();
        connection
            .execute(
                "INSERT INTO search_history(query, uses, last_used) VALUES ('old', 1, 10)",
                [],
            )
            .unwrap();
        drop(connection);
        let store = HistoryStore::open(&path).unwrap();
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(store.recent(10).unwrap()[0].query, "old");
        let index: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_history_last_used'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn failed_migration_keeps_v1_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection.execute_batch("PRAGMA user_version = 1").unwrap();
        connection
            .execute_batch("CREATE TABLE idx_history_last_used (id INTEGER);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO search_history(query, uses, last_used) VALUES ('keep', 1, 10)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(matches!(
            HistoryStore::open(&path),
            Err(HistoryError::Database(_))
        ));
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM search_history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn backup_is_readable_and_rejects_existing_target() {
        let dir = TempDir::new().unwrap();
        let mut store = HistoryStore::open(&dir.path().join("host.db")).unwrap();
        store.record("backup", 10).unwrap();
        let target = dir.path().join("backup.db");
        store.backup_to(&target).unwrap();
        let backup = HistoryStore::open(&target).unwrap();
        assert_eq!(backup.recent(10).unwrap()[0].query, "backup");
        assert!(matches!(
            store.backup_to(&target),
            Err(HistoryError::Database(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn backup_rejects_non_utf8_path() {
        use std::os::windows::ffi::OsStringExt;
        let dir = TempDir::new().unwrap();
        let store = HistoryStore::open(&dir.path().join("host.db")).unwrap();
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0xD800]));
        assert!(matches!(
            store.backup_to(&path),
            Err(HistoryError::InvalidPath)
        ));
    }

    #[test]
    fn restore_replaces_rows_from_backup() {
        let dir = TempDir::new().unwrap();
        let source_path = dir.path().join("source.db");
        let target_path = dir.path().join("target.db");
        let mut source = HistoryStore::open(&source_path).unwrap();
        source.record("source", 10).unwrap();
        source.set_setting("theme", "dark").unwrap();
        source.set_alias("docs", "*.md").unwrap();
        let mut target = HistoryStore::open(&target_path).unwrap();
        target.record("target", 20).unwrap();
        target.set_setting("theme", "light").unwrap();
        target.restore_from(&source_path).unwrap();
        assert_eq!(target.recent(10).unwrap()[0].query, "source");
        assert_eq!(
            target.get_setting("theme").unwrap().as_deref(),
            Some("dark")
        );
        assert_eq!(
            target.resolve_alias("docs").unwrap().as_deref(),
            Some("*.md")
        );
    }

    #[test]
    fn failed_restore_keeps_current_rows() {
        let dir = TempDir::new().unwrap();
        let mut target = HistoryStore::open(&dir.path().join("target.db")).unwrap();
        target.record("keep", 10).unwrap();
        assert!(matches!(
            target.restore_from(&dir.path().join("missing.db")),
            Err(HistoryError::Database(_))
        ));
        assert_eq!(target.recent(10).unwrap()[0].query, "keep");
    }

    #[test]
    fn future_backup_is_rejected_without_changes() {
        let dir = TempDir::new().unwrap();
        let target_path = dir.path().join("target.db");
        let mut target = HistoryStore::open(&target_path).unwrap();
        target.record("keep", 10).unwrap();
        let backup_path = dir.path().join("future.db");
        let connection = Connection::open(&backup_path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection.execute_batch("PRAGMA user_version = 3").unwrap();
        drop(connection);
        assert!(matches!(
            target.restore_from(&backup_path),
            Err(HistoryError::Schema(3))
        ));
        assert_eq!(target.recent(10).unwrap()[0].query, "keep");
    }

    #[test]
    fn path_is_available_for_file_stores() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.path(), Some(path.as_path()));
        assert!(HistoryStore::open_in_memory().unwrap().path().is_none());
    }

    #[test]
    fn prunes_old_pre_restore_backups() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let store = HistoryStore::open(&path).unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy();
        for index in 0..4 {
            let backup = dir.path().join(format!("{stem}.pre-restore-{index}.db"));
            std::fs::write(&backup, b"x").unwrap();
        }
        assert_eq!(store.prune_pre_restore_backups(2).unwrap(), 2);
        assert!(!dir.path().join(format!("{stem}.pre-restore-0.db")).exists());
        assert!(!dir.path().join(format!("{stem}.pre-restore-1.db")).exists());
        assert!(dir.path().join(format!("{stem}.pre-restore-2.db")).exists());
        assert!(dir.path().join(format!("{stem}.pre-restore-3.db")).exists());
        let listed = store.pre_restore_backups(2).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed[0].file_name().unwrap().to_string_lossy(),
            format!("{stem}.pre-restore-3.db")
        );
        assert_eq!(
            listed[1].file_name().unwrap().to_string_lossy(),
            format!("{stem}.pre-restore-2.db")
        );
        assert_eq!(
            HistoryStore::open_in_memory()
                .unwrap()
                .prune_pre_restore_backups(1)
                .unwrap(),
            0
        );
        assert!(HistoryStore::open_in_memory()
            .unwrap()
            .pre_restore_backups(1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deletes_only_matching_backups() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        let store = HistoryStore::open(&path).unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy();
        let backup = dir.path().join(format!("{stem}.pre-restore-1.db"));
        std::fs::write(&backup, b"x").unwrap();
        store.delete_restore_backup(&backup).unwrap();
        assert!(!backup.exists());
        let unrelated = dir.path().join("other.db");
        std::fs::write(&unrelated, b"x").unwrap();
        assert!(matches!(
            store.delete_restore_backup(&unrelated),
            Err(HistoryError::InvalidPath)
        ));
        assert!(unrelated.exists());
        assert!(matches!(
            HistoryStore::open_in_memory()
                .unwrap()
                .delete_restore_backup(&unrelated),
            Err(HistoryError::InvalidPath)
        ));
    }

    #[test]
    fn file_database_persists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.db");
        {
            let mut store = HistoryStore::open(&path).unwrap();
            store.record("persisted", 7).unwrap();
        }
        let store = HistoryStore::open(&path).unwrap();
        let recent = store.recent(1).unwrap();
        assert_eq!(recent[0].query, "persisted");
    }

    #[test]
    fn removes_history_entries() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.record("alpha", 10).unwrap();
        store.record("beta", 20).unwrap();
        assert!(store.remove("alpha").unwrap());
        assert!(!store.remove("alpha").unwrap());
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].query, "beta");
        assert!(matches!(
            store.remove("  "),
            Err(HistoryError::InvalidQuery(_))
        ));
        store.record("gamma", 30).unwrap();
        assert_eq!(store.clear_history().unwrap(), 2);
        assert!(store.recent(10).unwrap().is_empty());
    }

    #[test]
    fn records_mru_weights() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.record_mru("C:\\one.txt", 10).unwrap();
        store.record_mru("C:\\two.txt", 20).unwrap();
        store.record_mru("C:\\one.txt", 30).unwrap();
        assert_eq!(store.mru_score("C:\\one.txt").unwrap(), 2);
        assert_eq!(store.mru_score("C:\\missing.txt").unwrap(), 0);
        let recent = store.recent_mru(10).unwrap();
        assert_eq!(recent[0].path, "C:\\one.txt");
        assert_eq!(recent[0].uses, 2);
        assert_eq!(recent[1].path, "C:\\two.txt");
    }

    #[test]
    fn scores_and_ranks_items_by_mru() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.record_mru("C:\\a.txt", 10).unwrap();
        store.record_mru("C:\\a.txt", 20).unwrap();
        store.record_mru("C:\\b.txt", 30).unwrap();

        let paths = vec![
            "C:\\a.txt".to_owned(),
            "C:\\b.txt".to_owned(),
            "C:\\missing.txt".to_owned(),
            "C:\\a.txt".to_owned(),
        ];
        let scores = store.mru_scores(&paths).unwrap();
        assert_eq!(scores.get("C:\\a.txt"), Some(&2));
        assert_eq!(scores.get("C:\\b.txt"), Some(&1));
        assert!(!scores.contains_key("C:\\missing.txt"));
        assert!(store.mru_scores(&[]).unwrap().is_empty());

        let mut items = vec![
            item("C:\\b.txt"),
            item("C:\\a.txt"),
            item("C:\\missing.txt"),
        ];
        store.rank_items(&mut items).unwrap();
        assert_eq!(items[0].path, "C:\\a.txt");
        assert_eq!(items[1].path, "C:\\b.txt");
        assert_eq!(items[2].path, "C:\\missing.txt");

        let mut empty = Vec::new();
        store.rank_items(&mut empty).unwrap();
    }

    #[test]
    fn invalid_mru_paths_are_rejected() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        assert!(matches!(
            store.record_mru("  ", 0),
            Err(HistoryError::InvalidPath)
        ));
        assert!(matches!(
            store.record_mru("bad\0path", 0),
            Err(HistoryError::InvalidPath)
        ));
        assert!(matches!(
            store.record_mru(&"x".repeat(MAX_PATH_UTF16 + 1), 0),
            Err(HistoryError::InvalidPath)
        ));
        assert!(matches!(
            store.recent_mru(0),
            Err(HistoryError::InvalidLimit)
        ));
    }

    #[test]
    fn aliases_resolve_replace_and_remove() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.set_alias("docs", "*.md").unwrap();
        assert_eq!(
            store.resolve_alias("docs").unwrap().as_deref(),
            Some("*.md")
        );
        store.set_alias("docs", "*.txt").unwrap();
        assert_eq!(
            store.resolve_alias("docs").unwrap().as_deref(),
            Some("*.txt")
        );
        store.set_alias("web", "https://example.com").unwrap();
        assert_eq!(
            store.aliases(10).unwrap(),
            vec![
                ("docs".to_owned(), "*.txt".to_owned()),
                ("web".to_owned(), "https://example.com".to_owned())
            ]
        );
        assert!(store.remove_alias("docs").unwrap());
        assert!(!store.remove_alias("docs").unwrap());
        assert_eq!(store.resolve_alias("docs").unwrap(), None);
    }

    #[test]
    fn invalid_aliases_are_rejected() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        assert!(matches!(
            store.set_alias("", "x"),
            Err(HistoryError::InvalidAlias)
        ));
        assert!(matches!(
            store.set_alias("x", "  "),
            Err(HistoryError::InvalidAlias)
        ));
        assert!(matches!(
            store.set_alias("bad\0", "x"),
            Err(HistoryError::InvalidAlias)
        ));
        assert!(matches!(
            store.set_alias(&"x".repeat(MAX_QUERY_CHARS + 1), "y"),
            Err(HistoryError::InvalidAlias)
        ));
        assert!(matches!(store.aliases(0), Err(HistoryError::InvalidLimit)));
    }

    #[test]
    fn settings_roundtrip() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        store.set_setting("theme", "dark").unwrap();
        assert_eq!(store.get_setting("theme").unwrap().as_deref(), Some("dark"));
        store.set_setting("theme", "light").unwrap();
        store.set_setting("hotkey", "double ctrl").unwrap();
        assert_eq!(
            store.settings(10).unwrap(),
            vec![
                ("hotkey".to_owned(), "double ctrl".to_owned()),
                ("theme".to_owned(), "light".to_owned())
            ]
        );
        assert!(store.remove_setting("theme").unwrap());
        assert!(!store.remove_setting("theme").unwrap());
        assert_eq!(store.get_setting("theme").unwrap(), None);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        let mut store = HistoryStore::open_in_memory().unwrap();
        assert!(matches!(
            store.set_setting("", "x"),
            Err(HistoryError::InvalidSetting)
        ));
        assert!(matches!(
            store.set_setting("x", "  "),
            Err(HistoryError::InvalidSetting)
        ));
        assert!(matches!(
            store.set_setting("bad\0", "x"),
            Err(HistoryError::InvalidSetting)
        ));
        assert!(matches!(
            store.set_setting(&"x".repeat(MAX_QUERY_CHARS + 1), "y"),
            Err(HistoryError::InvalidSetting)
        ));
        assert!(matches!(store.settings(0), Err(HistoryError::InvalidLimit)));
    }

    #[test]
    fn schema_failures_are_reported() {
        let dir = TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt.db");
        std::fs::write(&corrupt, b"not a sqlite database").unwrap();
        assert!(matches!(
            HistoryStore::open(&corrupt),
            Err(HistoryError::Database(_))
        ));

        let dropped = dir.path().join("dropped.db");
        let mut store = HistoryStore::open(&dropped).unwrap();
        {
            let connection = Connection::open(&dropped).unwrap();
            connection
                .execute_batch(
                    "DROP TABLE search_history; DROP TABLE mru;
                     DROP TABLE settings; DROP TABLE aliases;",
                )
                .unwrap();
        }
        assert!(matches!(
            store.record("query", 1),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(store.recent(1), Err(HistoryError::Database(_))));
        assert!(matches!(
            store.remove("query"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.clear_history(),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.record_mru("C:\\a.txt", 1),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.mru_score("C:\\a.txt"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.recent_mru(1),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.mru_scores(&[String::from("C:\\a.txt")]),
            Err(HistoryError::Database(_))
        ));
        let mut ranked = vec![item("C:\\a.txt")];
        assert!(store.rank_items(&mut ranked).is_err());
        assert!(matches!(
            store.set_setting("k", "v"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.get_setting("k"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.remove_setting("k"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(store.settings(1), Err(HistoryError::Database(_))));
        assert!(matches!(
            store.set_alias("a", "b"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.resolve_alias("a"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(
            store.remove_alias("a"),
            Err(HistoryError::Database(_))
        ));
        assert!(matches!(store.aliases(1), Err(HistoryError::Database(_))));
    }

    #[test]
    fn missing_parent_is_reported() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            HistoryStore::open(&dir.path().join("missing").join("host.db")),
            Err(HistoryError::Database(_))
        ));
    }
}
