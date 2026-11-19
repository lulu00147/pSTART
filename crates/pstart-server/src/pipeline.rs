//! Volume scan and journal catch-up orchestration.
//!
//! This layer owns the ordering contract between the in-memory index, SQLite
//! metadata and the persisted USN checkpoint. It is platform-independent; the
//! Windows adapter wraps `pstart_ntfs::VolumeHandle`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pstart_core::{ErrorCode, Frn, VolumeId};
#[cfg(test)]
use pstart_index::MemoryIndex;
use pstart_index::{
    delta_directory, Checkpoint, DeletedFrn, FileRecord, IndexError, IndexStore, PersistBatch,
    SearchOutcome, SqliteSearch, SyncState, TransitionError, VolumeMeta, VolumeSync,
};
use pstart_ntfs::{JournalBatch, JournalInfo, MftBatch, NtfsError, UsnRecord};
use thiserror::Error;

/// Maximum file records applied in one SQLite transaction.
pub const DEFAULT_BATCH_LIMIT: usize = 65536;

/// Faster worker cadence while the initial MFT scan is in progress.
pub(crate) const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) fn worker_poll_delay(configured: Duration, scanning: bool) -> Duration {
    if scanning {
        configured.min(SCAN_POLL_INTERVAL)
    } else {
        configured
    }
}

/// Pipeline failure.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Index/store failure.
    #[error(transparent)]
    Index(#[from] IndexError),

    /// NTFS record conversion failure.
    #[error(transparent)]
    Ntfs(#[from] NtfsError),

    /// Volume state machine transition failure.
    #[error(transparent)]
    Transition(#[from] TransitionError),

    /// The source implementation failed.
    #[error("volume source error: {0}")]
    Source(String),

    /// Journal identity changed since the checkpoint was written.
    #[error("journal changed: expected {expected}, observed {observed}")]
    JournalChanged {
        /// Bound journal ID.
        expected: u64,
        /// Newly observed journal ID.
        observed: u64,
    },

    /// The requested USN is no longer in the journal.
    #[error("journal range lost: start {start_usn} is below lowest {lowest_valid_usn}")]
    JournalLost {
        /// Requested start.
        start_usn: u64,
        /// Lowest still-valid USN.
        lowest_valid_usn: i64,
    },

    /// The journal moved behind the persisted checkpoint.
    #[error("journal rewound: start {start_usn} is ahead of next {next_usn}")]
    JournalRewound {
        /// Requested start.
        start_usn: u64,
        /// Observed journal next USN.
        next_usn: u64,
    },

    /// A source page did not advance.
    #[error("source page did not advance from FRN {start_frn}")]
    NoProgress {
        /// FRN that failed to advance.
        start_frn: u64,
    },

    /// The pipeline was used in an invalid state.
    #[error("invalid pipeline state: {0}")]
    InvalidState(String),
}

impl PipelineError {
    /// Stable external error category.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Ntfs(error) => error.code(),
            Self::JournalChanged { .. }
            | Self::JournalLost { .. }
            | Self::JournalRewound { .. } => ErrorCode::JournalReset,
            Self::Source(_) | Self::NoProgress { .. } | Self::InvalidState(_) => {
                ErrorCode::Internal
            }
            Self::Index(_) | Self::Transition(_) => ErrorCode::Internal,
        }
    }
}

/// A source of NTFS journal/MFT data.
pub trait VolumeSource {
    /// Current journal metadata.
    fn journal(&self) -> Result<JournalInfo, PipelineError>;

    /// Enumerate one MFT page bounded by the journal snapshot.
    fn mft_page(&self, start_frn: u64, journal: &JournalInfo) -> Result<MftBatch, PipelineError>;

    /// Read journal records starting at `start_usn`.
    fn read_journal(
        &self,
        start_usn: i64,
        journal: &JournalInfo,
        timeout_ms: u32,
    ) -> Result<JournalBatch, PipelineError>;
}

/// Action recommended after opening a pipeline and querying the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartupAction {
    /// Clear persisted data and perform a full initial scan.
    #[default]
    Scan,
    /// Resume journal catch-up from the persisted checkpoint.
    CatchUp,
}

/// Summary of an initial scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// MFT pages read.
    pub pages: usize,
    /// Records processed.
    pub records: usize,
    /// SQLite batches committed.
    pub batches: usize,
    /// Journal position captured for the scan.
    pub scan_usn: u64,
}

/// Summary of a catch-up run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatchUpReport {
    /// Journal batches read.
    pub batches: usize,
    /// Records processed.
    pub records: usize,
    /// Whether the journal tail was reached.
    pub caught_up: bool,
}

/// Stateful volume index pipeline.
pub struct IndexPipeline {
    volume: VolumeId,
    #[allow(dead_code)]
    root: String,
    #[cfg(test)]
    memory: MemoryIndex,
    sync: VolumeSync,
    store: IndexStore,
    batch_limit: usize,
    scan_session: Option<ScanSession>,
    sqlite_search: Option<SqliteSearch>,
    sqlite_path: Option<PathBuf>,
    sqlite_usn: u64,
    database_path: PathBuf,
}

impl IndexPipeline {
    /// Open persisted state and load the in-memory index.
    pub fn open(
        meta: VolumeMeta,
        root: impl Into<String>,
        database_path: &Path,
        batch_limit: usize,
    ) -> Result<Self, PipelineError> {
        let root = root.into();
        let store = IndexStore::open(database_path, meta.clone())?;
        let persisted = store.load_volume_state()?;
        let checkpoint = store.load_checkpoint()?;
        let state = persisted
            .as_ref()
            .and_then(|value| SyncState::parse(&value.sync_state))
            .unwrap_or(SyncState::Uninitialized);
        let generation = persisted
            .as_ref()
            .map_or(0, |value| value.index_generation)
            .max(
                checkpoint
                    .as_ref()
                    .map_or(0, |value| value.index_generation),
            );
        let committed_usn = checkpoint.as_ref().map_or(0, |value| value.next_usn);
        let journal_id = persisted.as_ref().and_then(|value| value.journal_id);
        let sync = VolumeSync::restore(state, journal_id, generation, committed_usn);

        #[cfg(test)]
        let mut memory = MemoryIndex::new(meta.volume.clone(), root.clone(), Frn::ROOT, 256)?;
        #[cfg(test)]
        if state != SyncState::InitialScan {
            for record in store.load_records()? {
                memory.upsert(record)?;
            }
        }
        Ok(Self {
            volume: meta.volume,
            root,
            #[cfg(test)]
            memory,
            sync,
            store,
            batch_limit: batch_limit.max(1),
            scan_session: None,
            sqlite_search: None,
            sqlite_path: None,
            sqlite_usn: 0,
            database_path: database_path.to_owned(),
        })
    }

