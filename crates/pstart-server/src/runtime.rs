//! Resident server runtime: journal worker plus AF_UNIX query listener.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pstart_core::limits::MAX_IPC_CONNECTIONS;
use pstart_core::ErrorCode;
use pstart_ipc::frame::encode_payload;
use pstart_ipc::{IpcError, IpcMessage, Transport, UnixClient, UnixListener};

use crate::engine::{EngineConfig, EngineError, EngineStatus, SearchResponse, ServerEngine};
use crate::pipeline::{worker_poll_delay, VolumeSource};
use crate::service::{
    serve_connection_with_global_limit, BackendStatus, ConnectionLimits, QueryBackend,
    SharedQueryLimit,
};

/// Default concurrent connection limit.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// Maximum time shutdown waits for live connection threads.
pub const CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Runtime tuning.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// AF_UNIX socket path.
    pub socket_path: PathBuf,
    /// Journal polling interval.
    pub poll_interval: Duration,
    /// Maximum concurrent query connections.
    pub max_connections: usize,
    /// Per-connection query rate limits.
    pub query_limits: ConnectionLimits,
    /// Optional runtime-wide query rate limit shared by all connections.
    pub global_query_limits: Option<ConnectionLimits>,
}

/// Runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Engine startup or query failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Socket setup failed.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// Internal lock was poisoned.
    #[error("runtime lock poisoned")]
    LockPoisoned,
    /// Poll interval must be positive.
    #[error("poll interval must be positive")]
    InvalidPollInterval,
    /// Connection limit must be in 1..=64.
    #[error("max_connections must be in 1..=64")]
    InvalidConnectionLimit,
    /// Per-connection query limits are invalid.
    #[error("query limits must have a positive window and query count")]
    InvalidQueryLimits,
    /// A runtime thread panicked.
    #[error("runtime thread panicked")]
    ThreadPanicked,
}

impl RuntimeError {
    /// Stable external category.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Engine(error) => error.code(),
            Self::Ipc(error) => error.code(),
            Self::LockPoisoned | Self::ThreadPanicked => ErrorCode::Internal,
            Self::InvalidPollInterval | Self::InvalidConnectionLimit | Self::InvalidQueryLimits => {
                ErrorCode::InvalidRequest
            }
        }
    }
}

struct RuntimeBackend<S: VolumeSource> {
    engine: Arc<Mutex<ServerEngine<S>>>,
    starting: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    volume_id: String,
    indexed_files: Arc<AtomicU64>,
}

impl<S: VolumeSource> Clone for RuntimeBackend<S> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            starting: Arc::clone(&self.starting),
            failed: Arc::clone(&self.failed),
            volume_id: self.volume_id.clone(),
            indexed_files: Arc::clone(&self.indexed_files),
        }
    }
}

struct ActiveConnection(Arc<AtomicUsize>);

