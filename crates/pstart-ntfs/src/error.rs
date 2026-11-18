//! NTFS error taxonomy.

use pstart_core::ErrorCode;
use thiserror::Error;

/// Error returned by NTFS discovery, journal access or record parsing.
#[derive(Debug, Error)]
pub enum NtfsError {
    /// A Win32 call failed.
    #[error("{operation} failed with Win32 error {code}: {message}")]
    Windows {
        /// Stable operation label.
        operation: &'static str,
        /// Raw Win32 error code.
        code: u32,
        /// Human-readable OS message.
        message: String,
    },

    /// The USN journal is not active or is being deleted.
    #[error("USN journal is not available on {root} (Win32 error {code})")]
    JournalNotActive {
        /// Volume root.
        root: String,
        /// Raw Win32 error code.
        code: u32,
    },

    /// The requested root is not a local drive root.
    #[error("invalid volume root: {root}")]
    InvalidRoot {
        /// Caller-provided root.
        root: String,
    },

    /// The volume is not NTFS.
    #[error("volume {root} uses unsupported filesystem {filesystem}")]
    NotNtfs {
        /// Volume root.
        root: String,
        /// Filesystem name reported by Win32.
        filesystem: String,
    },

    /// The current build cannot access NTFS APIs.
    #[error("NTFS APIs are not available on this platform")]
    UnsupportedPlatform,

    /// An IOCTL output buffer was too small or truncated.
    #[error("buffer is {actual} bytes, minimum is {minimum}")]
    BufferTooSmall {
        /// Actual byte count.
        actual: usize,
        /// Required minimum.
        minimum: usize,
    },

    /// A USN record was structurally invalid.
    #[error("malformed USN record at offset {offset}: {reason}")]
    MalformedRecord {
        /// Byte offset into the record stream.
        offset: usize,
        /// Stable reason label.
        reason: &'static str,
    },

    /// A USN record version other than V2 was returned.
    #[error("unsupported USN record version {major} at offset {offset}")]
    UnsupportedRecordVersion {
        /// Byte offset.
        offset: usize,
        /// Major version.
        major: u16,
    },

    /// A file name was not valid UTF-16 or was unsafe for path assembly.
    #[error("invalid USN file name at offset {offset}")]
    InvalidName {
        /// Byte offset.
        offset: usize,
    },

    /// A record stream exceeded its per-buffer record budget.
    #[error("record stream exceeds {limit} records")]
    TooManyRecords {
        /// Maximum accepted records.
        limit: usize,
    },

    /// A record could not be converted into an index record.
    #[error("invalid index record: {0}")]
    InvalidRecord(String),
}

impl NtfsError {
    /// Stable external error category.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Windows { code: 5, .. } => ErrorCode::PermissionDenied,
            Self::Windows { .. } => ErrorCode::Internal,
            Self::JournalNotActive { .. } => ErrorCode::JournalReset,
            Self::InvalidRoot { .. } | Self::NotNtfs { .. } => ErrorCode::InvalidRequest,
            Self::UnsupportedPlatform => ErrorCode::Internal,
            Self::BufferTooSmall { .. } | Self::TooManyRecords { .. } => {
                ErrorCode::ResourceExhausted
            }
            Self::MalformedRecord { .. } | Self::UnsupportedRecordVersion { .. } => {
                ErrorCode::InvalidFrame
            }
            Self::InvalidName { .. } | Self::InvalidRecord(_) => ErrorCode::InvalidRequest,
        }
    }
}
