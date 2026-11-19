//! Server-facing engine: lifecycle, query API and status.
//!
//! The engine wraps one volume pipeline. It is deliberately transport- and
//! service-agnostic so that the Windows Service and IPC layers can drive it
//! without duplicating indexing rules.

use std::path::PathBuf;

use pstart_core::limits::{MAX_QUERY_BYTES, MAX_QUERY_CHARS, MAX_RESULTS};
use pstart_core::{ErrorCode, FileItem};
use pstart_index::{SyncState, VolumeMeta};
use thiserror::Error;

use crate::pipeline::{
    CatchUpReport, IndexPipeline, PipelineError, ScanReport, StartupAction, VolumeSource,
    DEFAULT_BATCH_LIMIT,
};

/// Default journal batches consumed per tick.
pub const DEFAULT_CATCH_UP_BATCHES: usize = 16;

/// Search backend selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchBackend {
    /// Live in-memory index.
    #[default]
    Memory,
    /// SQLite snapshot plus mmap trigram candidates.
    Sqlite,
}

/// Engine configuration for one volume.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Volume metadata used by SQLite.
    pub volume: VolumeMeta,
    /// Display root, for example `C:\`.
    pub root: String,
    /// SQLite database path.
    pub database_path: PathBuf,
    /// Records per SQLite transaction.
    pub batch_limit: usize,
    /// Journal batches consumed per catch-up call.
    pub catch_up_batches: usize,
    /// Perform the initial scan incrementally from `tick`.
    pub defer_scan: bool,
    /// Search backend.
    pub search_backend: SearchBackend,
    /// Include Windows/Program Files/ProgramData trees in the index.
    pub include_system_directories: bool,
}

impl EngineConfig {
    /// Construct a configuration with default batch sizes.
    #[must_use]
    pub fn new(volume: VolumeMeta, root: impl Into<String>, database_path: PathBuf) -> Self {
        Self {
            volume,
            root: root.into(),
            database_path,
            batch_limit: DEFAULT_BATCH_LIMIT,
            catch_up_batches: DEFAULT_CATCH_UP_BATCHES,
            defer_scan: false,
            search_backend: SearchBackend::Memory,
            include_system_directories: false,
        }
    }
}

/// One search response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResponse {
    /// File results.
    pub items: Vec<FileItem>,
    /// Whether the search stopped at a candidate/scan budget.
    pub truncated: bool,
}

/// Drop items that cannot be represented within wire display limits.
fn displayable_items(hits: impl IntoIterator<Item = FileItem>) -> (Vec<FileItem>, bool) {
    let mut dropped = false;
    let items = hits
        .into_iter()
        .filter(|item| {
            if item.validate().is_ok() {
                true
            } else {
                dropped = true;
                false
            }
        })
        .collect();
    (items, dropped)
}

/// Engine failure.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Pipeline failure.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),

    /// Operation requires a running engine.
    #[error("engine is not running")]
    NotRunning,

    /// Start was called twice.
    #[error("engine is already running")]
    AlreadyRunning,

    /// Query failed validation.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
}

impl EngineError {
    /// Stable external error category.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Pipeline(error) => error.code(),
            Self::NotRunning | Self::AlreadyRunning => ErrorCode::ServerBusy,
            Self::InvalidQuery(_) => ErrorCode::InvalidRequest,
        }
    }
}

/// Snapshot of engine state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineStatus {
    /// Whether the engine is running.
    pub running: bool,
    /// Synchronization state.
    pub sync_state: SyncState,
    /// Number of in-memory records.
    pub indexed_files: usize,
    /// Last committed USN.
    pub committed_usn: u64,
    /// Index generation.
    pub index_generation: u64,
    /// Bound journal ID.
    pub journal_id: Option<u64>,
    /// Last error retained for diagnostics.
    pub last_error: Option<String>,
}

/// Result of a successful start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupReport {
    /// Startup decision.
    pub action: StartupAction,
    /// Scan report when a scan ran.
    pub scan: Option<ScanReport>,
    /// Catch-up report.
    pub catch_up: CatchUpReport,
    /// Whether a journal inconsistency forced a rebuild.
    pub rebuilt: bool,
}

/// Result of one tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Journal records applied.
    pub records: usize,
    /// Whether the tail was reached.
    pub caught_up: bool,
    /// Whether a rebuild ran during the tick.
    pub rebuilt: bool,
}