/// Acquire one connection slot without exceeding `max_connections`.
fn try_acquire_connection(active: &AtomicUsize, max_connections: usize) -> bool {
    crate::admission::try_acquire(active, max_connections)
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ConnectionThread {
    handle: JoinHandle<()>,
}

fn drain_connection_threads(connections: &Mutex<Vec<ConnectionThread>>) {
    let handles = connections
        .lock()
        .map(|mut slots| std::mem::take(&mut *slots))
        .unwrap_or_default();
    let deadline = Instant::now() + CONNECTION_SHUTDOWN_GRACE;
    for entry in handles {
        while !entry.handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if entry.handle.is_finished() {
            let _ = entry.handle.join();
        }
        // Otherwise detach; the process/engine teardown ends the thread.
    }
}

impl<S: VolumeSource> QueryBackend for RuntimeBackend<S> {
    fn search(&self, query: &str, limit: u32) -> Result<SearchResponse, ErrorCode> {
        if self.starting.load(Ordering::Acquire) {
            return Err(ErrorCode::ServerBusy);
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(ErrorCode::Internal);
        }
        let engine = self.engine.lock().map_err(|_| ErrorCode::Internal)?;
        engine.search(query, limit).map_err(|error| error.code())
    }

    fn status(&self) -> Result<BackendStatus, ErrorCode> {
        if self.starting.load(Ordering::Acquire) {
            return Ok(BackendStatus {
                volume_id: self.volume_id.clone(),
                indexed_files: 0,
                is_syncing: true,
            });
        }
        if self.failed.load(Ordering::Acquire) {
            return Ok(BackendStatus {
                volume_id: self.volume_id.clone(),
                indexed_files: 0,
                is_syncing: false,
            });
        }
        let engine = self.engine.lock().map_err(|_| ErrorCode::Internal)?;
        let is_syncing = engine.is_syncing();
        Ok(BackendStatus {
            volume_id: self.volume_id.clone(),
            indexed_files: self.indexed_files.load(Ordering::Relaxed),
            is_syncing,
        })
    }
}

/// Resident server.
pub struct ServerRuntime<S: VolumeSource + Send + 'static> {
    engine: Arc<Mutex<ServerEngine<S>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    listener: Option<JoinHandle<()>>,
    counter: Option<JoinHandle<()>>,
    connections: Arc<Mutex<Vec<ConnectionThread>>>,
    socket_path: PathBuf,
}

