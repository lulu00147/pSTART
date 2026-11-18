//! IPC errors. Every error maps to a stable [`ErrorCode`].

use pstart_core::ErrorCode;
use thiserror::Error;

/// Errors produced by framing, decoding, transport and session handling.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Socket/pipe I/O failure.
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Message could not be serialized.
    #[error("message encoding failed: {0}")]
    Encode(#[source] postcard::Error),

    /// Message could not be deserialized.
    #[error("message decoding failed: {0}")]
    Decode(#[source] postcard::Error),

    /// Frame payload exceeded the configured limit.
    #[error("frame payload is {actual} bytes, maximum is {max}")]
    FrameTooLarge {
        /// Declared payload size.
        actual: usize,
        /// Configured maximum.
        max: usize,
    },

    /// A zero-length frame was received.
    #[error("zero-length frame")]
    EmptyFrame,

    /// The peer closed before a complete header arrived.
    #[error("truncated frame header: got {actual} of 4 bytes")]
    TruncatedHeader {
        /// Bytes actually read.
        actual: usize,
    },

    /// The peer closed before a complete payload arrived.
    #[error("truncated frame payload: got {actual} of {expected} bytes")]
    TruncatedPayload {
        /// Bytes actually read.
        actual: usize,
        /// Bytes declared by the header.
        expected: usize,
    },

    /// Message failed structural validation.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// Peer identity supplied by the OS transport was missing or invalid.
    #[error("invalid peer identity: {0}")]
    InvalidPeer(String),

    /// Session is already closed.
    #[error("session is closed")]
    Closed,

    /// Peer identity could not be obtained from the OS.
    #[error("peer identity unavailable: {0}")]
    PeerIdentityUnavailable(String),

    /// Windows transport call failed.
    #[error("{operation} failed with Win32 error {code}")]
    Windows {
        /// Stable operation label.
        operation: &'static str,
        /// Raw Win32 error code.
        code: u32,
    },
}

impl IpcError {
    /// Stable wire code for this error.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Io(_)
            | Self::Closed
            | Self::TruncatedHeader { .. }
            | Self::TruncatedPayload { .. } => ErrorCode::Transport,
            Self::Encode(_) | Self::Decode(_) | Self::EmptyFrame | Self::FrameTooLarge { .. } => {
                ErrorCode::InvalidFrame
            }
            Self::InvalidMessage(_) => ErrorCode::InvalidRequest,
            Self::InvalidPeer(_) | Self::PeerIdentityUnavailable(_) => ErrorCode::Unauthenticated,
            Self::Windows { code: 5, .. } => ErrorCode::PermissionDenied,
            Self::Windows { .. } => ErrorCode::Transport,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn postcard_error() -> postcard::Error {
        postcard::from_bytes::<u32>(&[0xff]).unwrap_err()
    }

    #[test]
    fn every_variant_maps_to_a_stable_code() {
        let cases = [
            (IpcError::Io(io::Error::other("io")), ErrorCode::Transport),
            (IpcError::Encode(postcard_error()), ErrorCode::InvalidFrame),
            (IpcError::Decode(postcard_error()), ErrorCode::InvalidFrame),
            (
                IpcError::FrameTooLarge { actual: 2, max: 1 },
                ErrorCode::InvalidFrame,
            ),
            (IpcError::EmptyFrame, ErrorCode::InvalidFrame),
            (
                IpcError::TruncatedHeader { actual: 1 },
                ErrorCode::Transport,
            ),
            (
                IpcError::TruncatedPayload {
                    actual: 1,
                    expected: 2,
                },
                ErrorCode::Transport,
            ),
            (
                IpcError::InvalidMessage("bad".to_owned()),
                ErrorCode::InvalidRequest,
            ),
            (
                IpcError::InvalidPeer("bad".to_owned()),
                ErrorCode::Unauthenticated,
            ),
            (
                IpcError::PeerIdentityUnavailable("none".to_owned()),
                ErrorCode::Unauthenticated,
            ),
            (
                IpcError::Windows {
                    operation: "test",
                    code: 5,
                },
                ErrorCode::PermissionDenied,
            ),
            (
                IpcError::Windows {
                    operation: "test",
                    code: 6,
                },
                ErrorCode::Transport,
            ),
            (IpcError::Closed, ErrorCode::Transport),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code(), expected, "{error}");
        }
    }
}
