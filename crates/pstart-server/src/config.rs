//! Service configuration resolved from environment variables.

use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use pstart_core::limits::MAX_IPC_CONNECTIONS;
use pstart_core::VolumeId;
use pstart_index::{IndexError, VolumeMeta};

use crate::engine::{EngineConfig, SearchBackend};

/// Default service data directory.
pub const DEFAULT_DATA_DIR: &str = r"C:\ProgramData\pSTART";
/// Default concurrent connection limit.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// Default per-connection queries per second.
pub const DEFAULT_QUERY_LIMIT_PER_SECOND: u32 = 200;
/// Default runtime-wide queries per second.
pub const DEFAULT_GLOBAL_QUERY_LIMIT_PER_SECOND: u32 = 800;

/// Service configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Directory for the database, socket and logs.
    pub data_dir: PathBuf,
    /// Normalized NTFS root, for example `C:\`.
    pub root: String,
    /// AF_UNIX socket path.
    pub socket_path: PathBuf,
    /// Journal polling interval.
    pub poll_interval: Duration,
    /// Maximum concurrent query connections.
    pub max_connections: usize,
    /// Per-connection query limit per second.
    pub query_limit_per_second: u32,
    /// Runtime-wide query limit per second.
    pub global_query_limit_per_second: u32,
}

impl ServiceConfig {
    /// Build and validate a configuration.
    pub fn new(
        data_dir: PathBuf,
        root: &str,
        poll_interval: Duration,
    ) -> Result<Self, ConfigError> {
        if poll_interval.is_zero() {
            return Err(ConfigError::InvalidPollInterval);
        }
        let root = normalize_root(root)?;
        let socket_path = data_dir.join("ipc").join("pstart.sock");
        Ok(Self {
            data_dir,
            root,
            socket_path,
            poll_interval,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            query_limit_per_second: DEFAULT_QUERY_LIMIT_PER_SECOND,
            global_query_limit_per_second: DEFAULT_GLOBAL_QUERY_LIMIT_PER_SECOND,
        })
    }

    /// Apply resource limits and validate them.
    pub fn with_limits(
        mut self,
        max_connections: usize,
        query_limit_per_second: u32,
        global_query_limit_per_second: u32,
    ) -> Result<Self, ConfigError> {
        if !(1..=MAX_IPC_CONNECTIONS).contains(&max_connections) {
            return Err(ConfigError::InvalidMaxConnections);
        }
        if query_limit_per_second == 0 {
            return Err(ConfigError::InvalidQueryLimit);
        }
        if global_query_limit_per_second == 0 {
            return Err(ConfigError::InvalidGlobalQueryLimit);
        }
        self.max_connections = max_connections;
        self.query_limit_per_second = query_limit_per_second;
        self.global_query_limit_per_second = global_query_limit_per_second;
        Ok(self)
    }