/// One-volume server engine.
pub struct ServerEngine<S: VolumeSource> {
    source: S,
    pipeline: IndexPipeline,
    config: EngineConfig,
    running: bool,
    last_error: Option<String>,
    scan_pending: bool,
}

impl<S: VolumeSource> ServerEngine<S> {
    /// Open the pipeline and source without starting indexing.
    pub fn open(config: EngineConfig, source: S) -> Result<Self, EngineError> {
        let mut pipeline = IndexPipeline::open(
            config.volume.clone(),
            config.root.clone(),
            &config.database_path,
            config.batch_limit,
        )?;
        pipeline.set_include_system_directories(config.include_system_directories);
        if config.search_backend == SearchBackend::Sqlite {
            pipeline.enable_sqlite_search()?;
        }
        Ok(Self {
            source,
            pipeline,
            config,
            running: false,
            last_error: None,
            scan_pending: false,
        })
    }

    /// Start indexing, scanning or catching up as required.
    pub fn start(&mut self) -> Result<StartupReport, EngineError> {
        let result = self.start_inner();
        if let Err(error) = &result {
            if !matches!(error, EngineError::AlreadyRunning) {
                self.last_error = Some(error.to_string());
            }
        }
        result
    }

    fn start_inner(&mut self) -> Result<StartupReport, EngineError> {
        if self.running {
            return Err(EngineError::AlreadyRunning);
        }
        self.scan_pending = false;
        self.pipeline.abort_scan();
        let action = self.pipeline.startup_action(&self.source)?;
        let mut report = StartupReport {
            action,
            ..StartupReport::default()
        };
        if action == StartupAction::Scan {
            if self.config.defer_scan {
                self.pipeline.begin_scan(&self.source)?;
                self.scan_pending = true;
                self.running = true;
                self.last_error = None;
                return Ok(report);
            }
            report.scan = Some(self.pipeline.initial_scan(&self.source)?);
        }
        let (catch_up, rebuilt_scan) = self.catch_up_or_rebuild()?;
        report.catch_up = catch_up;
        if let Some(scan) = rebuilt_scan {
            report.scan = Some(scan);
            report.rebuilt = true;
        }
        self.running = true;
        self.last_error = None;
        Ok(report)
    }

    /// Consume journal changes; rebuild automatically when required.
    pub fn tick(&mut self) -> Result<TickReport, EngineError> {
        if !self.running {
            return Err(EngineError::NotRunning);
        }
        if self.scan_pending {
            match self.pipeline.scan_step(&self.source, 256) {
                Ok(Some(_)) => {
                    self.scan_pending = false;
                    self.last_error = None;
                    return Ok(TickReport::default());
                }
                Ok(None) => return Ok(TickReport::default()),
                Err(error) => {
                    self.scan_pending = false;
                    self.running = false;
                    self.pipeline.abort_scan();
                    self.last_error = Some(error.to_string());
                    return Err(error.into());
                }
            }
        }
        let result = self.catch_up_or_rebuild();
        match result {
            Ok((catch_up, scan)) => {
                self.last_error = None;
                Ok(TickReport {
                    records: catch_up.records,
                    caught_up: catch_up.caught_up,
                    rebuilt: scan.is_some(),
                })
            }
            Err(error) => {
                self.running = false;
                self.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Search the in-memory index.
    pub fn search(&self, query: &str, limit: u32) -> Result<SearchResponse, EngineError> {
        if !self.running {
            return Err(EngineError::NotRunning);
        }
        let query = query.trim();
        if query.len() > MAX_QUERY_BYTES {
            return Err(EngineError::InvalidQuery(format!(
                "query exceeds {MAX_QUERY_BYTES} bytes"
            )));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(EngineError::InvalidQuery(format!(
                "query exceeds {MAX_QUERY_CHARS} characters"
            )));
        }
        let limit = limit.min(MAX_RESULTS);
        if query.is_empty() || limit == 0 {
            return Ok(SearchResponse::default());
        }
        let outcome = self.pipeline.search(query, limit);
        let (items, dropped) = displayable_items(outcome.hits.into_iter().map(|hit| hit.item));
        Ok(SearchResponse {
            items,
            truncated: outcome.truncated || dropped,
        })
    }

    /// Whether the engine is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether synchronization is active.
    #[must_use]
    pub fn is_syncing(&self) -> bool {
        !matches!(
            self.pipeline.state(),
            SyncState::LiveSync | SyncState::Stopped
        )
    }

    /// Whether the initial MFT scan is still in progress.
    #[must_use]
    pub fn is_initial_scan(&self) -> bool {
        self.running && self.pipeline.state() == SyncState::InitialScan
    }

    /// Current status snapshot.
    #[must_use]
    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.running,
            sync_state: self.pipeline.state(),
            indexed_files: self.pipeline.indexed_files(),
            committed_usn: self.pipeline.committed_usn(),
            index_generation: self.pipeline.index_generation(),
            journal_id: self.pipeline.journal_id(),
            last_error: self.last_error.clone(),
        }
    }

    /// Volume identifier for this engine.
    #[must_use]
    pub fn volume_id(&self) -> &str {
        self.pipeline.volume_id().as_str()
    }

    /// Persist stopped state and stop accepting work.
    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        if !self.running {
            return Ok(());
        }
        self.pipeline.mark_stopped()?;
        self.scan_pending = false;
        self.pipeline.abort_scan();
        self.running = false;
        Ok(())
    }

