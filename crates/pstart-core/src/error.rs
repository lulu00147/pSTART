//! Error taxonomy shared by the core, IPC and index crates.

use thiserror::Error;

/// Validation failures for shared model types.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelError {
    /// Volume identifier was empty.
    #[error("volume id must not be empty")]
    EmptyVolumeId,

    /// Volume identifier exceeded its hard limit.
    #[error("volume id is {actual} bytes, maximum is {max}")]
    VolumeIdTooLong {
        /// Actual byte length.
        actual: usize,
        /// Maximum allowed byte length.
        max: usize,
    },

    /// Volume identifier contained whitespace or control characters.
    #[error("volume id contains whitespace or control characters")]
    VolumeIdInvalidCharacter,

    /// File name was empty.
    #[error("file name must not be empty")]
    EmptyFileName,

    /// File name exceeded the NTFS limit.
    #[error("file name is {actual} UTF-16 units, maximum is {max}")]
    FileNameTooLong {
        /// Actual UTF-16 code unit count.
        actual: usize,
        /// Maximum allowed UTF-16 code unit count.
        max: usize,
    },

    /// File name contained a NUL character.
    #[error("file name contains NUL")]
    FileNameContainsNul,

    /// Display path exceeded its hard limit.
    #[error("path is {actual} UTF-16 units, maximum is {max}")]
    PathTooLong {
        /// Actual UTF-16 code unit count.
        actual: usize,
        /// Maximum allowed UTF-16 code unit count.
        max: usize,
    },

    /// Display path contained a NUL character.
    #[error("path contains NUL")]
    PathContainsNul,
}

/// Stable external error codes.
///
/// These values are part of the IPC contract. Never renumber an existing code;
/// add a new variant instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum ErrorCode {
    /// Operation succeeded. Not normally sent on the wire.
    Ok = 0,
    /// Frame header or payload was malformed.
    InvalidFrame = 1,
    /// Client requested an unsupported protocol version.
    UnsupportedVersion = 2,
    /// Request arrived before authentication completed.
    Unauthenticated = 3,
    /// Request was structurally valid but not allowed.
    InvalidRequest = 4,
    /// Query text exceeded the configured limit.
    QueryTooLong = 5,
    /// Requested result count or item count exceeded the limit.
    LimitExceeded = 6,
    /// Unexpected internal failure. Details are not exposed on the wire.
    Internal = 7,
    /// Server capacity is exhausted; the client may retry later.
    ServerBusy = 8,
    /// Operation timed out.
    Timeout = 9,
    /// The USN journal was reset; index rebuild is required.
    JournalReset = 10,
    /// Transport-level failure (connection closed, I/O error).
    Transport = 11,
    /// Target was not found.
    NotFound = 12,
    /// OS denied the operation.
    PermissionDenied = 13,
    /// A plugin trapped or returned invalid data.
    PluginTrap = 14,
    /// A bounded resource was exhausted.
    ResourceExhausted = 15,
}

impl ErrorCode {
    /// Convert to the stable wire value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parse a stable wire value; unknown values are rejected.
    #[must_use]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Ok),
            1 => Some(Self::InvalidFrame),
            2 => Some(Self::UnsupportedVersion),
            3 => Some(Self::Unauthenticated),
            4 => Some(Self::InvalidRequest),
            5 => Some(Self::QueryTooLong),
            6 => Some(Self::LimitExceeded),
            7 => Some(Self::Internal),
            8 => Some(Self::ServerBusy),
            9 => Some(Self::Timeout),
            10 => Some(Self::JournalReset),
            11 => Some(Self::Transport),
            12 => Some(Self::NotFound),
            13 => Some(Self::PermissionDenied),
            14 => Some(Self::PluginTrap),
            15 => Some(Self::ResourceExhausted),
            _ => None,
        }
    }

    /// Whether retrying the same request may reasonably succeed.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::ServerBusy | Self::Timeout | Self::Transport)
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?} ({})", self.as_u32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ErrorCode::InvalidFrame.as_u32(), 1);
        assert_eq!(ErrorCode::Unauthenticated.as_u32(), 3);
        assert_eq!(ErrorCode::PluginTrap.as_u32(), 14);
        assert_eq!(ErrorCode::ResourceExhausted.as_u32(), 15);
    }

    #[test]
    fn every_known_code_roundtrips() {
        for code in [
            ErrorCode::Ok,
            ErrorCode::InvalidFrame,
            ErrorCode::UnsupportedVersion,
            ErrorCode::Unauthenticated,
            ErrorCode::InvalidRequest,
            ErrorCode::QueryTooLong,
            ErrorCode::LimitExceeded,
            ErrorCode::Internal,
            ErrorCode::ServerBusy,
            ErrorCode::Timeout,
            ErrorCode::JournalReset,
            ErrorCode::Transport,
            ErrorCode::NotFound,
            ErrorCode::PermissionDenied,
            ErrorCode::PluginTrap,
            ErrorCode::ResourceExhausted,
        ] {
            assert_eq!(ErrorCode::from_u32(code.as_u32()), Some(code));
        }
        assert_eq!(ErrorCode::from_u32(999), None);
    }

    #[test]
    fn retryability_is_explicit() {
        assert!(ErrorCode::ServerBusy.is_retryable());
        assert!(ErrorCode::Timeout.is_retryable());
        assert!(!ErrorCode::Unauthenticated.is_retryable());
        assert!(!ErrorCode::QueryTooLong.is_retryable());
    }

    #[test]
    fn display_includes_name_and_stable_code() {
        assert_eq!(ErrorCode::Timeout.to_string(), "Timeout (9)");
        assert_eq!(ErrorCode::PluginTrap.to_string(), "PluginTrap (14)");
    }
}
