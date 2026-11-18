//! pSTART Server.
//!
//! Windows Service lifecycle and the IPC listener are implemented in M5. The
//! volume index pipeline is available here because it coordinates both the NTFS
//! source and SQLite persistence.

pub mod admission;
pub mod command;
pub mod config;
pub mod engine;
pub mod pipeline;
#[cfg(windows)]
pub mod runtime;
pub mod service;

pub use command::{parse_command, usage, ServerCommand};
pub use config::{ConfigError, ServiceConfig};
pub use engine::{
    EngineConfig, EngineError, EngineStatus, SearchResponse, ServerEngine, StartupReport,
    TickReport, DEFAULT_CATCH_UP_BATCHES,
};
pub use pipeline::{
    CatchUpReport, IndexPipeline, PipelineError, ScanReport, StartupAction, VolumeSource,
    DEFAULT_BATCH_LIMIT,
};
#[cfg(windows)]
pub use runtime::{RuntimeConfig, RuntimeError, ServerRuntime};
pub use service::{
    serve_connection, serve_connection_with_global_limit, serve_connection_with_limits,
    ConnectionLimits, QueryBackend, ServeStats, ServiceError, SharedQueryLimit,
    DEFAULT_MAX_QUERIES_PER_WINDOW, DEFAULT_QUERY_WINDOW,
};

#[cfg(windows)]
pub use pipeline::NtfsSource;

/// Service name used by the Windows Service Control Manager.
pub const SERVICE_NAME: &str = "pSTART-Server";