    /// Load configuration from the service environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let data_dir = lookup("PSTART_DATA_DIR")
            .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from);
        let root = lookup("PSTART_ROOT").unwrap_or_else(|| "C:\\".to_owned());
        let poll_ms = lookup("PSTART_POLL_MS")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| ConfigError::InvalidPollInterval)
            })
            .transpose()?
            .unwrap_or(500);
        let max_connections = lookup("PSTART_MAX_CONNECTIONS")
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| ConfigError::InvalidMaxConnections)
            })
            .transpose()?
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);
        let query_limit = lookup("PSTART_QUERY_LIMIT")
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| ConfigError::InvalidQueryLimit)
            })
            .transpose()?
            .unwrap_or(DEFAULT_QUERY_LIMIT_PER_SECOND);
        let global_limit = lookup("PSTART_GLOBAL_QUERY_LIMIT")
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| ConfigError::InvalidGlobalQueryLimit)
            })
            .transpose()?
            .unwrap_or(DEFAULT_GLOBAL_QUERY_LIMIT_PER_SECOND);
        Self::new(data_dir, &root, Duration::from_millis(poll_ms))?.with_limits(
            max_connections,
            query_limit,
            global_limit,
        )
    }

    /// Create data and IPC directories.
    pub fn ensure_directories(&self) -> Result<(), ConfigError> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.data_dir.join("ipc"))?;
        Ok(())
    }

    /// SQLite database path.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("index.db")
    }

    /// Volume metadata for the configured root.
    pub fn volume_meta(&self) -> Result<VolumeMeta, ConfigError> {
        let volume = VolumeId::new(volume_id(&self.root)?)
            .map_err(|error| ConfigError::VolumeId(error.to_string()))?;
        VolumeMeta::new(volume, "System", "NTFS").map_err(ConfigError::Index)
    }

    /// Engine configuration.
    pub fn engine_config(&self) -> Result<EngineConfig, ConfigError> {
        let mut config =
            EngineConfig::new(self.volume_meta()?, self.root.clone(), self.database_path());
        config.defer_scan = true;
        config.search_backend = SearchBackend::Sqlite;
        config.include_system_directories = std::env::var("PSTART_INCLUDE_SYSTEM")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        if std::env::var("PSTART_SEARCH_BACKEND")
            .is_ok_and(|value| value.eq_ignore_ascii_case("memory"))
        {
            config.search_backend = SearchBackend::Memory;
        }
        Ok(config)
    }
}

/// Configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Invalid volume root.
    #[error("invalid volume root: {0}")]
    InvalidRoot(String),
    /// Invalid poll interval.
    #[error("poll interval must be positive")]
    InvalidPollInterval,
    /// Invalid concurrent connection limit.
    #[error("max connections must be in 1..=64")]
    InvalidMaxConnections,
    /// Invalid per-connection query limit.
    #[error("query limit must be positive")]
    InvalidQueryLimit,
    /// Invalid runtime-wide query limit.
    #[error("global query limit must be positive")]
    InvalidGlobalQueryLimit,
    /// Volume identifier rejected.
    #[error("invalid volume id: {0}")]
    VolumeId(String),
    /// Directory operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Volume metadata rejected.
    #[error(transparent)]
    Index(#[from] IndexError),
}

fn normalize_root(root: &str) -> Result<String, ConfigError> {
    let bytes = root.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return Err(ConfigError::InvalidRoot(root.to_owned()));
    }
    Ok(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase()))
}