    /// Volume identifier.
    #[must_use]
    pub fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// Current synchronization state.
    #[must_use]
    pub fn state(&self) -> SyncState {
        self.sync.state()
    }

    /// Current in-memory index (tests only).
    #[cfg(test)]
    #[must_use]
    pub fn memory(&self) -> &MemoryIndex {
        &self.memory
    }

    /// Number of indexed records.
    #[must_use]
    pub fn indexed_files(&self) -> usize {
        #[cfg(test)]
        if !self.memory.is_empty() {
            return self.memory.len();
        }
        usize::try_from(self.store.file_count().unwrap_or(0)).unwrap_or(usize::MAX)
    }

    /// Enable the SQLite + trigram backend and build its first snapshot.
    pub fn enable_sqlite_search(&mut self) -> Result<(), PipelineError> {
        self.sqlite_path = Some(self.database_path.with_extension("trigram.bin"));
        self.rebuild_sqlite_search()
    }

    fn rebuild_sqlite_search(&mut self) -> Result<(), PipelineError> {
        let path = self.sqlite_path.clone().unwrap_or_default();
        self.sqlite_search = None;
        self.sqlite_usn = 0;
        let delta = delta_directory(&path);
        if delta.is_dir() {
            std::fs::remove_dir_all(&delta)
                .map_err(|error| PipelineError::Source(error.to_string()))?;
        }
        self.store.build_trigram_index(&self.volume, &path)?;
        let search = SqliteSearch::open(&self.database_path, &path, self.volume.clone())?;
        self.sqlite_search = Some(search);
        self.sqlite_usn = self.committed_usn();
        Ok(())
    }

    fn append_sqlite_delta(&mut self, frns: &HashSet<u64>) -> Result<(), PipelineError> {
        let Some(primary) = self.sqlite_path.clone() else {
            return Ok(());
        };
        if frns.is_empty() {
            return Ok(());
        }
        let directory = delta_directory(&primary);
        std::fs::create_dir_all(&directory)
            .map_err(|error| PipelineError::Source(error.to_string()))?;
        let shard = directory.join(format!("delta-{:020}.bin", self.committed_usn()));
        let values: Vec<u64> = frns.iter().copied().collect();
        self.store
            .build_trigram_delta(&self.volume, &shard, &values)?;
        let search = self.sqlite_search.as_mut().ok_or_else(|| {
            PipelineError::InvalidState("sqlite search is not enabled".to_owned())
        })?;
        search.add_shard(&shard)?;
        self.sqlite_usn = self.committed_usn();
        Ok(())
    }

    fn sync_sqlite_delta(&mut self, changed: &HashSet<u64>) {
        if self.append_sqlite_delta(changed).is_err() {
            self.invalidate_sqlite_search();
        }
    }

    fn invalidate_sqlite_search(&mut self) {
        self.sqlite_search = None;
        self.sqlite_usn = 0;
    }

    fn sqlite_outcome(&self, query: &str, limit: u32) -> Option<SearchOutcome> {
        if self.sqlite_usn != self.committed_usn() {
            return None;
        }
        let search = self.sqlite_search.as_ref()?;
        search.search(query, limit).ok()
    }

    fn empty_outcome(query: &str, limit: u32) -> SearchOutcome {
        let _ = (query, limit);
        SearchOutcome::default()
    }

    /// Search through the SQLite snapshot when it is current, otherwise fall
    /// back to the live in-memory index.
    #[must_use]
    pub fn search(&self, query: &str, limit: u32) -> SearchOutcome {
        if let Some(outcome) = self.sqlite_outcome(query, limit) {
            return outcome;
        }
        #[cfg(test)]
        {
            self.memory.search(query, limit)
        }
        #[cfg(not(test))]
        {
            Self::empty_outcome(query, limit)
        }
    }

    /// Volume identifier for this pipeline.
    #[must_use]
    pub fn volume_id(&self) -> &VolumeId {
        &self.volume
    }

    /// Current committed USN.
    #[must_use]
    pub fn committed_usn(&self) -> u64 {
        self.sync.committed_usn()
    }

    /// Current index generation.
    #[must_use]
    pub fn index_generation(&self) -> u64 {
        self.sync.index_generation()
    }

    /// Bound journal identifier.
    #[must_use]
    pub fn journal_id(&self) -> Option<u64> {
        self.sync.journal_id()
    }

    /// Persist a stopped state after service shutdown.
    pub fn mark_stopped(&mut self) -> Result<(), PipelineError> {
        let restart_state = self.sync.restart_state();
        self.sync.apply(pstart_index::SyncEvent::Shutdown)?;
        self.scan_session = None;
        let persisted = restart_state.unwrap_or(SyncState::Stopped);
        self.store.set_sync_state(persisted, now())?;
        Ok(())
    }

    /// Decide whether to scan or catch up.
    pub fn startup_action(
        &self,
        source: &dyn VolumeSource,
    ) -> Result<StartupAction, PipelineError> {
        let journal = source.journal()?;
        validate_journal(&journal)?;
        let persisted = self.store.load_volume_state()?;
        let state = persisted
            .as_ref()
            .and_then(|value| SyncState::parse(&value.sync_state))
            .unwrap_or(SyncState::Uninitialized);
        let checkpoint = self.store.load_checkpoint()?;

        match state {
            SyncState::CatchUp | SyncState::LiveSync | SyncState::Paused | SyncState::Stopped => {
                match checkpoint {
                    Some(checkpoint) if checkpoint_is_usable(&checkpoint, &journal) => {
                        Ok(StartupAction::CatchUp)
                    }
                    _ => Ok(StartupAction::Scan),
                }
            }
            SyncState::InitialScan
            | SyncState::Uninitialized
            | SyncState::RebuildRequired
            | SyncState::Failed => Ok(StartupAction::Scan),
        }
    }

    /// Abandon an in-progress scan before a controlled restart.
    pub fn abort_scan(&mut self) {
        self.scan_session = None;
    }

    /// Clear persisted state and begin a full MFT scan.
    pub fn begin_scan(&mut self, source: &dyn VolumeSource) -> Result<(), PipelineError> {
        if self.scan_session.is_some() {
            return Err(PipelineError::InvalidState(
                "scan already in progress".to_owned(),
            ));
        }
        let journal = source.journal()?;
        validate_journal(&journal)?;
        let scan_usn = u64::try_from(journal.next_usn)
            .map_err(|_| PipelineError::InvalidState("negative journal next_usn".to_owned()))?;

        let next_generation = self.sync.index_generation().saturating_add(1);
        let mut next_sync = VolumeSync::restore(SyncState::Uninitialized, None, next_generation, 0);
        next_sync.observe_journal(journal.journal_id);
        next_sync.apply(pstart_index::SyncEvent::Initialize)?;
        next_sync.observe_usn(scan_usn);
        self.sync = next_sync;

        self.store
            .reset_for_scan(journal.journal_id, self.sync.index_generation(), now())?;
        #[cfg(test)]
        {
            self.memory = MemoryIndex::new(self.volume.clone(), self.root.clone(), Frn::ROOT, 256)?;
        }
        self.scan_session = Some(ScanSession {
            journal,
            next_frn: 0,
            scan_usn,
            pending: PendingBatch::default(),
            report: ScanReport {
                scan_usn,
                ..ScanReport::default()
            },
        });
        Ok(())
    }