    fn catch_up_or_rebuild(&mut self) -> Result<(CatchUpReport, Option<ScanReport>), EngineError> {
        match self
            .pipeline
            .catch_up(&self.source, self.config.catch_up_batches)
        {
            Ok(report) => Ok((report, None)),
            Err(
                PipelineError::JournalChanged { .. }
                | PipelineError::JournalLost { .. }
                | PipelineError::JournalRewound { .. },
            ) => {
                let scan = self.pipeline.initial_scan(&self.source)?;
                let report = self
                    .pipeline
                    .catch_up(&self.source, self.config.catch_up_batches)?;
                Ok((report, Some(scan)))
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::QueryBackend;
    use pstart_core::VolumeId;
    use pstart_ntfs::{
        JournalBatch, JournalInfo, MftBatch, UsnRecord, USN_REASON_FILE_CREATE,
        USN_REASON_RENAME_NEW_NAME,
    };
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use tempfile::TempDir;

    #[test]
    fn sqlite_backend_serves_after_initial_scan() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("sqlite.db"));
        config.search_backend = SearchBackend::Sqlite;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        let response = engine.search("a.txt", 10).unwrap();
        assert!(response.items.iter().any(|item| item.name == "a.txt"));

        engine.source.journal.borrow_mut().next_usn = 101;
        engine.source.push_batch(JournalBatch {
            next_usn: 101,
            records: vec![record(20, 5, "delta.txt", 100, USN_REASON_FILE_CREATE)],
        });
        engine.tick().unwrap();
        let response = engine.search("delta.txt", 10).unwrap();
        assert!(response.items.iter().any(|item| item.name == "delta.txt"));
    }

    #[test]
    fn overlong_path_items_are_skipped() {
        let id = pstart_core::FileId::new(VolumeId::new("C:").unwrap(), pstart_core::Frn::new(1));
        let long = FileItem {
            id: id.clone(),
            name: "deep.txt".to_owned(),
            path: "x".repeat(pstart_core::MAX_PATH_UTF16 + 1),
            attributes: 0,
            is_dir: false,
        };
        let ok = FileItem {
            id,
            name: "ok.txt".to_owned(),
            path: "C:\\ok.txt".to_owned(),
            attributes: 0,
            is_dir: false,
        };
        let (items, dropped) = displayable_items([long, ok]);
        assert!(dropped);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "C:\\ok.txt");
    }

    #[test]
    fn search_marks_overlong_path_hits_truncated() {
        let mut records = Vec::new();
        for depth in 0..20 {
            let name = if depth == 19 {
                "target".to_owned()
            } else {
                "a".repeat(250)
            };
            records.push(record(
                11 + depth,
                if depth == 0 { 5 } else { 10 + depth },
                &name,
                1 + depth as i64,
                USN_REASON_FILE_CREATE,
            ));
        }
        let source = FakeSource::new(
            journal(7, 100),
            vec![(
                0,
                MftBatch {
                    next_start_frn: 0,
                    records,
                },
            )],
        );
        let dir = TempDir::new().unwrap();
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        let response = engine.search("target", 10).unwrap();
        assert!(response.items.is_empty());
        assert!(response.truncated);
    }

