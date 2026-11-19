//! SQLite persistence with atomic metadata + checkpoint commits.

use std::path::Path;

use pstart_core::VolumeId;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::IndexError;
use crate::model::FileRecord;
use crate::state::{Checkpoint, SyncState};
use crate::trigram::TrigramBuilder;

/// Current schema version.
pub const SCHEMA_VERSION: i64 = 2;

const MIGRATION_V1: &str = r#"
CREATE TABLE volume_state (
    volume_id TEXT PRIMARY KEY,
    volume_name TEXT NOT NULL,
    filesystem_type TEXT NOT NULL,
    journal_id INTEGER,
    next_usn INTEGER,
    index_generation INTEGER NOT NULL DEFAULT 0,
    sync_state TEXT NOT NULL DEFAULT 'uninitialized',
    updated_at INTEGER NOT NULL
);

CREATE TABLE file_metadata (
    volume_id TEXT NOT NULL,
    frn INTEGER NOT NULL CHECK (frn >= 0),
    parent_frn INTEGER,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    attributes INTEGER NOT NULL,
    is_dir INTEGER NOT NULL CHECK (is_dir IN (0, 1)),
    last_usn INTEGER,
    PRIMARY KEY (volume_id, frn)
);

CREATE INDEX idx_file_parent ON file_metadata(volume_id, parent_frn);
CREATE INDEX idx_file_name ON file_metadata(name);

CREATE TABLE index_checkpoint (
    volume_id TEXT PRIMARY KEY,
    journal_id INTEGER NOT NULL,
    next_usn INTEGER NOT NULL CHECK (next_usn >= 0),
    index_generation INTEGER NOT NULL,
    committed_at INTEGER NOT NULL
);
"#;

const MIGRATION_V2: &str = r#"
DROP INDEX IF EXISTS idx_file_name;
CREATE INDEX idx_file_name_nocase ON file_metadata(volume_id, name COLLATE NOCASE);
"#;

/// Static volume metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeMeta {
    /// Stable volume identifier.
    pub volume: VolumeId,
    /// Human-readable volume name.
    pub volume_name: String,
    /// Filesystem type, for example `NTFS`.
    pub filesystem_type: String,
}

impl VolumeMeta {
    /// Construct and validate metadata.
    pub fn new(
        volume: VolumeId,
        volume_name: impl Into<String>,
        filesystem_type: impl Into<String>,
    ) -> Result<Self, IndexError> {
        let volume_name = volume_name.into();
        let filesystem_type = filesystem_type.into();
        if volume_name.is_empty() {
            return Err(IndexError::InvalidData(
                "volume_name must not be empty".to_owned(),
            ));
        }
        if filesystem_type.is_empty() {
            return Err(IndexError::InvalidData(
                "filesystem_type must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            volume,
            volume_name,
            filesystem_type,
        })
    }
}

/// Delete event persisted in the same transaction as the checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeletedFrn {
    /// File reference number.
    pub frn: u64,
    /// USN of the delete event.
    pub usn: u64,
}

/// Atomic metadata and checkpoint batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistBatch {
    /// Records to create or update.
    pub upserts: Vec<FileRecord>,
    /// Records to remove.
    pub deletes: Vec<DeletedFrn>,
    /// Checkpoint that must become durable with this batch.
    pub checkpoint: Checkpoint,
    /// Optional synchronization state persisted with the same transaction.
    pub sync_state: Option<SyncState>,
}

/// Persisted volume_state row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredVolumeState {
    /// Journal identifier, when bound.
    pub journal_id: Option<u64>,
    /// Persisted next USN.
    pub next_usn: u64,
    /// Index generation.
    pub index_generation: u64,
    /// Raw synchronization state string.
    pub sync_state: String,
}

/// Counts returned by a successful commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitStats {
    /// Upserted records.
    pub upserted: usize,
    /// Deleted records.
    pub deleted: usize,
}

/// Result of comparing the persisted checkpoint with an observed journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointStatus {
    /// No checkpoint has ever been committed.
    Missing,
    /// Journal matches; the checkpoint can be used.
    Match(Checkpoint),
    /// Journal differs; a rebuild is required.
    Mismatch {
        /// Persisted checkpoint.
        checkpoint: Checkpoint,
        /// Journal ID observed from the volume.
        observed_journal_id: u64,
    },
}

/// One SQLite-backed volume index.
pub struct IndexStore {
    connection: Connection,
    meta: VolumeMeta,
}

impl std::fmt::Debug for IndexStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexStore")
            .field("volume", &self.meta.volume)
            .finish_non_exhaustive()
    }
}