    /// Advance an in-progress scan by at most `max_pages` pages.
    ///
    /// Returns `None` while more pages remain and `Some(report)` on completion.
    pub fn scan_step(
        &mut self,
        source: &dyn VolumeSource,
        max_pages: usize,
    ) -> Result<Option<ScanReport>, PipelineError> {
        let Some(mut session) = self.scan_session.take() else {
            return Err(PipelineError::InvalidState(
                "no scan in progress".to_owned(),
            ));
        };
        let mut remaining = max_pages.max(1);
        loop {
            let page = source.mft_page(session.next_frn, &session.journal)?;
            session.report.pages += 1;
            for record in &page.records {
                session.report.records += 1;
                self.apply_record(record, &mut session.pending, SyncState::InitialScan)?;
                if session.pending.len() >= self.batch_limit {
                    let checkpoint_usn = session.pending.last_usn;
                    self.flush_pending(
                        &mut session.pending,
                        checkpoint_usn,
                        SyncState::InitialScan,
                    )?;
                    session.report.batches += 1;
                }
            }
            if page.next_start_frn == 0 {
                self.flush_pending(&mut session.pending, session.scan_usn, SyncState::CatchUp)?;
                session.report.batches += 1;
                self.sync.apply(pstart_index::SyncEvent::ScanCompleted)?;
                self.store.set_sync_state(SyncState::CatchUp, now())?;
                return Ok(Some(session.report));
            }
            if page.next_start_frn <= session.next_frn {
                return Err(PipelineError::NoProgress {
                    start_frn: session.next_frn,
                });
            }
            session.next_frn = page.next_start_frn;
            remaining -= 1;
            if remaining == 0 {
                self.scan_session = Some(session);
                return Ok(None);
            }
        }
    }

    /// Clear persisted state and perform a full MFT scan.
    pub fn initial_scan(&mut self, source: &dyn VolumeSource) -> Result<ScanReport, PipelineError> {
        self.begin_scan(source)?;
        loop {
            if let Some(report) = self.scan_step(source, 1)? {
                if self.sqlite_path.is_some() {
                    self.rebuild_sqlite_search()?;
                }
                return Ok(report);
            }
        }
    }

    /// Apply journal records until the tail is reached or the batch budget ends.
    pub fn catch_up(
        &mut self,
        source: &dyn VolumeSource,
        max_batches: usize,
    ) -> Result<CatchUpReport, PipelineError> {
        if matches!(self.sync.state(), SyncState::Stopped | SyncState::Paused) {
            self.sync.apply(pstart_index::SyncEvent::ResumeCatchUp)?;
        }
        if !matches!(self.sync.state(), SyncState::CatchUp | SyncState::LiveSync) {
            return Err(PipelineError::InvalidState(format!(
                "catch-up requires CatchUp/LiveSync, got {:?}",
                self.sync.state()
            )));
        }
        let mut report = CatchUpReport::default();
        let mut changed = HashSet::new();
        for _ in 0..max_batches.max(1) {
            let journal = source.journal()?;
            validate_journal(&journal)?;
            let expected = self
                .sync
                .journal_id()
                .ok_or_else(|| PipelineError::InvalidState("no journal bound".to_owned()))?;
            if journal.journal_id != expected {
                return Err(PipelineError::JournalChanged {
                    expected,
                    observed: journal.journal_id,
                });
            }
            let start = self.sync.committed_usn();
            let lowest = u64::try_from(journal.lowest_valid_usn).unwrap_or(0);
            if start < lowest {
                return Err(PipelineError::JournalLost {
                    start_usn: start,
                    lowest_valid_usn: journal.lowest_valid_usn,
                });
            }
            let next = u64::try_from(journal.next_usn).unwrap_or(0);
            if start > next {
                return Err(PipelineError::JournalRewound {
                    start_usn: start,
                    next_usn: next,
                });
            }
            let batch = source.read_journal(
                i64::try_from(start)
                    .expect("committed USN always originates from a signed journal value"),
                &journal,
                0,
            )?;
            let target = u64::try_from(batch.next_usn)
                .map_err(|_| PipelineError::InvalidState("negative journal next_usn".to_owned()))?;
            match classify_batch_range(start, target, next) {
                BatchRange::Ahead => {
                    // A concurrent journal append can make the read batch
                    // newer than the snapshot. Leave the checkpoint untouched;
                    // the next tick re-reads from the same USN.
                    self.sync_sqlite_delta(&changed);
                    return Ok(report);
                }
                BatchRange::Rewound => {
                    return Err(PipelineError::JournalRewound {
                        start_usn: start,
                        next_usn: target,
                    });
                }
                BatchRange::InRange => {}
            }
            let state = self.sync.state();
            let mut pending = PendingBatch::default();
            for record in &batch.records {
                let record_usn = u64::try_from(record.usn)
                    .map_err(|_| PipelineError::InvalidState("negative record USN".to_owned()))?;
                if !record_is_within_batch(record_usn, target) {
                    return Err(PipelineError::InvalidState(format!(
                        "record USN {record_usn} is ahead of batch {target}"
                    )));
                }
                report.records += 1;
                changed.insert(record.frn);
                self.apply_record(record, &mut pending, state)?;
                if pending.len() >= self.batch_limit {
                    let checkpoint_usn = pending.last_usn;
                    self.flush_pending(&mut pending, checkpoint_usn, state)?;
                    report.batches += 1;
                }
            }
            self.flush_pending(&mut pending, target, state)?;
            report.batches += 1;

            let at_tail = target >= next;
            if at_tail && batch.records.is_empty() {
                if self.sync.state() == SyncState::CatchUp {
                    self.sync.apply(pstart_index::SyncEvent::ReplayCompleted)?;
                    self.store.set_sync_state(SyncState::LiveSync, now())?;
                }
                report.caught_up = true;
                self.sync_sqlite_delta(&changed);
                return Ok(report);
            }
            if target == start && batch.records.is_empty() {
                self.sync_sqlite_delta(&changed);
                return Ok(report);
            }
        }
        self.sync_sqlite_delta(&changed);
        Ok(report)
    }