    struct FakeSource {
        journal: RefCell<JournalInfo>,
        pages: Vec<(u64, MftBatch)>,
        batches: RefCell<VecDeque<JournalBatch>>,
        fail_read: RefCell<bool>,
    }

    impl FakeSource {
        fn new(journal: JournalInfo, pages: Vec<(u64, MftBatch)>) -> Self {
            Self {
                journal: RefCell::new(journal),
                pages,
                batches: RefCell::new(VecDeque::new()),
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
                return Err(PipelineError::Source("injected read failure".to_owned()));
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

    fn record(frn: u64, parent: u64, name: &str, usn: i64, reason: u32) -> UsnRecord {
        UsnRecord {
            frn,
            parent_frn: parent,
            usn,
            timestamp: 0,
            reason,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: name.into(),
        }
    }

    fn pages() -> Vec<(u64, MftBatch)> {
        vec![
            (
                0,
                MftBatch {
                    next_start_frn: 50,
                    records: vec![
                        UsnRecord {
                            frn: 11,
                            parent_frn: 5,
                            usn: 1,
                            timestamp: 0,
                            reason: 0,
                            source_info: 0,
                            security_id: 0,
                            attributes: 0x10,
                            name: "Users".to_owned(),
                        },
                        record(12, 11, "a.txt", 2, USN_REASON_FILE_CREATE),
                    ],
                },
            ),
            (
                50,
                MftBatch {
                    next_start_frn: 0,
                    records: vec![record(13, 11, "b.txt", 3, USN_REASON_FILE_CREATE)],
                },
            ),
        ]
    }

    fn engine(dir: &TempDir, source: FakeSource) -> ServerEngine<FakeSource> {
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("engine.db"));
        ServerEngine::open(config, source).unwrap()
    }

    fn many_pages(count: u64) -> Vec<(u64, MftBatch)> {
        (0..count)
            .map(|index| {
                (
                    index * 10,
                    MftBatch {
                        next_start_frn: if index + 1 == count {
                            0
                        } else {
                            (index + 1) * 10
                        },
                        records: Vec::new(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn start_searches_and_reports_status() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        assert_eq!(engine.tick().unwrap_err().code(), ErrorCode::ServerBusy);
        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::Scan);
        assert_eq!(report.scan.unwrap().records, 3);
        assert!(!report.rebuilt);
        assert_eq!(engine.status().indexed_files, 3);
        assert_eq!(engine.status().sync_state, SyncState::LiveSync);
        assert_eq!(engine.volume_id(), "C:");
        assert_eq!(engine.search("a.txt", 10).unwrap().items.len(), 1);
    }

    #[test]
    fn deferred_scan_serves_partial_results() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("deferred.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::Scan);
        assert!(report.scan.is_none());
        assert_eq!(engine.status().indexed_files, 0);
        assert_eq!(engine.status().sync_state, SyncState::InitialScan);

        engine.tick().unwrap();
        assert_eq!(engine.status().indexed_files, 3);
        assert_eq!(engine.search("a.txt", 10).unwrap().items.len(), 1);
        assert_eq!(engine.status().sync_state, SyncState::CatchUp);
    }

    #[test]
    fn start_failure_records_last_error() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(
            JournalInfo {
                journal_id: 0,
                ..journal(7, 100)
            },
            pages(),
        );
        let mut engine = engine(&dir, source);
        assert!(engine.start().is_err());
        assert!(engine.status().last_error.is_some());

        *engine.source.journal.borrow_mut() = journal(7, 100);
        assert!(engine.start().is_ok());
        assert!(engine.status().last_error.is_none());
    }

    #[test]
    fn failed_deferred_scan_restarts_with_scan() {
        let dir = TempDir::new().unwrap();
        let mut broken = pages();
        broken[0].1.next_start_frn = 999;
        let source = FakeSource::new(journal(7, 100), broken);
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("failed-deferred.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        assert!(engine.tick().is_err());
        assert!(!engine.status().running);
        engine.shutdown().unwrap();

        let source = FakeSource::new(journal(7, 100), pages());
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("failed-deferred.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::Scan);
        engine.tick().unwrap();
        assert!(engine.status().running);
        assert_eq!(engine.status().indexed_files, 3);
    }

    #[test]
    fn shutdown_during_deferred_scan_allows_restart() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("deferred-stop.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        engine.shutdown().unwrap();
        assert!(!engine.status().running);

        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::Scan);
        assert!(engine.status().running);
        engine.tick().unwrap();
        assert_eq!(engine.status().indexed_files, 3);
        assert_eq!(engine.status().sync_state, SyncState::CatchUp);
    }

    #[test]
    fn shutdown_after_partial_scan_restarts_with_scan() {
        let dir = TempDir::new().unwrap();
        let pages = (0..300_u64)
            .map(|index| {
                (
                    index * 10,
                    MftBatch {
                        next_start_frn: if index == 299 { 0 } else { (index + 1) * 10 },
                        records: vec![record(
                            10 + index,
                            5,
                            &format!("f{index}"),
                            i64::try_from(index + 1).unwrap(),
                            USN_REASON_FILE_CREATE,
                        )],
                    },
                )
            })
            .collect();
        let source = FakeSource::new(journal(7, 10_000), pages);
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("partial-stop.db"));
        config.defer_scan = true;
        config.batch_limit = 1;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        engine.tick().unwrap();
        assert_eq!(engine.status().sync_state, SyncState::InitialScan);
        engine.shutdown().unwrap();

        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::Scan);
    }

    #[test]
    fn deferred_scan_progresses_across_ticks() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), many_pages(300));
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("deferred-many.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        engine.tick().unwrap();
        assert_eq!(engine.status().sync_state, SyncState::InitialScan);
        engine.tick().unwrap();
        assert_eq!(engine.status().sync_state, SyncState::CatchUp);
        engine.source.push_batch(JournalBatch {
            next_usn: 100,
            records: Vec::new(),
        });
        assert!(engine.tick().unwrap().caught_up);
        assert_eq!(engine.status().sync_state, SyncState::LiveSync);
    }

    #[test]
    fn deferred_scan_failure_stops_engine() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), Vec::new());
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("deferred-fail.db"));
        config.defer_scan = true;
        let mut engine = ServerEngine::open(config, source).unwrap();
        engine.start().unwrap();
        assert!(engine.tick().is_err());
        assert!(!engine.status().running);
        assert_eq!(engine.status().indexed_files, 0);
    }

    #[test]
    fn tick_applies_journal_changes() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        engine.source.push_batch(JournalBatch {
            next_usn: 105,
            records: vec![record(
                12,
                11,
                "renamed.txt",
                104,
                USN_REASON_RENAME_NEW_NAME,
            )],
        });
        engine.source.push_batch(JournalBatch {
            next_usn: 105,
            records: Vec::new(),
        });
        *engine.source.journal.borrow_mut() = journal(7, 105);
        let report = engine.tick().unwrap();
        assert_eq!(report.records, 1);
        assert!(report.caught_up);
        assert!(!report.rebuilt);
        assert_eq!(engine.search("renamed", 10).unwrap().items.len(), 1);
        assert!(engine.search("a.txt", 10).unwrap().items.is_empty());
    }

    #[test]
    fn journal_change_triggers_rebuild() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        let generation = engine.status().index_generation;
        *engine.source.journal.borrow_mut() = journal(8, 100);
        let report = engine.tick().unwrap();
        assert!(report.rebuilt);
        assert_eq!(engine.status().index_generation, generation + 1);
        assert_eq!(engine.status().indexed_files, 3);
    }

    #[test]
    fn tick_failure_does_not_serve_partial_results() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        *engine.source.journal.borrow_mut() = journal(7, 105);
        engine.source.push_batch(JournalBatch {
            next_usn: 105,
            records: vec![
                record(14, 11, "partial.txt", 104, USN_REASON_FILE_CREATE),
                record(15, 11, "", 105, USN_REASON_FILE_CREATE),
            ],
        });
        assert!(engine.tick().is_err());
        assert!(!engine.status().running);
        assert!(matches!(
            engine.search("partial", 10),
            Err(EngineError::NotRunning)
        ));
    }

    #[test]
    fn journal_rewind_triggers_rebuild() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        let generation = engine.status().index_generation;
        *engine.source.journal.borrow_mut() = journal(7, 80);
        let report = engine.tick().unwrap();
        assert!(report.rebuilt);
        assert_eq!(engine.status().index_generation, generation + 1);
    }

    #[test]
    fn shutdown_and_restart_resume_catchup() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        engine.shutdown().unwrap();
        assert_eq!(engine.status().sync_state, SyncState::Stopped);
        assert!(matches!(
            engine.search("a.txt", 10),
            Err(EngineError::NotRunning)
        ));
        let report = engine.start().unwrap();
        assert_eq!(report.action, StartupAction::CatchUp);
        assert!(report.scan.is_none());
        assert_eq!(engine.status().sync_state, SyncState::LiveSync);
    }

    #[test]
    fn duplicate_start_and_query_validation() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        assert!(matches!(engine.start(), Err(EngineError::AlreadyRunning)));
        assert!(engine.search("", 10).unwrap().items.is_empty());
        assert!(engine.search("x", 0).unwrap().items.is_empty());
        assert_eq!(
            engine.search("no-such-file", u32::MAX).unwrap().items.len(),
            0
        );
        assert!(matches!(
            engine.search(&"x".repeat(MAX_QUERY_CHARS + 1), 10),
            Err(EngineError::InvalidQuery(_))
        ));
        assert!(matches!(
            engine.search(&"a".repeat(MAX_QUERY_BYTES + 1), 10),
            Err(EngineError::InvalidQuery(_))
        ));
    }

    #[test]
    fn error_codes_and_open_failure() {
        assert_eq!(
            EngineError::Pipeline(PipelineError::InvalidState("x".to_owned())).code(),
            ErrorCode::Internal
        );
        assert_eq!(EngineError::NotRunning.code(), ErrorCode::ServerBusy);
        assert_eq!(EngineError::AlreadyRunning.code(), ErrorCode::ServerBusy);
        assert_eq!(
            EngineError::InvalidQuery("x".to_owned()).code(),
            ErrorCode::InvalidRequest
        );

        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let config = EngineConfig::new(
            meta(),
            "C:\\",
            dir.path().join("missing-subdir").join("engine.db"),
        );
        assert!(ServerEngine::open(config, source).is_err());
    }

    #[test]
    fn shutdown_before_start_and_tick_failure() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.shutdown().unwrap();
        assert!(matches!(engine.tick(), Err(EngineError::NotRunning)));

        engine.start().unwrap();
        *engine.source.fail_read.borrow_mut() = true;
        assert!(matches!(engine.tick(), Err(EngineError::Pipeline(_))));
        assert!(engine.status().last_error.is_some());
    }

    #[test]
    fn engine_implements_query_backend() {
        let dir = TempDir::new().unwrap();
        let source = FakeSource::new(journal(7, 100), pages());
        let mut engine = engine(&dir, source);
        engine.start().unwrap();
        let response = QueryBackend::search(&engine, "a.txt", 10).unwrap();
        assert_eq!(response.items.len(), 1);
        let status = QueryBackend::status(&engine).unwrap();
        assert_eq!(status.volume_id, "C:");
        assert_eq!(status.indexed_files, 3);
        assert!(!status.is_syncing);
    }

    struct SwitchingSource {
        inner: FakeSource,
        calls: std::cell::Cell<usize>,
    }

    impl VolumeSource for SwitchingSource {
        fn journal(&self) -> Result<JournalInfo, PipelineError> {
            let count = self.calls.get() + 1;
            self.calls.set(count);
            let mut journal = self.inner.journal()?;
            if count >= 3 {
                journal.journal_id = 8;
            }
            Ok(journal)
        }

        fn mft_page(
            &self,
            start_frn: u64,
            journal: &JournalInfo,
        ) -> Result<MftBatch, PipelineError> {
            self.inner.mft_page(start_frn, journal)
        }

        fn read_journal(
            &self,
            start_usn: i64,
            journal: &JournalInfo,
            timeout_ms: u32,
        ) -> Result<JournalBatch, PipelineError> {
            self.inner.read_journal(start_usn, journal, timeout_ms)
        }
    }

    #[test]
    fn start_rebuilds_when_journal_changes_during_startup() {
        let dir = TempDir::new().unwrap();
        let source = SwitchingSource {
            inner: FakeSource::new(journal(7, 100), pages()),
            calls: std::cell::Cell::new(0),
        };
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("switch.db"));
        let mut engine = ServerEngine::open(config, source).unwrap();
        let report = engine.start().unwrap();
        assert!(report.rebuilt);
        assert_eq!(report.scan.unwrap().records, 3);
        assert_eq!(engine.status().journal_id, Some(8));
        assert_eq!(engine.status().index_generation, 2);
    }
}