fn volume_id(root: &str) -> Result<String, ConfigError> {
    let letter = root
        .as_bytes()
        .first()
        .filter(|byte| byte.is_ascii_alphabetic())
        .ok_or_else(|| ConfigError::InvalidRoot(root.to_owned()))?;
    Ok(format!("{}:", (*letter as char).to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn valid_configuration_normalizes_root() {
        let dir = TempDir::new().unwrap();
        let config =
            ServiceConfig::new(dir.path().to_path_buf(), "c:", Duration::from_millis(10)).unwrap();
        assert_eq!(config.root, "C:\\");
        assert_eq!(
            config.socket_path,
            dir.path().join("ipc").join("pstart.sock")
        );
        assert_eq!(config.database_path(), dir.path().join("index.db"));
        assert_eq!(config.volume_meta().unwrap().volume.as_str(), "C:");
        assert_eq!(config.engine_config().unwrap().root, "C:\\");
    }

    #[test]
    fn invalid_roots_and_poll_are_rejected() {
        let dir = TempDir::new().unwrap();
        for root in ["", "nope", "1:", "CC"] {
            assert!(matches!(
                ServiceConfig::new(dir.path().to_path_buf(), root, Duration::from_millis(1)),
                Err(ConfigError::InvalidRoot(_))
            ));
        }
        assert!(matches!(
            ServiceConfig::new(dir.path().to_path_buf(), "C:\\", Duration::ZERO),
            Err(ConfigError::InvalidPollInterval)
        ));
    }

    #[test]
    fn directories_are_created() {
        let dir = TempDir::new().unwrap();
        let data = dir.path().join("data");
        let config = ServiceConfig::new(data.clone(), "C:\\", Duration::from_millis(1)).unwrap();
        config.ensure_directories().unwrap();
        assert!(data.exists());
        assert!(data.join("ipc").exists());
    }

    #[test]
    fn engine_config_honors_memory_backend_and_restores_environment() {
        let directory = TempDir::new().unwrap();
        for seed in [None, Some("sentinel")] {
            match seed {
                Some(value) => std::env::set_var("PSTART_SEARCH_BACKEND", value),
                None => std::env::remove_var("PSTART_SEARCH_BACKEND"),
            }
            let previous = std::env::var("PSTART_SEARCH_BACKEND").ok();
            std::env::set_var("PSTART_SEARCH_BACKEND", "memory");
            let config = ServiceConfig::from_lookup(|name| match name {
                "PSTART_DATA_DIR" => Some(directory.path().display().to_string()),
                "PSTART_ROOT" => Some("C:\\".to_owned()),
                _ => None,
            })
            .unwrap();
            assert_eq!(
                config.engine_config().unwrap().search_backend,
                SearchBackend::Memory
            );
            if let Some(value) = previous {
                std::env::set_var("PSTART_SEARCH_BACKEND", value);
            } else {
                std::env::remove_var("PSTART_SEARCH_BACKEND");
            }
        }
    }

    #[test]
    fn environment_lookup_is_validated() {
        let dir = TempDir::new().unwrap();
        let data = dir.path().join("env-data");
        let data_value = data.to_string_lossy().into_owned();
        let environment = HashMap::from([
            ("PSTART_DATA_DIR", data_value.as_str()),
            ("PSTART_ROOT", "d:"),
            ("PSTART_POLL_MS", "25"),
            ("PSTART_MAX_CONNECTIONS", "16"),
            ("PSTART_QUERY_LIMIT", "50"),
            ("PSTART_GLOBAL_QUERY_LIMIT", "100"),
        ]);
        let config = ServiceConfig::from_lookup(|name| {
            environment.get(name).map(|value| (*value).to_owned())
        })
        .unwrap();
        assert_eq!(config.root, "D:\\");
        assert_eq!(config.data_dir, data);
        assert_eq!(config.poll_interval, Duration::from_millis(25));
        assert_eq!(config.max_connections, 16);
        assert_eq!(config.query_limit_per_second, 50);
        assert_eq!(config.global_query_limit_per_second, 100);

        let defaults = ServiceConfig::from_lookup(|_| None).unwrap();
        assert_eq!(defaults.root, "C:\\");
        assert_eq!(defaults.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(defaults.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(
            defaults.query_limit_per_second,
            DEFAULT_QUERY_LIMIT_PER_SECOND
        );
        assert_eq!(
            defaults.global_query_limit_per_second,
            DEFAULT_GLOBAL_QUERY_LIMIT_PER_SECOND
        );

        let from_env = ServiceConfig::from_env().unwrap();
        assert_eq!(from_env.root, "C:\\");

        let error = ServiceConfig::from_lookup(|name| match name {
            "PSTART_ROOT" => Some("bad".to_owned()),
            _ => None,
        })
        .err()
        .unwrap();
        assert!(matches!(error, ConfigError::InvalidRoot(_)));

        let error = ServiceConfig::from_lookup(|name| match name {
            "PSTART_POLL_MS" => Some("not-a-number".to_owned()),
            _ => None,
        })
        .err()
        .unwrap();
        assert!(matches!(error, ConfigError::InvalidPollInterval));

        for (name, value) in [
            ("PSTART_MAX_CONNECTIONS", "0"),
            ("PSTART_MAX_CONNECTIONS", "65"),
            ("PSTART_QUERY_LIMIT", "0"),
            ("PSTART_GLOBAL_QUERY_LIMIT", "0"),
        ] {
            let error = ServiceConfig::from_lookup(|key| (key == name).then(|| value.to_owned()))
                .err()
                .unwrap();
            assert!(matches!(
                error,
                ConfigError::InvalidMaxConnections
                    | ConfigError::InvalidQueryLimit
                    | ConfigError::InvalidGlobalQueryLimit
            ));
        }
    }
}