    fn apply_record(
        &mut self,
        record: &UsnRecord,
        pending: &mut PendingBatch,
        batch_state: SyncState,
    ) -> Result<(), PipelineError> {
        if is_reserved_metadata(record) {
            return Ok(());
        }
        let Some(event) = record.to_index_event()? else {
            return Ok(());
        };
        match event {
            pstart_index::IndexEvent::Upsert(file) => {
                if pending.seen.get(&file.frn) == Some(&OperationKind::Delete) {
                    let checkpoint_usn = pending.last_usn;
                    self.flush_pending(pending, checkpoint_usn, batch_state)?;
                }
                #[cfg(test)]
                self.memory.upsert(file.clone())?;
                pending.push_upsert(file, record.usn as u64);
            }
            pstart_index::IndexEvent::Delete { frn, usn } => {
                if pending.seen.get(&frn) == Some(&OperationKind::Upsert) {
                    let checkpoint_usn = pending.last_usn;
                    self.flush_pending(pending, checkpoint_usn, batch_state)?;
                }
                #[cfg(test)]
                self.memory.delete(Frn::new(frn), usn)?;
                pending.push_delete(DeletedFrn { frn, usn }, usn);
            }
        }
        Ok(())
    }

    fn flush_pending(
        &mut self,
        pending: &mut PendingBatch,
        checkpoint_usn: u64,
        state: SyncState,
    ) -> Result<(), PipelineError> {
        let checkpoint_usn = effective_checkpoint_usn(checkpoint_usn, pending.last_usn);
        let journal_id = self
            .sync
            .journal_id()
            .ok_or_else(|| PipelineError::InvalidState("no journal bound".to_owned()))?;
        let checkpoint = Checkpoint {
            journal_id,
            next_usn: checkpoint_usn,
            index_generation: self.sync.index_generation(),
            committed_at: now(),
        };
        self.store.commit_batch(&PersistBatch {
            upserts: std::mem::take(&mut pending.upserts),
            deletes: std::mem::take(&mut pending.deletes),
            checkpoint: checkpoint.clone(),
            sync_state: Some(state),
        })?;
        self.sync.observe_usn(checkpoint_usn);
        self.sync.commit_checkpoint(&checkpoint)?;
        pending.seen.clear();
        pending.last_usn = checkpoint_usn;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationKind {
    Upsert,
    Delete,
}

#[derive(Debug)]
struct ScanSession {
    journal: JournalInfo,
    next_frn: u64,
    scan_usn: u64,
    pending: PendingBatch,
    report: ScanReport,
}

#[derive(Debug, Default)]
struct PendingBatch {
    upserts: Vec<FileRecord>,
    deletes: Vec<DeletedFrn>,
    seen: HashMap<u64, OperationKind>,
    last_usn: u64,
}

impl PendingBatch {
    fn len(&self) -> usize {
        self.upserts.len() + self.deletes.len()
    }

    fn push_upsert(&mut self, record: FileRecord, usn: u64) {
        self.seen.insert(record.frn, OperationKind::Upsert);
        self.last_usn = self.last_usn.max(usn);
        self.upserts.push(record);
    }

    fn push_delete(&mut self, deleted: DeletedFrn, usn: u64) {
        self.seen.insert(deleted.frn, OperationKind::Delete);
        self.last_usn = self.last_usn.max(usn);
        self.deletes.push(deleted);
    }
}

fn is_reserved_metadata(record: &UsnRecord) -> bool {
    const RESERVED: &[&str] = &[
        "$MFT",
        "$MFTMirr",
        "$LogFile",
        "$Volume",
        "$AttrDef",
        "$Bitmap",
        "$Boot",
        "$BadClus",
        "$Secure",
        "$UpCase",
        "$Extend",
        "$RmMetadata",
        "$Txf",
        "$TxfLog",
    ];
    Frn::new(record.parent_frn).record_number() == Frn::ROOT.record_number()
        && RESERVED.contains(&record.name.as_str())
}

fn validate_journal(journal: &JournalInfo) -> Result<(), PipelineError> {
    journal.validate()?;
    if journal.lowest_valid_usn < 0 || journal.next_usn < 0 {
        return Err(PipelineError::InvalidState(
            "journal USN fields must be non-negative".to_owned(),
        ));
    }
    Ok(())
}

fn checkpoint_is_usable(checkpoint: &Checkpoint, journal: &JournalInfo) -> bool {
    checkpoint.journal_id == journal.journal_id
        && checkpoint.next_usn >= u64::try_from(journal.lowest_valid_usn).unwrap_or(0)
        && checkpoint.next_usn <= u64::try_from(journal.next_usn).unwrap_or(0)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(windows)]
mod ntfs_source {
    use pstart_ntfs::{
        JournalBatch, JournalInfo, MftBatch, VolumeHandle, DEFAULT_IOCTL_BUFFER_SIZE,
    };

    use super::{PipelineError, VolumeSource};

    /// Production source backed by a Windows NTFS volume handle.
    pub struct NtfsSource {
        handle: VolumeHandle,
        buffer_size: usize,
    }

    impl NtfsSource {
        /// Open a live NTFS volume.
        pub fn open(root: &str) -> Result<Self, PipelineError> {
            Ok(Self {
                handle: VolumeHandle::open(root)?,
                buffer_size: DEFAULT_IOCTL_BUFFER_SIZE,
            })
        }
    }

    impl VolumeSource for NtfsSource {
        fn journal(&self) -> Result<JournalInfo, PipelineError> {
            Ok(self.handle.query_journal()?)
        }

        fn mft_page(
            &self,
            start_frn: u64,
            journal: &JournalInfo,
        ) -> Result<MftBatch, PipelineError> {
            Ok(self
                .handle
                .enum_mft_page(start_frn, journal, self.buffer_size)?)
        }

        fn read_journal(
            &self,
            start_usn: i64,
            journal: &JournalInfo,
            timeout_ms: u32,
        ) -> Result<JournalBatch, PipelineError> {
            Ok(self
                .handle
                .read_journal(start_usn, timeout_ms, journal, self.buffer_size)?)
        }
    }
}

#[cfg(windows)]
pub use ntfs_source::NtfsSource;

fn effective_checkpoint_usn(requested: u64, pending_last: u64) -> u64 {
    if pending_last > requested {
        pending_last
    } else {
        requested
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchRange {
    InRange,
    Ahead,
    Rewound,
}

fn classify_batch_range(start: u64, target: u64, next: u64) -> BatchRange {
    if target > next {
        BatchRange::Ahead
    } else if target < start {
        BatchRange::Rewound
    } else {
        BatchRange::InRange
    }
}

fn record_is_within_batch(record_usn: u64, target: u64) -> bool {
    record_usn <= target
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_ntfs::{
        JournalBatch, MftBatch, UsnRecord, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    };
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use tempfile::TempDir;

    struct FakeSource {
        journal: RefCell<JournalInfo>,
        pages: Vec<(u64, MftBatch)>,
        batches: RefCell<VecDeque<JournalBatch>>,
        fail_start: RefCell<Option<u64>>,
        fail_read: RefCell<bool>,
    }

    impl FakeSource {
        fn new(journal: JournalInfo, pages: Vec<(u64, MftBatch)>) -> Self {
            Self {
                journal: RefCell::new(journal),
                pages,
                batches: RefCell::new(VecDeque::new()),
                fail_start: RefCell::new(None),
                fail_read: RefCell::new(false),
            }
        }

        fn push_batch(&self, batch: JournalBatch) {
            self.batches.borrow_mut().push_back(batch);
        }
    }

    impl VolumeSource for FakeSource {
        fn journal(&self) -> Result<JournalInfo, PipelineError> {
            Ok(self.journal.borrow().clone())
        }

        fn mft_page(
            &self,
            start_frn: u64,
            _journal: &JournalInfo,
        ) -> Result<MftBatch, PipelineError> {
            if *self.fail_start.borrow() == Some(start_frn) {
                return Err(PipelineError::Source("injected page failure".to_owned()));
            }
            self.pages
                .iter()
                .find(|(start, _)| *start == start_frn)
                .map(|(_, page)| page.clone())
                .ok_or_else(|| PipelineError::Source(format!("no page for FRN {start_frn}")))
        }

        fn read_journal(
            &self,
            _start_usn: i64,
            journal: &JournalInfo,
            _timeout_ms: u32,
        ) -> Result<JournalBatch, PipelineError> {
            if *self.fail_read.borrow() {
                return Err(PipelineError::Source("injected journal failure".to_owned()));
            }
            Ok(self
                .batches
                .borrow_mut()
                .pop_front()
                .unwrap_or(JournalBatch {
                    next_usn: journal.next_usn,
                    records: Vec::new(),
                }))
        }
    }

    fn meta() -> VolumeMeta {
        VolumeMeta::new(VolumeId::new("C:").unwrap(), "System", "NTFS").unwrap()
    }

    fn journal(id: u64, next_usn: i64) -> JournalInfo {
        JournalInfo {
            journal_id: id,
            first_usn: 10,
            next_usn,
            lowest_valid_usn: 0,
            max_usn: 10_000,
            maximum_size: 1024 * 1024,
            allocation_delta: 4096,
        }
    }

    fn record(
        frn: u64,
        parent: u64,
        name: &str,
        usn: i64,
        reason: u32,
        attributes: u32,
    ) -> UsnRecord {
        UsnRecord {
            frn,
            parent_frn: parent,
            usn,
            timestamp: 0,
            reason,
            source_info: 0,
            security_id: 0,
            attributes,
            name: name.into(),
        }
    }

    fn page(start: u64, next: u64, records: Vec<UsnRecord>) -> (u64, MftBatch) {
        (
            start,
            MftBatch {
                next_start_frn: next,
                records,
            },
        )
    }

    fn sample_pages() -> Vec<(u64, MftBatch)> {
        vec![
            page(
                0,
                50,
                vec![
                    record(11, 5, "Users", 1, 0, 0x10),
                    record(12, 11, "a.txt", 2, USN_REASON_FILE_CREATE, 0),
                ],
            ),
            page(
                50,
                0,
                vec![record(13, 11, "b.txt", 3, USN_REASON_FILE_CREATE, 0)],
            ),
        ]
    }

    fn open_pipeline(dir: &TempDir, batch_limit: usize) -> IndexPipeline {
        IndexPipeline::open(meta(), "C:\\", &dir.path().join("index.db"), batch_limit).unwrap()
    }

    #[test]
    fn sqlite_helper_paths_are_covered() {
        let dir = TempDir::new().unwrap();
        let mut pipeline = open_pipeline(&dir, 1);
        assert_eq!(pipeline.indexed_files(), 0);
        assert_eq!(
            IndexPipeline::empty_outcome("alpha", 1),
            SearchOutcome::default()
        );
        pipeline.enable_sqlite_search().unwrap();
        let path = pipeline.sqlite_path.clone().unwrap();
        std::fs::create_dir_all(delta_directory(&path)).unwrap();
        pipeline.rebuild_sqlite_search().unwrap();
        pipeline.sqlite_search = None;
        pipeline.sync_sqlite_delta(&HashSet::from([1_u64]));
        assert!(pipeline.sqlite_search.is_none());
    }

    #[test]
    fn effective_checkpoint_covers_both_inputs() {
        assert_eq!(effective_checkpoint_usn(10, 5), 10);
        assert_eq!(effective_checkpoint_usn(5, 10), 10);
        assert_eq!(effective_checkpoint_usn(5, 5), 5);
    }

    #[test]
    fn fresh_scan_and_catchup_reach_live_sync() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );

        let report = pipeline.initial_scan(&source).unwrap();
        assert_eq!(report.scan_usn, 100);
        assert_eq!(report.pages, 2);
        assert_eq!(report.records, 3);
        assert!(report.batches >= 2);
        assert_eq!(pipeline.state(), SyncState::CatchUp);
        assert_eq!(pipeline.committed_usn(), 100);
        assert_eq!(pipeline.index_generation(), 1);
        assert_eq!(pipeline.memory().search("a.txt", 10).hits.len(), 1);
        assert_eq!(pipeline.store.file_count().unwrap(), 3);

        source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        let report = pipeline.catch_up(&source, 4).unwrap();
        assert!(report.caught_up);
        assert_eq!(pipeline.state(), SyncState::LiveSync);
        assert_eq!(
            pipeline
                .store
                .load_volume_state()
                .unwrap()
                .unwrap()
                .sync_state,
            "live_sync"
        );

        let reopened = open_pipeline(&dir, 2);
        assert_eq!(reopened.state(), SyncState::LiveSync);
        assert_eq!(reopened.memory().len(), 3);
        assert_eq!(
            reopened.startup_action(&source).unwrap(),
            StartupAction::CatchUp
        );
    }

    #[test]
    fn chunked_scan_exposes_partial_index() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.begin_scan(&source).unwrap();
        assert_eq!(pipeline.state(), SyncState::InitialScan);
        assert!(pipeline.scan_step(&source, 1).unwrap().is_none());
        assert!(!pipeline.memory().is_empty());
        assert!(pipeline.memory().len() < 3);
        let mut report = None;
        while report.is_none() {
            report = pipeline.scan_step(&source, 1).unwrap();
        }
        let report = report.unwrap();
        assert_eq!(report.pages, 2);
        assert_eq!(report.records, 3);
        assert_eq!(pipeline.memory().len(), 3);
        assert_eq!(pipeline.state(), SyncState::CatchUp);
    }

    #[test]
    fn scan_step_without_begin_is_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        assert!(matches!(
            pipeline.scan_step(&source, 1),
            Err(PipelineError::InvalidState(_))
        ));
    }