impl IndexStore {
    /// Open or create a database and run migrations.
    pub fn open(path: &Path, meta: VolumeMeta) -> Result<Self, IndexError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, meta)
    }

    /// Open an isolated in-memory database (tests and dry runs).
    pub fn open_in_memory(meta: VolumeMeta) -> Result<Self, IndexError> {
        let connection = Connection::open_in_memory()?;
        Self::from_connection(connection, meta)
    }

    fn from_connection(connection: Connection, meta: VolumeMeta) -> Result<Self, IndexError> {
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-20000;
             PRAGMA busy_timeout=5000;",
        )?;
        let mut store = Self { connection, meta };
        store.migrate()?;
        store.ensure_volume_row()?;
        Ok(store)
    }

    /// Current schema version from `PRAGMA user_version`.
    pub fn schema_version(&self) -> Result<i64, IndexError> {
        Ok(self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Volume metadata.
    #[must_use]
    pub fn meta(&self) -> &VolumeMeta {
        &self.meta
    }

    /// Build a small delta shard for the given FRNs.
    pub fn build_trigram_delta(
        &self,
        volume: &VolumeId,
        path: &Path,
        frns: &[u64],
    ) -> Result<(u64, usize), IndexError> {
        let mut statement = self
            .connection
            .prepare("SELECT rowid, name FROM file_metadata WHERE volume_id = ?1 AND frn = ?2")?;
        let mut builder = TrigramBuilder::new();
        let mut documents = 0usize;
        for frn in frns {
            let mut rows = statement.query(params![volume.as_str(), *frn as i64])?;
            if let Some(row) = rows.next()? {
                let rowid: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                builder.add(rowid as u32, name.to_lowercase().as_bytes());
                documents += 1;
            }
        }
        let bytes = builder.write(path)?;
        Ok((bytes, documents))
    }

    /// Build an immutable trigram candidate index from the persisted names.
    ///
    /// Document ids are SQLite `rowid` values, so callers can map candidates
    /// back with `WHERE rowid = ?`.
    pub fn build_trigram_index(
        &self,
        volume: &VolumeId,
        path: &Path,
    ) -> Result<(u64, usize), IndexError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid, name FROM file_metadata
                 WHERE volume_id = ?1 AND rowid <= 4294967295
                 ORDER BY rowid",
            )
            .expect("file metadata schema is validated at open");
        let rows = statement.query_map(params![volume.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut builder = TrigramBuilder::new();
        let mut documents = 0usize;
        for row in rows {
            let (rowid, name) = row?;
            builder.add(rowid as u32, name.to_lowercase().as_bytes());
            documents += 1;
        }
        let bytes = builder.write(path)?;
        Ok((bytes, documents))
    }

    fn migrate(&mut self) -> Result<(), IndexError> {
        let version = self.schema_version()?;
        if version > SCHEMA_VERSION {
            return Err(IndexError::InvalidData(format!(
                "database schema version {version} is newer than supported {SCHEMA_VERSION}"
            )));
        }
        if version < 1 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_V1)?;
            transaction.pragma_update(None, "user_version", 1)?;
            transaction.commit()?;
        }
        if version < 2 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(MIGRATION_V2)?;
            transaction.pragma_update(None, "user_version", 2)?;
            transaction.commit()?;
        }
        Ok(())
    }

    fn ensure_volume_row(&mut self) -> Result<(), IndexError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO volume_state
             (volume_id, volume_name, filesystem_type, journal_id, next_usn,
              index_generation, sync_state, updated_at)
             VALUES (?1, ?2, ?3, NULL, 0, 0, 'uninitialized', 0)",
            params![
                self.meta.volume.as_str(),
                self.meta.volume_name,
                self.meta.filesystem_type
            ],
        )?;
        Ok(())
    }

    /// Commit metadata changes and the checkpoint in one SQLite transaction.
    pub fn commit_batch(&mut self, batch: &PersistBatch) -> Result<CommitStats, IndexError> {
        self.validate_batch(batch)?;
        let cp = &batch.checkpoint;
        let journal_id = sql_i64("journal_id", cp.journal_id)?;
        let next_usn = sql_i64("next_usn", cp.next_usn)?;
        let generation = sql_i64("index_generation", cp.index_generation)?;
        let committed_at = sql_i64("committed_at", cp.committed_at)?;
        let volume_id = self.meta.volume.as_str();

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current: (Option<i64>, i64, i64) = transaction.query_row(
            "SELECT journal_id, next_usn, index_generation
             FROM volume_state WHERE volume_id = ?1",
            params![volume_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        validate_against_current(current, cp, journal_id, next_usn, generation)?;

        {
            let mut statement = transaction.prepare(
                "INSERT INTO file_metadata
                 (volume_id, frn, parent_frn, name, attributes, is_dir, last_usn)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(volume_id, frn) DO UPDATE SET
                    parent_frn = excluded.parent_frn,
                    name = excluded.name,
                    attributes = excluded.attributes,
                    is_dir = excluded.is_dir,
                    last_usn = excluded.last_usn",
            )?;
            for record in &batch.upserts {
                statement.execute(params![
                    volume_id,
                    sql_i64("frn", record.frn)?,
                    sql_i64("parent_frn", record.parent_frn)?,
                    record.name,
                    i64::from(record.attributes),
                    i64::from(u8::from(record.is_dir)),
                    sql_i64("last_usn", record.last_usn)?,
                ])?;
            }
        }

        {
            let mut statement = transaction.prepare(
                "DELETE FROM file_metadata
                 WHERE volume_id = ?1 AND frn = ?2
                   AND (last_usn IS NULL OR last_usn <= ?3)",
            )?;
            for deleted in &batch.deletes {
                statement.execute(params![
                    volume_id,
                    sql_i64("frn", deleted.frn)?,
                    sql_i64("usn", deleted.usn)?,
                ])?;
            }
        }

        transaction.execute(
            "UPDATE volume_state
             SET journal_id = ?1, next_usn = ?2, index_generation = ?3, updated_at = ?4,
                 sync_state = COALESCE(?5, sync_state)
             WHERE volume_id = ?6",
            params![
                journal_id,
                next_usn,
                generation,
                committed_at,
                batch.sync_state.map(SyncState::as_str),
                volume_id
            ],
        )?;

        transaction.execute(
            "INSERT INTO index_checkpoint
             (volume_id, journal_id, next_usn, index_generation, committed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(volume_id) DO UPDATE SET
                journal_id = excluded.journal_id,
                next_usn = excluded.next_usn,
                index_generation = excluded.index_generation,
                committed_at = excluded.committed_at",
            params![volume_id, journal_id, next_usn, generation, committed_at],
        )?;

        transaction.commit()?;
        Ok(CommitStats {
            upserted: batch.upserts.len(),
            deleted: batch.deletes.len(),
        })
    }

    fn validate_batch(&self, batch: &PersistBatch) -> Result<(), IndexError> {
        let cp = &batch.checkpoint;
        for record in &batch.upserts {
            record.validate()?;
            if record.frn == 0 {
                return Err(IndexError::InvalidData("FRN 0 is not valid".to_owned()));
            }
            if record.name.contains('\\') {
                return Err(IndexError::InvalidData(
                    "file name contains a path separator".to_owned(),
                ));
            }
            if record.name.contains('/') {
                return Err(IndexError::InvalidData(
                    "file name contains a path separator".to_owned(),
                ));
            }
            if record.last_usn > cp.next_usn {
                return Err(IndexError::CheckpointAhead {
                    next_usn: cp.next_usn,
                    observed_usn: record.last_usn,
                });
            }
        }
        for deleted in &batch.deletes {
            if deleted.usn > cp.next_usn {
                return Err(IndexError::CheckpointAhead {
                    next_usn: cp.next_usn,
                    observed_usn: deleted.usn,
                });
            }
        }
        Ok(())
    }

    /// Load the persisted `volume_state` row.
    pub fn load_volume_state(&self) -> Result<Option<StoredVolumeState>, IndexError> {
        let row = self
            .connection
            .query_row(
                "SELECT journal_id, next_usn, index_generation, sync_state
                 FROM volume_state WHERE volume_id = ?1",
                params![self.meta.volume.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(journal_id, next_usn, generation, sync_state)| {
            Ok(StoredVolumeState {
                journal_id: journal_id
                    .map(|value| u64_of("journal_id", value))
                    .transpose()?,
                next_usn: next_usn
                    .map(|value| u64_of("next_usn", value))
                    .transpose()?
                    .unwrap_or(0),
                index_generation: u64_of("index_generation", generation)?,
                sync_state,
            })
        })
        .transpose()
    }

    /// Clear all metadata and checkpoint state before a full rescan.
    ///
    /// The new state is committed atomically as `initial_scan`, so a crash in
    /// the middle of the scan cannot later be mistaken for catch-up data.
    pub fn reset_for_scan(
        &mut self,
        journal_id: u64,
        index_generation: u64,
        committed_at: u64,
    ) -> Result<(), IndexError> {
        let journal_id = sql_i64("journal_id", journal_id)?;
        let generation = sql_i64("index_generation", index_generation)?;
        let committed_at = sql_i64("committed_at", committed_at)?;
        let volume_id = self.meta.volume.as_str();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM file_metadata WHERE volume_id = ?1",
            params![volume_id],
        )?;
        transaction.execute(
            "DELETE FROM index_checkpoint WHERE volume_id = ?1",
            params![volume_id],
        )?;
        transaction.execute(
            "UPDATE volume_state
             SET journal_id = ?1, next_usn = 0, index_generation = ?2,
                 sync_state = ?3, updated_at = ?4
             WHERE volume_id = ?5",
            params![
                journal_id,
                generation,
                SyncState::InitialScan.as_str(),
                committed_at,
                volume_id
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist only the synchronization state.
    pub fn set_sync_state(&mut self, state: SyncState, updated_at: u64) -> Result<(), IndexError> {
        self.connection.execute(
            "UPDATE volume_state SET sync_state = ?1, updated_at = ?2
             WHERE volume_id = ?3",
            params![
                state.as_str(),
                sql_i64("updated_at", updated_at)?,
                self.meta.volume.as_str()
            ],
        )?;
        Ok(())
    }

    /// Load the persisted checkpoint.
    pub fn load_checkpoint(&self) -> Result<Option<Checkpoint>, IndexError> {
        let row = self
            .connection
            .query_row(
                "SELECT journal_id, next_usn, index_generation, committed_at
                 FROM index_checkpoint WHERE volume_id = ?1",
                params![self.meta.volume.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(journal_id, next_usn, generation, committed_at)| {
            Ok(Checkpoint {
                journal_id: u64_of("journal_id", journal_id)?,
                next_usn: u64_of("next_usn", next_usn)?,
                index_generation: u64_of("index_generation", generation)?,
                committed_at: u64_of("committed_at", committed_at)?,
            })
        })
        .transpose()
    }

    /// Compare the persisted checkpoint with an observed journal ID.
    pub fn checkpoint_status(
        &self,
        observed_journal_id: u64,
    ) -> Result<CheckpointStatus, IndexError> {
        Ok(classify_checkpoint(
            self.load_checkpoint()?,
            observed_journal_id,
        ))
    }

    /// Load all records for the volume.
    pub fn load_records(&self) -> Result<Vec<FileRecord>, IndexError> {
        let mut statement = self.connection.prepare(
            "SELECT frn, parent_frn, name, attributes, is_dir, last_usn
             FROM file_metadata WHERE volume_id = ?1",
        )?;
        let mut rows = statement.query(params![self.meta.volume.as_str()])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let frn: i64 = row.get(0)?;
            let parent_frn: Option<i64> = row.get(1)?;
            let name: String = row.get(2)?;
            let attributes: i64 = row.get(3)?;
            let is_dir: i64 = row.get(4)?;
            let last_usn: Option<i64> = row.get(5)?;
            records.push(FileRecord {
                frn: u64_of("frn", frn)?,
                parent_frn: parent_frn
                    .map(|value| u64_of("parent_frn", value))
                    .transpose()?
                    .unwrap_or(0),
                name: name.into(),
                attributes: u32::try_from(attributes).map_err(|_| {
                    IndexError::InvalidData(format!("attributes {attributes} out of range"))
                })?,
                is_dir: is_dir != 0,
                last_usn: last_usn
                    .map(|value| u64_of("last_usn", value))
                    .transpose()?
                    .unwrap_or(0),
            });
        }
        Ok(records)
    }

    /// Number of persisted records.
    pub fn file_count(&self) -> Result<u64, IndexError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM file_metadata WHERE volume_id = ?1",
            params![self.meta.volume.as_str()],
            |row| row.get(0),
        )?;
        u64_of("file_count", count)
    }
}

/// Count persisted files without opening the writable store.
pub fn count_records(database: &Path, volume_id: &str) -> Result<u64, IndexError> {
    let connection = Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_millis(1000))?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM file_metadata WHERE volume_id = ?1",
        params![volume_id],
        |row| row.get(0),
    )?;
    u64_of("file_count", count)
}

#[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
fn classify_checkpoint(checkpoint: Option<Checkpoint>, observed: u64) -> CheckpointStatus {
    match checkpoint {
        None => CheckpointStatus::Missing,
        Some(checkpoint) if checkpoint.journal_id == observed => {
            CheckpointStatus::Match(checkpoint)
        }
        Some(checkpoint) => CheckpointStatus::Mismatch {
            checkpoint,
            observed_journal_id: observed,
        },
    }
}

fn validate_against_current(
    current: (Option<i64>, i64, i64),
    checkpoint: &Checkpoint,
    journal_id: i64,
    next_usn: i64,
    generation: i64,
) -> Result<(), IndexError> {
    let (current_journal, current_usn, current_generation) = current;
    match classify_against_current(
        current_journal,
        current_usn,
        current_generation,
        journal_id,
        next_usn,
        generation,
    ) {
        CheckpointDecision::Accept => Ok(()),
        CheckpointDecision::GenerationBehind => Err(IndexError::InvalidData(format!(
            "checkpoint generation {generation} is behind persisted generation {current_generation}"
        ))),
        CheckpointDecision::Regression => Err(IndexError::CheckpointRegression {
            next_usn: checkpoint.next_usn,
            committed_usn: u64_of("next_usn", current_usn)?,
        }),
        CheckpointDecision::JournalMismatch => {
            let current_journal =
                current_journal.expect("journal mismatch requires a current journal");
            Err(IndexError::JournalMismatch {
                expected: u64_of("journal_id", current_journal)?,
                actual: checkpoint.journal_id,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointDecision {
    Accept,
    GenerationBehind,
    Regression,
    JournalMismatch,
}

#[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
fn classify_against_current(
    current_journal: Option<i64>,
    current_usn: i64,
    current_generation: i64,
    journal_id: i64,
    next_usn: i64,
    generation: i64,
) -> CheckpointDecision {
    if generation < current_generation {
        return CheckpointDecision::GenerationBehind;
    }
    if generation == current_generation {
        if next_usn < current_usn {
            return CheckpointDecision::Regression;
        }
    }
    if let Some(current) = current_journal {
        if current != journal_id {
            if generation <= current_generation {
                return CheckpointDecision::JournalMismatch;
            }
        }
    }
    CheckpointDecision::Accept
}

fn sql_i64(field: &str, value: u64) -> Result<i64, IndexError> {
    i64::try_from(value).map_err(|_| {
        IndexError::InvalidData(format!(
            "{field} value {value} does not fit in SQLite INTEGER"
        ))
    })
}

fn u64_of(field: &str, value: i64) -> Result<u64, IndexError> {
    u64::try_from(value)
        .map_err(|_| IndexError::InvalidData(format!("{field} value {value} is negative")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Checkpoint;
    use tempfile::TempDir;

    fn meta() -> VolumeMeta {
        VolumeMeta::new(VolumeId::new("C:").unwrap(), "System", "NTFS").unwrap()
    }

    fn record(frn: u64, parent: u64, name: &str, usn: u64) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.into(),
            attributes: 0,
            is_dir: false,
            last_usn: usn,
        }
    }

    fn checkpoint(next_usn: u64, generation: u64, journal_id: u64) -> Checkpoint {
        Checkpoint {
            journal_id,
            next_usn,
            index_generation: generation,
            committed_at: 1_700_000_000,
        }
    }

    fn batch(upserts: Vec<FileRecord>, deletes: Vec<DeletedFrn>, next_usn: u64) -> PersistBatch {
        PersistBatch {
            upserts,
            deletes,
            checkpoint: checkpoint(next_usn, 0, 7),
            sync_state: None,
        }
    }

    #[test]
    fn set_sync_state_error_is_propagated() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute("DROP TABLE volume_state", [])
            .unwrap();
        assert!(store
            .set_sync_state(SyncState::LiveSync, 1_700_000_000)
            .is_err());
    }

    #[test]
    fn migrations_are_idempotent() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        drop(store);
        let store = IndexStore::open_in_memory(meta()).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn locked_database_rejects_commit_without_checkpoint_advance() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("locked.db");
        let mut store = IndexStore::open(&path, meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 2)], vec![], 5))
            .unwrap();
        let locker = Connection::open(&path).unwrap();
        locker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let result = store.commit_batch(&batch(vec![record(11, 5, "b.txt", 6)], vec![], 6));
        assert!(result.is_err());
        drop(locker);
        assert_eq!(store.load_checkpoint().unwrap().unwrap().next_usn, 5);
        assert_eq!(store.file_count().unwrap(), 1);
    }

    #[test]
    fn checkpoint_trigger_failure_rolls_back_batch() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 2)], vec![], 5))
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_checkpoint_insert
                 BEFORE INSERT ON index_checkpoint
                 BEGIN SELECT RAISE(ABORT, 'fail'); END;
                 CREATE TRIGGER fail_checkpoint_update
                 BEFORE UPDATE ON index_checkpoint
                 BEGIN SELECT RAISE(ABORT, 'fail'); END;",
            )
            .unwrap();
        let result = store.commit_batch(&batch(vec![record(11, 5, "b.txt", 6)], vec![], 6));
        assert!(result.is_err());
        let checkpoint = store.load_checkpoint().unwrap().unwrap();
        assert_eq!(checkpoint.next_usn, 5);
        assert_eq!(store.file_count().unwrap(), 1);
        assert!(!store
            .load_records()
            .unwrap()
            .iter()
            .any(|value| value.frn == 11));
    }

    #[test]
    fn corrupt_database_is_reported_without_panic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.db");
        std::fs::write(&path, b"this is not a sqlite database").unwrap();
        assert!(IndexStore::open(&path, meta()).is_err());
    }

    #[test]
    fn commit_and_load_roundtrip() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let stats = store
            .commit_batch(&batch(
                vec![record(5, 5, ".", 1), record(10, 5, "Users", 2)],
                vec![],
                10,
            ))
            .unwrap();
        assert_eq!(stats.upserted, 2);
        assert_eq!(store.file_count().unwrap(), 2);
        let checkpoint = store.load_checkpoint().unwrap().unwrap();
        assert_eq!(checkpoint.next_usn, 10);
        assert_eq!(checkpoint.journal_id, 7);
        let records = store.load_records().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|value| value.name.as_ref() == "Users"));
    }

    #[test]
    fn persistent_database_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut store = IndexStore::open(&path, meta()).unwrap();
            store
                .commit_batch(&batch(vec![record(5, 5, ".", 1)], vec![], 5))
                .unwrap();
        }
        let store = IndexStore::open(&path, meta()).unwrap();
        assert_eq!(store.file_count().unwrap(), 1);
        assert_eq!(store.load_checkpoint().unwrap().unwrap().next_usn, 5);
    }

    #[test]
    fn deletes_are_applied_with_checkpoint() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(
                vec![record(5, 5, ".", 1), record(10, 5, "old.txt", 2)],
                vec![],
                5,
            ))
            .unwrap();
        let stats = store
            .commit_batch(&batch(vec![], vec![DeletedFrn { frn: 10, usn: 6 }], 6))
            .unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(store.file_count().unwrap(), 1);
        assert_eq!(store.load_checkpoint().unwrap().unwrap().next_usn, 6);
    }

    #[test]
    fn newer_record_update_replaces_existing_row() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "old.txt", 2)], vec![], 5))
            .unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "new.txt", 6)], vec![], 6))
            .unwrap();
        let records = store.load_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name.as_ref(), "new.txt");
    }

    #[test]
    fn checkpoint_cannot_be_ahead_of_records() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let result = store.commit_batch(&batch(vec![record(10, 5, "a.txt", 10)], vec![], 9));
        assert!(matches!(result, Err(IndexError::CheckpointAhead { .. })));
        assert_eq!(store.file_count().unwrap(), 0);
        assert!(store.load_checkpoint().unwrap().is_none());
    }

    #[test]
    fn checkpoint_regression_is_rejected_without_writes() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 2)], vec![], 10))
            .unwrap();
        let result = store.commit_batch(&batch(vec![record(11, 5, "b.txt", 9)], vec![], 9));
        assert!(matches!(
            result,
            Err(IndexError::CheckpointRegression {
                next_usn: 9,
                committed_usn: 10
            })
        ));
        assert_eq!(store.file_count().unwrap(), 1);
        assert_eq!(store.load_checkpoint().unwrap().unwrap().next_usn, 10);
    }

    #[test]
    fn journal_mismatch_is_detected() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(5, 5, ".", 1)], vec![], 5))
            .unwrap();
        assert_eq!(
            store.checkpoint_status(7).unwrap(),
            CheckpointStatus::Match(checkpoint(5, 0, 7))
        );
        assert!(matches!(
            store.checkpoint_status(8).unwrap(),
            CheckpointStatus::Mismatch { .. }
        ));
        assert_eq!(
            IndexStore::open_in_memory(meta())
                .unwrap()
                .checkpoint_status(7)
                .unwrap(),
            CheckpointStatus::Missing
        );
    }

    #[test]
    fn journal_change_requires_new_generation() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(5, 5, ".", 1)], vec![], 5))
            .unwrap();
        let mut next = batch(vec![record(5, 5, ".", 2)], vec![], 6);
        next.checkpoint.journal_id = 8;
        assert!(matches!(
            store.commit_batch(&next),
            Err(IndexError::JournalMismatch {
                expected: 7,
                actual: 8
            })
        ));
        next.checkpoint.index_generation = 1;
        store.commit_batch(&next).unwrap();
        assert_eq!(store.load_checkpoint().unwrap().unwrap().journal_id, 8);
    }

    #[test]
    fn generation_regression_is_rejected() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(5, 5, ".", 1)], vec![], 5))
            .unwrap();
        store.commit_batch(&batch(vec![], vec![], 6)).unwrap();

        let mut rebuilt = batch(vec![], vec![], 7);
        rebuilt.checkpoint.index_generation = 1;
        rebuilt.checkpoint.journal_id = 9;
        store.commit_batch(&rebuilt).unwrap();

        let stale_generation = batch(vec![], vec![], 8);
        assert!(matches!(
            store.commit_batch(&stale_generation),
            Err(IndexError::InvalidData(_))
        ));
    }

    #[test]
    fn invalid_names_are_rejected_before_writing() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let mut record = record(10, 5, "valid.txt", 2);
        record.name = "x".repeat(300).into();
        let result = store.commit_batch(&batch(vec![record], vec![], 5));
        assert!(result.is_err());
        assert_eq!(store.file_count().unwrap(), 0);
    }

    #[test]
    fn volume_metadata_validation() {
        let volume = VolumeId::new("C:").unwrap();
        assert!(VolumeMeta::new(volume.clone(), "", "NTFS").is_err());
        assert!(VolumeMeta::new(volume.clone(), "System", "").is_err());
        assert!(VolumeMeta::new(volume, "System", "NTFS").is_ok());
    }

    #[test]
    fn store_exposes_metadata_and_debug() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        assert_eq!(store.meta().volume.as_str(), "C:");
        let debug = format!("{store:?}");
        assert!(debug.contains("IndexStore"));
        assert!(debug.contains("C:"));
    }

    #[test]
    fn newer_schema_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("newer.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(IndexStore::open(&path, meta()).is_err());
    }

    #[test]
    fn invalid_upsert_fields_are_rejected() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        assert!(store
            .commit_batch(&batch(vec![record(0, 5, "zero.txt", 1)], vec![], 2))
            .is_err());
        assert!(store
            .commit_batch(&batch(vec![record(10, 5, "bad\\name", 1)], vec![], 2))
            .is_err());
        assert!(store
            .commit_batch(&batch(vec![], vec![DeletedFrn { frn: 10, usn: 3 }], 2))
            .is_err());
    }

    #[test]
    fn overflowing_frn_is_rejected_by_sql_conversion() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let overflow = FileRecord {
            frn: u64::MAX,
            parent_frn: 5,
            name: "huge.txt".into(),
            attributes: 0,
            is_dir: false,
            last_usn: 1,
        };
        assert!(store
            .commit_batch(&batch(vec![overflow], vec![], 2))
            .is_err());
    }

    #[test]
    fn null_optional_columns_load_as_zero() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO file_metadata
                 (volume_id, frn, parent_frn, name, attributes, is_dir, last_usn)
                 VALUES (?1, 10, NULL, 'rootless.txt', 0, 0, NULL)",
                params![store.meta.volume.as_str()],
            )
            .unwrap();
        let records = store.load_records().unwrap();
        assert_eq!(records[0].parent_frn, 0);
        assert_eq!(records[0].last_usn, 0);
    }

    #[test]
    fn out_of_range_attributes_are_rejected() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO file_metadata
                 (volume_id, frn, parent_frn, name, attributes, is_dir, last_usn)
                 VALUES (?1, 11, 5, 'bad-attr.txt', 4294967296, 0, 1)",
                params![store.meta.volume.as_str()],
            )
            .unwrap();
        assert!(store.load_records().is_err());
    }

    #[test]
    fn negative_integer_conversion_is_rejected() {
        assert!(u64_of("value", -1).is_err());
    }

    #[test]
    fn empty_batch_advances_no_state() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let stats = store.commit_batch(&batch(vec![], vec![], 1)).unwrap();
        assert_eq!(stats.upserted, 0);
        assert_eq!(stats.deleted, 0);
        assert_eq!(store.load_checkpoint().unwrap().unwrap().next_usn, 1);
    }

    #[test]
    fn read_only_connection_fails_pragma_setup() {
        use rusqlite::OpenFlags;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("readonly.db");
        Connection::open(&path).unwrap();
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(IndexStore::from_connection(connection, meta()).is_err());
    }

    #[test]
    fn migration_v2_installs_nocase_prefix_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v1.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATION_V1).unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
        }
        let store = IndexStore::open(&path, meta()).unwrap();
        assert_eq!(store.schema_version().unwrap(), 2);
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='idx_file_name_nocase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn trigram_index_builds_from_persisted_names() {
        let dir = TempDir::new().unwrap();
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(
                vec![
                    record(10, 5, "Alpha.txt", 1),
                    record(11, 5, "alpine.txt", 2),
                    record(12, 5, "other.txt", 3),
                ],
                vec![],
                3,
            ))
            .unwrap();
        let path = dir.path().join("trigram.bin");
        let (bytes, documents) = store.build_trigram_index(&meta().volume, &path).unwrap();
        assert!(bytes > 0);
        assert_eq!(documents, 3);
        let index = crate::trigram::TrigramIndex::open(&path).unwrap();
        assert_eq!(index.candidates("alp", 10).len(), 2);
        assert!(index.candidates("zzz", 10).is_empty());
    }

    #[test]
    fn missing_volume_table_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing-volume.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATION_V1).unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            connection.execute_batch("DROP TABLE volume_state").unwrap();
        }
        assert!(IndexStore::open(&path, meta()).is_err());
    }

    #[test]
    fn missing_volume_row_fails_commit() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute_batch("DROP TABLE volume_state")
            .unwrap();
        assert!(store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 1)], vec![], 2))
            .is_err());
    }

    #[test]
    fn injected_insert_failure_rolls_back() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_insert BEFORE INSERT ON file_metadata
                 BEGIN SELECT RAISE(ABORT, 'injected insert'); END;",
            )
            .unwrap();
        assert!(store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 1)], vec![], 2))
            .is_err());
        assert_eq!(store.file_count().unwrap(), 0);
    }

    #[test]
    fn injected_delete_failure_rolls_back() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 1)], vec![], 2))
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_delete BEFORE DELETE ON file_metadata
                 BEGIN SELECT RAISE(ABORT, 'injected delete'); END;",
            )
            .unwrap();
        assert!(store
            .commit_batch(&batch(vec![], vec![DeletedFrn { frn: 10, usn: 3 }], 3))
            .is_err());
        assert_eq!(store.file_count().unwrap(), 1);
    }

    #[test]
    fn injected_checkpoint_failure_rolls_back_metadata() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_checkpoint BEFORE INSERT ON index_checkpoint
                 BEGIN SELECT RAISE(ABORT, 'injected checkpoint'); END;",
            )
            .unwrap();
        assert!(store
            .commit_batch(&batch(vec![record(10, 5, "a.txt", 1)], vec![], 2))
            .is_err());
        assert_eq!(store.file_count().unwrap(), 0);
        assert!(store.load_checkpoint().unwrap().is_none());
    }

    #[test]
    fn load_errors_are_reported_after_schema_loss() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute_batch("DROP TABLE file_metadata")
            .unwrap();
        assert!(store.load_records().is_err());
        assert!(store.file_count().is_err());
    }

    #[test]
    fn volume_state_roundtrips_and_reset_clears_data() {
        let mut store = IndexStore::open_in_memory(meta()).unwrap();
        let initial = store.load_volume_state().unwrap().unwrap();
        assert_eq!(initial.journal_id, None);
        assert_eq!(initial.next_usn, 0);
        assert_eq!(initial.sync_state, "uninitialized");

        let mut first = batch(vec![record(10, 5, "a.txt", 1)], vec![], 5);
        first.sync_state = Some(SyncState::CatchUp);
        store.commit_batch(&first).unwrap();
        let state = store.load_volume_state().unwrap().unwrap();
        assert_eq!(state.journal_id, Some(7));
        assert_eq!(state.next_usn, 5);
        assert_eq!(state.sync_state, "catch_up");

        store.reset_for_scan(9, 2, 1_700_000_100).unwrap();
        assert_eq!(store.file_count().unwrap(), 0);
        assert!(store.load_checkpoint().unwrap().is_none());
        let state = store.load_volume_state().unwrap().unwrap();
        assert_eq!(state.journal_id, Some(9));
        assert_eq!(state.next_usn, 0);
        assert_eq!(state.index_generation, 2);
        assert_eq!(state.sync_state, "initial_scan");

        store
            .set_sync_state(SyncState::LiveSync, 1_700_000_200)
            .unwrap();
        assert_eq!(
            store.load_volume_state().unwrap().unwrap().sync_state,
            "live_sync"
        );
    }

    #[test]
    fn load_volume_state_returns_none_without_row() {
        let store = IndexStore::open_in_memory(meta()).unwrap();
        store
            .connection
            .execute("DELETE FROM volume_state", [])
            .unwrap();
        assert!(store.load_volume_state().unwrap().is_none());
    }
}