impl<S: VolumeSource + Send + 'static> ServerRuntime<S> {
    /// Start indexing, the journal worker, and the AF_UNIX listener.
    pub fn start(
        engine_config: EngineConfig,
        source: S,
        runtime_config: RuntimeConfig,
    ) -> Result<Self, RuntimeError> {
        if runtime_config.poll_interval.is_zero() {
            return Err(RuntimeError::InvalidPollInterval);
        }
        if !(1..=MAX_IPC_CONNECTIONS).contains(&runtime_config.max_connections) {
            return Err(RuntimeError::InvalidConnectionLimit);
        }
        if !runtime_config.query_limits.is_valid() {
            return Err(RuntimeError::InvalidQueryLimits);
        }
        let max_connections = runtime_config.max_connections;
        let query_limits = runtime_config.query_limits;
        let global_limit = match runtime_config.global_query_limits {
            Some(limits) => {
                Some(SharedQueryLimit::new(limits).map_err(|_| RuntimeError::InvalidQueryLimits)?)
            }
            None => None,
        };
        let count_database = engine_config.database_path.clone();
        let count_volume = engine_config.volume.volume.as_str().to_owned();
        let indexed_files = Arc::new(AtomicU64::new(0));
        let engine = ServerEngine::open(engine_config, source)?;
        let volume_id = engine.volume_id().to_owned();
        let engine = Arc::new(Mutex::new(engine));
        let listener = UnixListener::bind(&runtime_config.socket_path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let starting = Arc::new(AtomicBool::new(true));
        let failed = Arc::new(AtomicBool::new(false));
        let startup_done = Arc::new(AtomicBool::new(false));

        let worker_engine = Arc::clone(&engine);
        let worker_stop = Arc::clone(&stop);
        let worker_failed = Arc::clone(&failed);
        let worker_startup = Arc::clone(&startup_done);
        let poll_interval = runtime_config.poll_interval;
        let worker = thread::spawn(move || {
            let mut retry_delay = Duration::from_secs(1);
            while !worker_stop.load(Ordering::Acquire) {
                let scanning = {
                    let engine = worker_engine
                        .lock()
                        .expect("runtime engine lock must not be poisoned");
                    engine.is_initial_scan()
                };
                thread::sleep(worker_poll_delay(poll_interval, scanning));
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                if !worker_startup.load(Ordering::Acquire) {
                    continue;
                }
                let outcome = {
                    let mut engine = worker_engine
                        .lock()
                        .expect("runtime engine lock must not be poisoned");
                    if engine.is_running() {
                        engine.tick().map(|_| ())
                    } else {
                        engine.start().map(|_| ())
                    }
                };
                match outcome {
                    Ok(()) => {
                        worker_failed.store(false, Ordering::Release);
                        retry_delay = Duration::from_secs(1);
                    }
                    Err(_) => {
                        worker_failed.store(true, Ordering::Release);
                        thread::sleep(retry_delay);
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        });

        let listener_stop = Arc::clone(&stop);
        let active = Arc::new(AtomicUsize::new(0));
        let connections: Arc<Mutex<Vec<ConnectionThread>>> = Arc::new(Mutex::new(Vec::new()));
        let listener_connections = Arc::clone(&connections);
        let backend = RuntimeBackend {
            engine: Arc::clone(&engine),
            starting: Arc::clone(&starting),
            failed: Arc::clone(&failed),
            volume_id,
            indexed_files: Arc::clone(&indexed_files),
        };
        let listener_thread = thread::spawn(move || {
            while !listener_stop.load(Ordering::Acquire) {
                let mut transport = listener.accept().expect("listener accept");
                if listener_stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = transport
                    .stream_mut()
                    .set_read_timeout(Duration::from_secs(30));
                if !try_acquire_connection(&active, max_connections) {
                    let _ = transport
                        .stream_mut()
                        .set_read_timeout(Duration::from_millis(200));
                    if let Ok(payload) = encode_payload(&IpcMessage::Error {
                        request_id: None,
                        code: ErrorCode::ServerBusy.as_u32(),
                    }) {
                        let _ = transport.send_frame(&payload);
                    }
                    // Consume an already-sent Hello so close is graceful.
                    let _ = transport.receive_frame();
                    let _ = transport.close();
                    continue;
                }
                let active_guard = ActiveConnection(Arc::clone(&active));
                let backend = backend.clone();
                let global = global_limit.clone();
                let handle = thread::spawn(move || {
                    let _guard = active_guard;
                    let mut transport = transport;
                    let _ = serve_connection_with_global_limit(
                        &mut transport,
                        &backend,
                        1,
                        env!("CARGO_PKG_VERSION"),
                        query_limits,
                        global.as_ref(),
                    );
                });
                let mut slots = listener_connections
                    .lock()
                    .expect("connection registry is not poisoned");
                slots.retain(|entry| !entry.handle.is_finished());
                slots.push(ConnectionThread { handle });
            }
        });

        let started = engine
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)?
            .start();
        if let Err(error) = started {
            stop.store(true, Ordering::Release);
            let _ = UnixClient::connect(&runtime_config.socket_path);
            let _ = listener_thread.join();
            drain_connection_threads(&connections);
            let _ = worker.join();
            let _ = std::fs::remove_file(&runtime_config.socket_path);
            return Err(RuntimeError::Engine(error));
        }
        starting.store(false, Ordering::Release);
        failed.store(false, Ordering::Release);
        startup_done.store(true, Ordering::Release);
        let initial_count = engine
            .lock()
            .map_err(|_| RuntimeError::LockPoisoned)?
            .status()
            .indexed_files as u64;
        indexed_files.store(initial_count, Ordering::Release);
        let counter_stop = Arc::clone(&stop);
        let counter_files = Arc::clone(&indexed_files);
        let counter = thread::spawn(move || {
            while !counter_stop.load(Ordering::Acquire) {
                for _ in 0..10 {
                    if counter_stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if let Ok(count) = pstart_index::count_records(&count_database, &count_volume) {
                    counter_files.store(count, Ordering::Release);
                }
            }
        });

        Ok(Self {
            engine,
            stop,
            worker: Some(worker),
            listener: Some(listener_thread),
            counter: Some(counter),
            connections,
            socket_path: runtime_config.socket_path,
        })
    }

    /// Search the current in-memory index.
    pub fn search(&self, query: &str, limit: u32) -> Result<SearchResponse, RuntimeError> {
        let engine = self.engine.lock().map_err(|_| RuntimeError::LockPoisoned)?;
        engine.search(query, limit).map_err(RuntimeError::Engine)
    }

    /// Snapshot engine status.
    pub fn status(&self) -> Result<EngineStatus, RuntimeError> {
        let engine = self.engine.lock().map_err(|_| RuntimeError::LockPoisoned)?;
        Ok(engine.status())
    }

    /// Stop the worker and listener, then join both threads.
    pub fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), RuntimeError> {
        self.stop.store(true, Ordering::Release);
        // Wake a blocking accept; the connection is dropped without a session.
        let _ = UnixClient::connect(&self.socket_path);
        if let Some(handle) = self.listener.take() {
            handle.join().map_err(|_| RuntimeError::ThreadPanicked)?;
        }
        if let Some(handle) = self.counter.take() {
            handle.join().map_err(|_| RuntimeError::ThreadPanicked)?;
        }
        drain_connection_threads(&self.connections);
        if let Some(handle) = self.worker.take() {
            handle.join().map_err(|_| RuntimeError::ThreadPanicked)?;
        }
        Ok(())
    }
}

impl<S: VolumeSource + Send + 'static> Drop for ServerRuntime<S> {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::worker_poll_delay;
    use pstart_core::VolumeId;
    use pstart_index::VolumeMeta;

    #[test]
    fn scan_poll_delay_is_bounded_by_configured_interval() {
        assert_eq!(
            worker_poll_delay(Duration::from_millis(500), true),
            Duration::from_millis(10)
        );
        assert_eq!(
            worker_poll_delay(Duration::from_millis(500), false),
            Duration::from_millis(500)
        );
        assert_eq!(
            worker_poll_delay(Duration::from_millis(5), true),
            Duration::from_millis(5)
        );
    }
    use pstart_ipc::{IpcMessage, UnixClient};
    use pstart_ntfs::{JournalBatch, JournalInfo, MftBatch, UsnRecord, USN_REASON_FILE_CREATE};
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct SharedSource(Arc<Mutex<SourceState>>);

    struct SourceState {
        journal: JournalInfo,
        pages: Vec<(u64, MftBatch)>,
        batches: VecDeque<JournalBatch>,
        fail_next_page: bool,
    }

    impl SharedSource {
        fn new(journal: JournalInfo, pages: Vec<(u64, MftBatch)>) -> Self {
            Self(Arc::new(Mutex::new(SourceState {
                journal,
                pages,
                batches: VecDeque::new(),
                fail_next_page: false,
            })))
        }

        fn fail_next_page(&self) {
            self.0.lock().unwrap().fail_next_page = true;
        }

        fn push_batch(&self, batch: JournalBatch) {
            self.0.lock().unwrap().batches.push_back(batch);
        }

        fn set_journal(&self, journal: JournalInfo) {
            self.0.lock().unwrap().journal = journal;
        }
    }

    impl VolumeSource for SharedSource {
        fn journal(&self) -> Result<JournalInfo, crate::pipeline::PipelineError> {
            self.0
                .lock()
                .map(|state| state.journal.clone())
                .map_err(|_| crate::pipeline::PipelineError::Source("lock poisoned".to_owned()))
        }

        fn mft_page(
            &self,
            start_frn: u64,
            _journal: &JournalInfo,
        ) -> Result<MftBatch, crate::pipeline::PipelineError> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| crate::pipeline::PipelineError::Source("lock poisoned".to_owned()))?;
            if state.fail_next_page {
                state.fail_next_page = false;
                return Err(crate::pipeline::PipelineError::Source(
                    "injected page failure".to_owned(),
                ));
            }
            state
                .pages
                .iter()
                .find(|(start, _)| *start == start_frn)
                .map(|(_, page)| page.clone())
                .ok_or_else(|| {
                    crate::pipeline::PipelineError::Source(format!("no page for FRN {start_frn}"))
                })
        }

        fn read_journal(
            &self,
            _start_usn: i64,
            journal: &JournalInfo,
            _timeout_ms: u32,
        ) -> Result<JournalBatch, crate::pipeline::PipelineError> {
            let mut state = self
                .0
                .lock()
                .map_err(|_| crate::pipeline::PipelineError::Source("lock poisoned".to_owned()))?;
            Ok(state.batches.pop_front().unwrap_or(JournalBatch {
                next_usn: journal.next_usn,
                records: Vec::new(),
            }))
        }
    }

    fn meta() -> VolumeMeta {
        VolumeMeta::new(VolumeId::new("C:").unwrap(), "System", "NTFS").unwrap()
    }

    fn journal(next_usn: i64) -> JournalInfo {
        JournalInfo {
            journal_id: 7,
            first_usn: 10,
            next_usn,
            lowest_valid_usn: 0,
            max_usn: 10_000,
            maximum_size: 1024 * 1024,
            allocation_delta: 4096,
        }
    }

    fn record(frn: u64, parent: u64, name: &str, usn: i64) -> UsnRecord {
        UsnRecord {
            frn,
            parent_frn: parent,
            usn,
            timestamp: 0,
            reason: USN_REASON_FILE_CREATE,
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
                        record(12, 11, "a.txt", 2),
                    ],
                },
            ),
            (
                50,
                MftBatch {
                    next_start_frn: 0,
                    records: vec![record(13, 11, "b.txt", 3)],
                },
            ),
        ]
    }

    fn start_runtime(
        dir: &TempDir,
        source: SharedSource,
        poll_interval: Duration,
    ) -> (ServerRuntime<SharedSource>, PathBuf) {
        let socket = dir.path().join("pstart-runtime.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("runtime.db"));
        let runtime = ServerRuntime::start(
            config,
            source,
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval,
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .unwrap();
        (runtime, socket)
    }

    fn connect(socket: &Path) -> UnixClient {
        UnixClient::connect(socket).expect("runtime socket connect")
    }

    fn handshake(client: &mut UnixClient) {
        client
            .send(&IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::HelloAck { .. }
        ));
    }

    #[test]
    fn starting_backend_rejects_queries() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("starting.db"));
        let engine = ServerEngine::open(config, SharedSource::new(journal(100), pages())).unwrap();
        let backend = RuntimeBackend {
            engine: Arc::new(Mutex::new(engine)),
            starting: Arc::new(AtomicBool::new(true)),
            failed: Arc::new(AtomicBool::new(false)),
            volume_id: "C:".to_owned(),
            indexed_files: Arc::new(AtomicU64::new(0)),
        };
        assert_eq!(
            QueryBackend::search(&backend, "a", 10).unwrap_err(),
            ErrorCode::ServerBusy
        );
        let status = QueryBackend::status(&backend).unwrap();
        assert_eq!(status.volume_id, "C:");
        assert_eq!(status.indexed_files, 0);
        assert!(status.is_syncing);
    }

    #[test]
    fn startup_failure_cleans_listener_and_socket() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("startup-failure.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("failure.db"));
        let mut bad = journal(100);
        bad.next_usn = -1;
        let result = ServerRuntime::start(
            config,
            SharedSource::new(bad, pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        );
        assert!(result.is_err());
        assert!(!socket.exists());
    }

    #[test]
    fn failed_backend_hides_partial_results() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("failed.db"));
        let engine = ServerEngine::open(config, SharedSource::new(journal(100), pages())).unwrap();
        let backend = RuntimeBackend {
            engine: Arc::new(Mutex::new(engine)),
            starting: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(true)),
            volume_id: "C:".to_owned(),
            indexed_files: Arc::new(AtomicU64::new(0)),
        };
        assert_eq!(
            QueryBackend::search(&backend, "a", 10).unwrap_err(),
            ErrorCode::Internal
        );
        let status = QueryBackend::status(&backend).unwrap();
        assert_eq!(status.volume_id, "C:");
        assert_eq!(status.indexed_files, 0);
        assert!(!status.is_syncing);
    }

    #[test]
    fn status_before_start_reports_syncing() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("status.db"));
        let engine = ServerEngine::open(config, SharedSource::new(journal(100), pages())).unwrap();
        let backend = RuntimeBackend {
            engine: Arc::new(Mutex::new(engine)),
            starting: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            volume_id: "C:".to_owned(),
            indexed_files: Arc::new(AtomicU64::new(0)),
        };
        let status = QueryBackend::status(&backend).unwrap();
        assert_eq!(status.volume_id, "C:");
        assert_eq!(status.indexed_files, 0);
        assert!(status.is_syncing);
    }

    #[test]
    fn serves_query_and_shuts_down() {
        let dir = TempDir::new().unwrap();
        let (runtime, socket) = start_runtime(
            &dir,
            SharedSource::new(journal(100), pages()),
            Duration::from_millis(10),
        );
        assert_eq!(runtime.status().unwrap().indexed_files, 3);
        assert_eq!(runtime.search("a.txt", 10).unwrap().items.len(), 1);

        let mut client = connect(&socket);
        handshake(&mut client);
        client
            .send(&IpcMessage::Query {
                request_id: 9,
                text: "a.txt".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::QueryResult {
                request_id: 9,
                items,
                ..
            } if items.len() == 1
        ));
        client
            .send(&IpcMessage::StatusRequest { request_id: 10 })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::StatusResult {
                request_id: 10,
                volume_id,
                indexed_files: 3,
                is_syncing: false,
            } if volume_id == "C:"
        ));
        drop(client);

        runtime.shutdown().unwrap();
        assert!(!socket.exists());
    }

    #[test]
    fn worker_retries_after_scan_failure() {
        let dir = TempDir::new().unwrap();
        let source = SharedSource::new(journal(100), pages());
        source.fail_next_page();
        let socket = dir.path().join("retry.sock");
        let mut config = EngineConfig::new(meta(), "C:\\", dir.path().join("retry.db"));
        config.defer_scan = true;
        let runtime = ServerRuntime::start(
            config,
            source,
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(20),
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_failure = false;
        let mut recovered = false;
        while !recovered && Instant::now() < deadline {
            let mut client = connect(&socket);
            handshake(&mut client);
            client
                .send(&IpcMessage::StatusRequest { request_id: 1 })
                .unwrap();
            if let IpcMessage::StatusResult {
                indexed_files,
                is_syncing,
                ..
            } = client.receive().unwrap()
            {
                if !is_syncing && indexed_files == 0 {
                    saw_failure = true;
                }
                if !is_syncing && indexed_files == 3 {
                    recovered = true;
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
        runtime.shutdown().unwrap();
        assert!(saw_failure, "worker did not expose the failed scan");
        assert!(recovered, "worker did not retry the scan");
    }

    #[test]
    fn drain_connection_threads_joins_finished_workers() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = thread::spawn(move || flag.store(true, Ordering::Release));
        let connections = Mutex::new(vec![ConnectionThread { handle }]);
        drain_connection_threads(&connections);
        assert!(done.load(Ordering::Acquire));
        assert!(connections.lock().unwrap().is_empty());
    }

    #[test]
    fn connection_slots_are_atomic() {
        use std::sync::Barrier;

        let active = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(32));
        let workers: Vec<_> = (0..32)
            .map(|_| {
                let active = Arc::clone(&active);
                let successes = Arc::clone(&successes);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    if try_acquire_connection(&active, 4) {
                        successes.fetch_add(1, Ordering::AcqRel);
                        thread::sleep(Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::AcqRel);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(successes.load(Ordering::Acquire), 4);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn connection_limit_rejects_extra_clients() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("limit.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("limit.db"));
        let runtime = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 1,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .unwrap();
        let mut first = connect(&socket);
        handshake(&mut first);

        let mut second = connect(&socket);
        second
            .send(&IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            second.receive().unwrap(),
            IpcMessage::Error {
                request_id: None,
                code,
            } if code == ErrorCode::ServerBusy.as_u32()
        ));
        drop(second);
        drop(first);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn concurrent_clients_are_served() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("concurrent.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("concurrent.db"));
        let runtime = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 2,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .unwrap();
        let mut first = connect(&socket);
        handshake(&mut first);
        let mut second = connect(&socket);
        handshake(&mut second);
        for (client, request_id) in [(&mut first, 1), (&mut second, 2)] {
            client
                .send(&IpcMessage::Query {
                    request_id,
                    text: "a.txt".to_owned(),
                    limit: 10,
                })
                .unwrap();
            assert!(matches!(
                client.receive().unwrap(),
                IpcMessage::QueryResult { request_id: id, .. } if id == request_id
            ));
        }
        drop(first);
        drop(second);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_is_bounded_with_idle_connection() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("idle.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("idle.db"));
        let runtime = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 1,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .unwrap();
        let mut idle = connect(&socket);
        handshake(&mut idle);
        let started = Instant::now();
        runtime.shutdown().unwrap();
        assert!(
            started.elapsed() < CONNECTION_SHUTDOWN_GRACE + Duration::from_secs(2),
            "shutdown exceeded grace period"
        );
        drop(idle);
    }

    #[test]
    fn runtime_applies_query_limits() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("query-limit.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("query-limit.db"));
        let runtime = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 1,
                query_limits: ConnectionLimits {
                    max_queries: 1,
                    window: Duration::from_secs(60),
                },
                global_query_limits: None,
            },
        )
        .unwrap();
        let mut client = connect(&socket);
        handshake(&mut client);
        client
            .send(&IpcMessage::Query {
                request_id: 1,
                text: "a.txt".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::QueryResult { request_id: 1, .. }
        ));
        client
            .send(&IpcMessage::Query {
                request_id: 2,
                text: "a.txt".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(
            client.receive().unwrap(),
            IpcMessage::Error {
                request_id: Some(2),
                code: ErrorCode::ResourceExhausted.as_u32(),
            }
        );
        drop(client);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn global_query_limit_spans_connections() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("global-limit.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("global-limit.db"));
        let runtime = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: socket.clone(),
                poll_interval: Duration::from_millis(10),
                max_connections: 2,
                query_limits: ConnectionLimits::default(),
                global_query_limits: Some(ConnectionLimits {
                    max_queries: 1,
                    window: Duration::from_secs(60),
                }),
            },
        )
        .unwrap();
        let mut first = connect(&socket);
        handshake(&mut first);
        first
            .send(&IpcMessage::Query {
                request_id: 1,
                text: "a.txt".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert!(matches!(
            first.receive().unwrap(),
            IpcMessage::QueryResult { request_id: 1, .. }
        ));
        let mut second = connect(&socket);
        handshake(&mut second);
        second
            .send(&IpcMessage::Query {
                request_id: 2,
                text: "a.txt".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(
            second.receive().unwrap(),
            IpcMessage::Error {
                request_id: Some(2),
                code: ErrorCode::ResourceExhausted.as_u32(),
            }
        );
        drop(first);
        drop(second);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn invalid_query_limits_are_rejected() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("query-invalid.db"));
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: dir.path().join("query-invalid.sock"),
                poll_interval: Duration::from_millis(10),
                max_connections: 8,
                query_limits: ConnectionLimits {
                    max_queries: 0,
                    window: Duration::from_secs(1),
                },
                global_query_limits: None,
            },
        )
        .err()
        .expect("zero query limit must fail");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn invalid_global_query_limits_are_rejected() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("global-invalid.db"));
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: dir.path().join("global-invalid.sock"),
                poll_interval: Duration::from_millis(10),
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: Some(ConnectionLimits {
                    max_queries: 0,
                    window: Duration::from_secs(1),
                }),
            },
        )
        .err()
        .expect("zero global query limit must fail");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn invalid_connection_limit_is_rejected() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("limit-invalid.db"));
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: dir.path().join("limit-invalid.sock"),
                poll_interval: Duration::from_millis(10),
                max_connections: 0,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .err()
        .expect("zero connection limit must fail");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn worker_applies_journal_batches() {
        let dir = TempDir::new().unwrap();
        let source = SharedSource::new(journal(100), pages());
        let handle = source.clone();
        let (runtime, _socket) = start_runtime(&dir, source, Duration::from_millis(10));
        handle.set_journal(journal(110));
        handle.push_batch(JournalBatch {
            next_usn: 110,
            records: vec![record(14, 11, "c.txt", 105)],
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if runtime.search("c.txt", 10).unwrap().items.len() == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not apply journal batch"
            );
            thread::sleep(Duration::from_millis(10));
        }
        runtime.shutdown().unwrap();
    }

    #[test]
    fn poisoned_source_is_reported() {
        let source = SharedSource::new(journal(100), pages());
        let poison = source.clone();
        let thread = thread::spawn(move || {
            let _guard = poison.0.lock().unwrap();
            panic!("poison source");
        });
        assert!(thread.join().is_err());
        assert!(matches!(
            source.journal(),
            Err(crate::pipeline::PipelineError::Source(_))
        ));
    }

    #[test]
    fn missing_mft_page_is_reported() {
        let source = SharedSource::new(journal(100), Vec::new());
        assert!(matches!(
            source.mft_page(99, &journal(100)),
            Err(crate::pipeline::PipelineError::Source(_))
        ));
    }

    #[test]
    fn invalid_poll_interval_is_rejected() {
        let dir = TempDir::new().unwrap();
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("invalid.db"));
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: dir.path().join("invalid.sock"),
                poll_interval: Duration::ZERO,
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .err()
        .expect("zero poll interval must fail");
        assert_eq!(error.code(), ErrorCode::InvalidRequest);
    }

    #[test]
    fn error_categories_are_stable() {
        assert_eq!(
            RuntimeError::Engine(EngineError::NotRunning).code(),
            ErrorCode::ServerBusy
        );
        assert_eq!(
            RuntimeError::Ipc(IpcError::Closed).code(),
            ErrorCode::Transport
        );
        assert_eq!(RuntimeError::LockPoisoned.code(), ErrorCode::Internal);
        assert_eq!(RuntimeError::ThreadPanicked.code(), ErrorCode::Internal);
        assert_eq!(
            RuntimeError::InvalidPollInterval.code(),
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn startup_failures_are_reported() {
        let dir = TempDir::new().unwrap();
        let missing_parent = dir.path().join("missing").join("runtime.sock");
        let config = EngineConfig::new(meta(), "C:\\", dir.path().join("bind.db"));
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: missing_parent,
                poll_interval: Duration::from_millis(10),
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .err()
        .expect("missing socket parent must fail");
        assert!(matches!(error, RuntimeError::Ipc(_)));

        let config = EngineConfig::new(
            meta(),
            "C:\\",
            dir.path().join("missing-db").join("runtime.db"),
        );
        let error = ServerRuntime::start(
            config,
            SharedSource::new(journal(100), pages()),
            RuntimeConfig {
                socket_path: dir.path().join("engine-failure.sock"),
                poll_interval: Duration::from_millis(10),
                max_connections: 8,
                query_limits: ConnectionLimits::default(),
                global_query_limits: None,
            },
        )
        .err()
        .expect("missing database parent must fail");
        assert!(matches!(error, RuntimeError::Engine(_)));
    }
}