    #[test]
    fn begin_scan_twice_is_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.begin_scan(&source).unwrap();
        assert!(matches!(
            pipeline.begin_scan(&source),
            Err(PipelineError::InvalidState(_))
        ));
    }

    #[test]
    fn index_transitions_match_pipeline() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);

        // StartScan: Uninitialized -> InitialScan.
        assert_eq!(pipeline.state(), SyncState::Uninitialized);
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );

        // FinishScan: InitialScan -> CatchUp.
        pipeline.initial_scan(&source).unwrap();
        assert_eq!(pipeline.state(), SyncState::CatchUp);
        assert_eq!(pipeline.committed_usn(), 100);

        // GoLive: CatchUp -> LiveSync after consuming the journal tail.
        source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        pipeline.catch_up(&source, 4).unwrap();
        assert_eq!(pipeline.state(), SyncState::LiveSync);

        // Reset: journal identity change requires a full scan.
        *source.journal.borrow_mut() = journal(8, 100);
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );

        // Reset: journal rewind is resettable and re-enters InitialScan.
        *source.journal.borrow_mut() = journal(7, 80);
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::JournalRewound {
                start_usn: 100,
                next_usn: 80
            })
        ));
        pipeline.initial_scan(&source).unwrap();
        assert_eq!(pipeline.state(), SyncState::CatchUp);
        assert_eq!(pipeline.committed_usn(), 80);
    }

    #[test]
    fn catchup_applies_rename_and_delete() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 105,
            records: vec![record(12, 11, "renamed.txt", 104, 0, 0)],
        });
        source.push_batch(JournalBatch {
            next_usn: 110,
            records: vec![record(13, 11, "b.txt", 109, USN_REASON_FILE_DELETE, 0)],
        });
        source.push_batch(JournalBatch {
            next_usn: 110,
            records: Vec::new(),
        });
        *source.journal.borrow_mut() = journal(7, 110);

        let report = pipeline.catch_up(&source, 4).unwrap();
        assert!(report.caught_up);
        assert_eq!(report.records, 2);
        assert_eq!(pipeline.state(), SyncState::LiveSync);
        assert_eq!(pipeline.committed_usn(), 110);
        assert_eq!(pipeline.memory().search("renamed", 10).hits.len(), 1);
        assert!(pipeline.memory().search("b.txt", 10).hits.is_empty());
        assert_eq!(pipeline.store.file_count().unwrap(), 2);
    }

    #[test]
    fn locked_store_fails_first_batch_flush() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 1);
        let locker = rusqlite::Connection::open(dir.path().join("index.db")).unwrap();
        locker
            .execute_batch("PRAGMA busy_timeout = 0; BEGIN IMMEDIATE")
            .unwrap();
        assert!(pipeline.initial_scan(&source).is_err());
    }

    #[test]
    fn locked_store_does_not_advance_checkpoint() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        let locker = rusqlite::Connection::open(dir.path().join("index.db")).unwrap();
        locker
            .execute_batch("PRAGMA busy_timeout = 0; BEGIN IMMEDIATE")
            .unwrap();
        let error = pipeline.initial_scan(&source).unwrap_err();
        assert!(matches!(error, PipelineError::Index(_)));
        assert_eq!(pipeline.committed_usn(), 0);
        assert!(pipeline.store.load_checkpoint().unwrap().is_none());
        drop(locker);
    }

    #[test]
    fn interrupted_scan_is_restarted_and_generation_increases() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        *source.fail_start.borrow_mut() = Some(50);
        let mut pipeline = open_pipeline(&dir, 1);
        let error = pipeline.initial_scan(&source).unwrap_err();
        assert!(matches!(error, PipelineError::Source(_)));
        assert_eq!(
            pipeline
                .store
                .load_volume_state()
                .unwrap()
                .unwrap()
                .sync_state,
            "initial_scan"
        );

        drop(pipeline);
        *source.fail_start.borrow_mut() = None;
        let mut restarted = open_pipeline(&dir, 1);
        assert_eq!(
            restarted.startup_action(&source).unwrap(),
            StartupAction::Scan
        );
        let report = restarted.initial_scan(&source).unwrap();
        assert_eq!(report.records, 3);
        assert_eq!(restarted.index_generation(), 2);
        assert_eq!(restarted.store.file_count().unwrap(), 3);
        assert_eq!(restarted.state(), SyncState::CatchUp);
    }

    #[test]
    fn reserved_root_metadata_is_not_indexed() {
        let dir = TempDir::new().unwrap();
        let pages = vec![
            page(
                0,
                5,
                vec![
                    record(10, 5, "$MFT", 10, 0, 0),
                    record(11, 5, "$RmMetadata", 11, 0, 0),
                    record(12, 5, "$custom", 12, 0, 0),
                    record(13, 5, "normal.txt", 13, 0, 0),
                    record(14, 0x0005_0000_0000_0005, "$Extend", 14, 0, 0),
                ],
            ),
            page(5, 0, Vec::new()),
        ];
        let source = FakeSource::new(journal(7, 100), pages);
        let mut pipeline = open_pipeline(&dir, 2);
        let report = pipeline.initial_scan(&source).unwrap();
        assert_eq!(report.records, 5);
        assert_eq!(pipeline.store.file_count().unwrap(), 2);
        assert_eq!(
            pipeline.memory.record(Frn::new(12)).unwrap().name.as_ref(),
            "$custom"
        );
        assert!(pipeline.memory.record(Frn::new(10)).is_none());
        assert!(pipeline.memory.record(Frn::new(14)).is_none());
    }

    #[test]
    fn journal_change_and_range_loss_require_scan() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        pipeline.catch_up(&source, 2).unwrap();

        *source.journal.borrow_mut() = journal(8, 100);
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );

        *source.journal.borrow_mut() = JournalInfo {
            first_usn: 150,
            lowest_valid_usn: 150,
            ..journal(7, 200)
        };
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );
    }

    #[test]
    fn stopped_checkpoint_resumes_catchup() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        pipeline.mark_stopped().unwrap();

        let mut reopened = open_pipeline(&dir, 2);
        assert_eq!(reopened.state(), SyncState::Stopped);
        assert_eq!(
            reopened.startup_action(&source).unwrap(),
            StartupAction::CatchUp
        );
        source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        assert!(reopened.catch_up(&source, 1).unwrap().caught_up);
        assert_eq!(reopened.state(), SyncState::LiveSync);
    }

    #[test]
    fn paused_partial_scan_restarts_with_scan() {
        let dir = TempDir::new().unwrap();
        let pages = (0..300_u64)
            .map(|index| {
                page(
                    index * 10,
                    if index == 299 { 0 } else { (index + 1) * 10 },
                    vec![record(
                        10 + index,
                        5,
                        &format!("f{index}"),
                        i64::try_from(index + 1).unwrap(),
                        USN_REASON_FILE_CREATE,
                        0,
                    )],
                )
            })
            .collect();
        let source = FakeSource::new(journal(7, 10_000), pages);
        let mut pipeline = open_pipeline(&dir, 1);
        pipeline.begin_scan(&source).unwrap();
        pipeline.scan_step(&source, 1).unwrap();
        pipeline
            .sync
            .apply(pstart_index::SyncEvent::VolumeUnavailable)
            .unwrap();
        assert_eq!(pipeline.state(), SyncState::Paused);
        pipeline.mark_stopped().unwrap();

        let reopened = open_pipeline(&dir, 2);
        assert_eq!(
            reopened.startup_action(&source).unwrap(),
            StartupAction::Scan
        );
    }

    #[test]
    fn shutdown_preserves_paused_rebuild_states() {
        for target in [SyncState::RebuildRequired, SyncState::Failed] {
            let dir = TempDir::new().unwrap();
            let source = FakeSource::new(journal(7, 100), sample_pages());
            let mut pipeline = open_pipeline(&dir, 2);
            pipeline.initial_scan(&source).unwrap();
            let event = if target == SyncState::RebuildRequired {
                pstart_index::SyncEvent::JournalReset
            } else {
                pstart_index::SyncEvent::Fail
            };
            pipeline.sync.apply(event).unwrap();
            pipeline
                .sync
                .apply(pstart_index::SyncEvent::VolumeUnavailable)
                .unwrap();
            assert_eq!(pipeline.state(), SyncState::Paused);
            pipeline.mark_stopped().unwrap();

            let reopened = open_pipeline(&dir, 2);
            assert_eq!(
                reopened.startup_action(&source).unwrap(),
                StartupAction::Scan,
                "{target:?}"
            );
        }
    }

    #[test]
    fn shutdown_preserves_rebuild_and_failed_states() {
        for target in [SyncState::RebuildRequired, SyncState::Failed] {
            let dir = TempDir::new().unwrap();
            let source = FakeSource::new(journal(7, 100), sample_pages());
            let mut pipeline = open_pipeline(&dir, 2);
            pipeline.initial_scan(&source).unwrap();
            let event = if target == SyncState::RebuildRequired {
                pstart_index::SyncEvent::JournalReset
            } else {
                pstart_index::SyncEvent::Fail
            };
            pipeline.sync.apply(event).unwrap();
            assert_eq!(pipeline.state(), target);
            pipeline.mark_stopped().unwrap();

            let reopened = open_pipeline(&dir, 2);
            assert_eq!(
                reopened.startup_action(&source).unwrap(),
                StartupAction::Scan,
                "{target:?}"
            );
        }
    }

    #[test]
    fn checkpoint_ahead_of_journal_requires_scan() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();

        *source.journal.borrow_mut() = journal(7, 80);
        assert_eq!(
            pipeline.startup_action(&source).unwrap(),
            StartupAction::Scan
        );
    }

    #[test]
    fn catchup_rejects_record_ahead_of_batch() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 100,
            records: vec![record(14, 11, "ahead.txt", 101, USN_REASON_FILE_CREATE, 0)],
        });
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::InvalidState(_))
        ));
        assert_eq!(pipeline.committed_usn(), 100);
        assert!(pipeline.memory.record(Frn::new(14)).is_none());
    }

    #[test]
    fn catchup_retries_batch_ahead_of_stale_journal() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 101,
            records: Vec::new(),
        });
        let report = pipeline.catch_up(&source, 1).unwrap();
        assert!(report.batches == 0);
        assert!(!report.caught_up);
        assert_eq!(pipeline.committed_usn(), 100);
    }

    #[test]
    fn catchup_rejects_journal_rewind() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(7, 80);
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::JournalRewound {
                start_usn: 100,
                next_usn: 80
            })
        ));
    }

    #[test]
    fn catchup_rejects_journal_identity_change() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(8, 110);
        let error = pipeline.catch_up(&source, 1).unwrap_err();
        assert!(matches!(
            error,
            PipelineError::JournalChanged {
                expected: 7,
                observed: 8
            }
        ));
    }

    #[test]
    fn catchup_rejects_lost_journal_range() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = JournalInfo {
            first_usn: 150,
            lowest_valid_usn: 150,
            ..journal(7, 200)
        };
        let error = pipeline.catch_up(&source, 1).unwrap_err();
        assert!(matches!(error, PipelineError::JournalLost { .. }));
    }

    #[test]
    fn delete_then_recreate_is_ordered_across_transactions() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 16);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 112,
            records: vec![
                record(12, 11, "a.txt", 110, USN_REASON_FILE_DELETE, 0),
                record(12, 11, "new.txt", 111, USN_REASON_FILE_CREATE, 0),
            ],
        });
        *source.journal.borrow_mut() = journal(7, 112);
        pipeline.catch_up(&source, 2).unwrap();
        assert_eq!(
            pipeline
                .memory()
                .record(Frn::new(12))
                .unwrap()
                .name
                .as_ref(),
            "new.txt"
        );
        assert_eq!(pipeline.store.load_records().unwrap().len(), 3);
        assert_eq!(pipeline.committed_usn(), 112);
    }

    #[test]
    fn non_advancing_page_is_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(
            journal(7, 100),
            vec![
                page(0, 50, vec![record(11, 5, "Users", 1, 0, 0x10)]),
                page(50, 50, vec![record(12, 11, "a.txt", 2, 0, 0)]),
            ],
        );
        let mut pipeline = open_pipeline(&dir, 16);
        let error = pipeline.initial_scan(&source).unwrap_err();
        assert!(matches!(error, PipelineError::NoProgress { start_frn: 50 }));
    }

    #[test]
    fn empty_batch_advances_checkpoint_and_reaches_tail() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(7, 120);
        source.push_batch(JournalBatch {
            next_usn: 120,
            records: Vec::new(),
        });
        let report = pipeline.catch_up(&source, 2).unwrap();
        assert!(report.caught_up);
        assert_eq!(pipeline.committed_usn(), 120);
        assert_eq!(pipeline.state(), SyncState::LiveSync);
    }

    #[test]
    fn invalid_source_journal_is_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(
            JournalInfo {
                journal_id: 0,
                ..journal(7, 100)
            },
            sample_pages(),
        );
        let pipeline = open_pipeline(&dir, 2);
        assert!(matches!(
            pipeline.startup_action(&source),
            Err(PipelineError::Ntfs(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_ntfs_source_adapter() {
        let source = NtfsSource::open("C:\\").unwrap();
        let journal = source.journal().unwrap();
        assert!(journal.next_usn >= journal.first_usn);
        let page = source.mft_page(0, &journal).unwrap();
        assert!(!page.records.is_empty());
        let batch = source.read_journal(journal.next_usn, &journal, 0).unwrap();
        assert!(batch.next_usn >= journal.next_usn);
    }

    #[test]
    fn volume_getter_and_catchup_state_validation() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        assert_eq!(pipeline.volume().as_str(), "C:");
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::InvalidState(_))
        ));
    }

    #[test]
    fn catchup_ignores_old_rename_and_flushes_upsert_then_delete() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 16);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 106,
            records: vec![
                record(
                    12,
                    11,
                    "a.txt",
                    103,
                    pstart_ntfs::USN_REASON_RENAME_OLD_NAME,
                    0,
                ),
                record(12, 11, "x.txt", 104, USN_REASON_FILE_CREATE, 0),
                record(12, 11, "x.txt", 105, USN_REASON_FILE_DELETE, 0),
            ],
        });
        source.push_batch(JournalBatch {
            next_usn: 106,
            records: Vec::new(),
        });
        *source.journal.borrow_mut() = journal(7, 106);
        let report = pipeline.catch_up(&source, 4).unwrap();
        assert!(report.caught_up);
        assert!(pipeline.memory().record(Frn::new(12)).is_none());
        assert_eq!(pipeline.store.file_count().unwrap(), 2);
    }

    #[test]
    fn catchup_flushes_mid_batch_without_skipping_usns() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 1);
        pipeline.initial_scan(&source).unwrap();
        source.push_batch(JournalBatch {
            next_usn: 106,
            records: vec![
                record(14, 11, "c.txt", 104, USN_REASON_FILE_CREATE, 0),
                record(15, 11, "d.txt", 105, USN_REASON_FILE_CREATE, 0),
                record(16, 11, "e.txt", 106, USN_REASON_FILE_CREATE, 0),
            ],
        });
        source.push_batch(JournalBatch {
            next_usn: 106,
            records: Vec::new(),
        });
        *source.journal.borrow_mut() = journal(7, 106);
        let report = pipeline.catch_up(&source, 2).unwrap();
        assert!(report.caught_up);
        assert_eq!(pipeline.store.file_count().unwrap(), 6);
        assert_eq!(pipeline.committed_usn(), 106);
    }

    #[test]
    fn catchup_rejects_regressing_batch_and_stops_on_no_progress() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 4);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(7, 110);
        source.push_batch(JournalBatch {
            next_usn: 99,
            records: Vec::new(),
        });
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::JournalRewound {
                start_usn: 100,
                next_usn: 99
            })
        ));
        source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        let report = pipeline.catch_up(&source, 1).unwrap();
        assert!(!report.caught_up);
    }

    #[test]
    fn catchup_stops_at_batch_budget() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 4);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(7, 110);
        source.push_batch(JournalBatch {
            next_usn: 105,
            records: vec![record(14, 11, "c.txt", 104, USN_REASON_FILE_CREATE, 0)],
        });
        source.push_batch(JournalBatch {
            next_usn: 110,
            records: vec![record(15, 11, "d.txt", 109, USN_REASON_FILE_CREATE, 0)],
        });
        let report = pipeline.catch_up(&source, 1).unwrap();
        assert!(!report.caught_up);
        assert_eq!(report.batches, 1);
        assert_eq!(pipeline.committed_usn(), 105);
    }

    #[test]
    fn negative_journal_fields_are_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(
            JournalInfo {
                lowest_valid_usn: -1,
                ..journal(7, 100)
            },
            sample_pages(),
        );
        let pipeline = open_pipeline(&dir, 2);
        assert!(matches!(
            pipeline.startup_action(&source),
            Err(PipelineError::InvalidState(_))
        ));
    }

    #[test]
    fn negative_batch_target_is_rejected() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.journal.borrow_mut() = journal(7, 110);
        source.push_batch(JournalBatch {
            next_usn: -1,
            records: Vec::new(),
        });
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::InvalidState(_))
        ));
    }

    #[test]
    fn journal_read_failure_is_propagated() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), sample_pages());
        let mut pipeline = open_pipeline(&dir, 2);
        pipeline.initial_scan(&source).unwrap();
        *source.fail_read.borrow_mut() = true;
        assert!(matches!(
            pipeline.catch_up(&source, 1),
            Err(PipelineError::Source(_))
        ));
    }

    #[test]
    fn pipeline_error_codes_are_stable() {
        let cases = [
            (
                PipelineError::JournalChanged {
                    expected: 1,
                    observed: 2,
                },
                ErrorCode::JournalReset,
            ),
            (
                PipelineError::JournalLost {
                    start_usn: 1,
                    lowest_valid_usn: 2,
                },
                ErrorCode::JournalReset,
            ),
            (PipelineError::Source("x".to_owned()), ErrorCode::Internal),
            (
                PipelineError::NoProgress { start_frn: 1 },
                ErrorCode::Internal,
            ),
            (
                PipelineError::InvalidState("x".to_owned()),
                ErrorCode::Internal,
            ),
            (
                PipelineError::Index(IndexError::InvalidData("x".to_owned())),
                ErrorCode::Internal,
            ),
            (
                PipelineError::Transition(TransitionError {
                    state: SyncState::Uninitialized,
                    event: pstart_index::SyncEvent::Initialize,
                }),
                ErrorCode::Internal,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code(), expected, "{error}");
        }
        assert_eq!(
            PipelineError::Ntfs(NtfsError::UnsupportedPlatform).code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn checkpoint_is_usable_requires_journal_bounds() {
        let mut journal = journal(7, 100);
        let checkpoint = Checkpoint {
            journal_id: 7,
            next_usn: 100,
            index_generation: 0,
            committed_at: 0,
        };
        assert!(checkpoint_is_usable(&checkpoint, &journal));

        let mut ahead = checkpoint.clone();
        ahead.next_usn = 101;
        assert!(!checkpoint_is_usable(&ahead, &journal));

        let mut old = checkpoint.clone();
        old.next_usn = 9;
        journal.lowest_valid_usn = 10;
        assert!(!checkpoint_is_usable(&old, &journal));

        let mut wrong_journal = checkpoint.clone();
        wrong_journal.journal_id = 8;
        assert!(!checkpoint_is_usable(&wrong_journal, &journal));
    }
}
