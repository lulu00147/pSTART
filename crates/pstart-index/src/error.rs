//! Index errors and recovery categories.

use pstart_core::ModelError;
use thiserror::Error;

use crate::path::PathError;
use crate::state::TransitionError;

/// Errors produced by the index subsystem.
#[derive(Debug, Error)]
pub enum IndexError {
    /// Shared model validation failed.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// Path reconstruction failed.
    #[error(transparent)]
    Path(#[from] PathError),

    /// State machine transition failed.
    #[error(transparent)]
    State(#[from] TransitionError),

    /// SQLite failure.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A record or batch violated an index invariant.
    #[error("invalid index data: {0}")]
    InvalidData(String),

    /// A checkpoint belongs to a different journal.
    #[error("journal mismatch: expected {expected}, got {actual}")]
    JournalMismatch {
        /// Journal identifier currently bound to the volume state.
        expected: u64,
        /// Journal identifier in the checkpoint or observation.
        actual: u64,
    },

    /// A checkpoint would advance beyond data observed by the ingester.
    #[error("checkpoint next_usn {next_usn} is ahead of observed_usn {observed_usn}")]
    CheckpointAhead {
        /// Requested checkpoint position.
        next_usn: u64,
        /// Highest observed USN.
        observed_usn: u64,
    },

    /// A checkpoint would move backwards.
    #[error("checkpoint next_usn {next_usn} is behind committed_usn {committed_usn}")]
    CheckpointRegression {
        /// Requested checkpoint position.
        next_usn: u64,
        /// Last committed position.
        committed_usn: u64,
    },

    /// A checkpoint was proposed in a state that does not accept commits.
    #[error("checkpoint is not allowed in state {state:?}")]
    CheckpointNotAllowed {
        /// Current state.
        state: crate::state::SyncState,
    },
}
