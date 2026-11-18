//! Volume synchronization state machine and checkpoint rules.
//!
//! The central safety rule: a checkpoint may only advance to a USN that has
//! already been observed and committed by the persistence layer. This type only
//! enforces the ordering contract; it does not itself write SQLite.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::IndexError;

/// Volume synchronization states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncState {
    /// No journal or checkpoint has been established.
    Uninitialized,
    /// Full MFT scan in progress.
    InitialScan,
    /// Replaying journal records produced after the scan started.
    CatchUp,
    /// Journal tail is being consumed live.
    LiveSync,
    /// Journal identity/consistency changed; full rebuild is required.
    RebuildRequired,
    /// Volume or service temporarily unavailable.
    Paused,
    /// Service stopped or volume failed permanently for this process.
    Stopped,
    /// Unrecoverable error until an explicit reinitialization.
    Failed,
}

impl SyncState {
    /// Stable persistence string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::InitialScan => "initial_scan",
            Self::CatchUp => "catch_up",
            Self::LiveSync => "live_sync",
            Self::RebuildRequired => "rebuild_required",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    /// Parse a persisted state string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "uninitialized" => Some(Self::Uninitialized),
            "initial_scan" => Some(Self::InitialScan),
            "catch_up" => Some(Self::CatchUp),
            "live_sync" => Some(Self::LiveSync),
            "rebuild_required" => Some(Self::RebuildRequired),
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Events accepted by the volume state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncEvent {
    /// Begin or restart synchronization.
    Initialize,
    /// Initial scan reached a consistent checkpoint.
    ScanCompleted,
    /// Catch-up replay reached the journal tail.
    ReplayCompleted,
    /// Journal ID changed or journal was deleted.
    JournalReset,
    /// Volume became unavailable.
    VolumeUnavailable,
    /// Volume became available again.
    Resume,
    /// Resume catch-up using persisted data after pause or shutdown.
    ResumeCatchUp,
    /// Begin a rebuild from the current journal.
    Rebuild,
    /// Service shutdown.
    Shutdown,
    /// Enter the failed state.
    Fail,
}

/// A successful state transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    /// State before the event.
    pub from: SyncState,
    /// State after the event.
    pub to: SyncState,
}

/// Illegal state transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("event {event:?} is not allowed in state {state:?}")]
pub struct TransitionError {
    /// Current state.
    pub state: SyncState,
    /// Rejected event.
    pub event: SyncEvent,
}

/// Result of comparing an observed journal ID with the bound one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalObservation {
    /// First journal ID bound to this volume.
    New,
    /// Journal matches the bound ID.
    Match,
    /// Journal differs; rebuild is required.
    Mismatch {
        /// Previously bound journal ID.
        previous: u64,
        /// Newly observed journal ID.
        observed: u64,
    },
}

/// Durable USN checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Journal identifier this checkpoint belongs to.
    pub journal_id: u64,
    /// Next USN to read.
    pub next_usn: u64,
    /// Index generation that produced the data.
    pub index_generation: u64,
    /// Unix timestamp of the transaction commit.
    pub committed_at: u64,
}

/// Volume synchronization state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSync {
    state: SyncState,
    journal_id: Option<u64>,
    index_generation: u64,
    committed_usn: u64,
    observed_usn: u64,
    paused_from: Option<SyncState>,
}

impl Default for VolumeSync {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeSync {
    /// Create an uninitialized state machine.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: SyncState::Uninitialized,
            journal_id: None,
            index_generation: 0,
            committed_usn: 0,
            observed_usn: 0,
            paused_from: None,
        }
    }

    /// Restore a state machine from persisted values.
    #[must_use]
    pub fn restore(
        state: SyncState,
        journal_id: Option<u64>,
        index_generation: u64,
        committed_usn: u64,
    ) -> Self {
        Self {
            state,
            journal_id,
            index_generation,
            committed_usn,
            observed_usn: committed_usn,
            paused_from: None,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> SyncState {
        self.state
    }

    /// State that must be resumed or rebuilt after a controlled restart.
    #[must_use]
    pub fn restart_state(&self) -> Option<SyncState> {
        match self.state {
            SyncState::InitialScan | SyncState::RebuildRequired | SyncState::Failed => {
                Some(self.state)
            }
            SyncState::Paused => self.paused_from.filter(|state| {
                matches!(
                    state,
                    SyncState::InitialScan | SyncState::RebuildRequired | SyncState::Failed
                )
            }),
            _ => None,
        }
    }

    /// Bound journal identifier.
    #[must_use]
    pub const fn journal_id(&self) -> Option<u64> {
        self.journal_id
    }

    /// Current index generation.
    #[must_use]
    pub const fn index_generation(&self) -> u64 {
        self.index_generation
    }

    /// Last committed USN.
    #[must_use]
    pub const fn committed_usn(&self) -> u64 {
        self.committed_usn
    }

    /// Highest observed USN.
    #[must_use]
    pub const fn observed_usn(&self) -> u64 {
        self.observed_usn
    }

    /// Apply one event.
    pub fn apply(&mut self, event: SyncEvent) -> Result<Transition, TransitionError> {
        let from = self.state;
        let to = match (from, event) {
            (
                SyncState::Uninitialized | SyncState::Stopped | SyncState::Failed,
                SyncEvent::Initialize,
            ) => SyncState::InitialScan,
            (SyncState::InitialScan, SyncEvent::ScanCompleted) => SyncState::CatchUp,
            (SyncState::CatchUp, SyncEvent::ReplayCompleted) => SyncState::LiveSync,
            (
                SyncState::InitialScan | SyncState::CatchUp | SyncState::LiveSync,
                SyncEvent::JournalReset,
            ) => SyncState::RebuildRequired,
            (SyncState::RebuildRequired, SyncEvent::Rebuild) => {
                self.index_generation = self.index_generation.saturating_add(1);
                self.committed_usn = 0;
                self.observed_usn = 0;
                self.journal_id = None;
                SyncState::InitialScan
            }
            (SyncState::Failed, SyncEvent::Rebuild) => {
                self.index_generation = self.index_generation.saturating_add(1);
                self.committed_usn = 0;
                self.observed_usn = 0;
                self.journal_id = None;
                SyncState::InitialScan
            }
            (
                SyncState::Uninitialized
                | SyncState::InitialScan
                | SyncState::CatchUp
                | SyncState::LiveSync
                | SyncState::RebuildRequired
                | SyncState::Failed,
                SyncEvent::VolumeUnavailable,
            ) => {
                self.paused_from = Some(from);
                SyncState::Paused
            }
            (SyncState::Paused, SyncEvent::Resume) => {
                self.paused_from.take().unwrap_or(SyncState::Uninitialized)
            }
            (SyncState::Paused | SyncState::Stopped, SyncEvent::ResumeCatchUp) => {
                self.paused_from = None;
                SyncState::CatchUp
            }
            (_, SyncEvent::Shutdown) if from != SyncState::Stopped => SyncState::Stopped,
            (_, SyncEvent::Fail) if !matches!(from, SyncState::Stopped | SyncState::Failed) => {
                SyncState::Failed
            }
            _ => return Err(TransitionError { state: from, event }),
        };

        self.state = to;
        if to != SyncState::Paused {
            self.paused_from = None;
        }
        Ok(Transition { from, to })
    }

    /// Bind or compare a journal ID.
    ///
    /// A mismatch moves the machine to [`SyncState::RebuildRequired`]; it never
    /// silently reuses the old checkpoint.
    pub fn observe_journal(&mut self, observed: u64) -> JournalObservation {
        match self.journal_id {
            None => {
                self.journal_id = Some(observed);
                JournalObservation::New
            }
            Some(previous) if previous == observed => JournalObservation::Match,
            Some(previous) => {
                if !matches!(
                    self.state,
                    SyncState::Uninitialized
                        | SyncState::Stopped
                        | SyncState::Paused
                        | SyncState::Failed
                ) {
                    self.state = SyncState::RebuildRequired;
                }
                JournalObservation::Mismatch { previous, observed }
            }
        }
    }

    /// Record the highest USN observed by the ingester.
    pub fn observe_usn(&mut self, usn: u64) {
        self.observed_usn = self.observed_usn.max(usn);
    }

    /// Build a checkpoint for the current committed position.
    #[must_use]
    pub fn checkpoint(&self, committed_at: u64) -> Option<Checkpoint> {
        self.journal_id.map(|journal_id| Checkpoint {
            journal_id,
            next_usn: self.committed_usn,
            index_generation: self.index_generation,
            committed_at,
        })
    }

    /// Commit a checkpoint after the caller has successfully committed the
    /// corresponding metadata transaction.
    ///
    /// This method is deliberately separate from persistence so that callers
    /// cannot advance the in-memory cursor before the SQL transaction commits.
    pub fn commit_checkpoint(&mut self, checkpoint: &Checkpoint) -> Result<(), IndexError> {
        if !matches!(
            self.state,
            SyncState::InitialScan | SyncState::CatchUp | SyncState::LiveSync
        ) {
            return Err(IndexError::CheckpointNotAllowed { state: self.state });
        }
        let journal_id = self.journal_id.ok_or(IndexError::InvalidData(
            "checkpoint committed without a bound journal".to_owned(),
        ))?;
        if checkpoint.journal_id != journal_id {
            return Err(IndexError::JournalMismatch {
                expected: journal_id,
                actual: checkpoint.journal_id,
            });
        }
        if checkpoint.index_generation != self.index_generation {
            return Err(IndexError::InvalidData(format!(
                "checkpoint generation {} does not match {}",
                checkpoint.index_generation, self.index_generation
            )));
        }
        if checkpoint.next_usn < self.committed_usn {
            return Err(IndexError::CheckpointRegression {
                next_usn: checkpoint.next_usn,
                committed_usn: self.committed_usn,
            });
        }
        if checkpoint.next_usn > self.observed_usn {
            return Err(IndexError::CheckpointAhead {
                next_usn: checkpoint.next_usn,
                observed_usn: self.observed_usn,
            });
        }
        self.committed_usn = checkpoint.next_usn;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_transitions() {
        let mut sync = VolumeSync::new();
        assert_eq!(
            sync.apply(SyncEvent::Initialize).unwrap().to,
            SyncState::InitialScan
        );
        assert_eq!(
            sync.apply(SyncEvent::ScanCompleted).unwrap().to,
            SyncState::CatchUp
        );
        assert_eq!(
            sync.apply(SyncEvent::ReplayCompleted).unwrap().to,
            SyncState::LiveSync
        );
    }

    #[test]
    fn illegal_skip_is_rejected() {
        let mut sync = VolumeSync::new();
        assert_eq!(
            sync.apply(SyncEvent::ScanCompleted),
            Err(TransitionError {
                state: SyncState::Uninitialized,
                event: SyncEvent::ScanCompleted
            })
        );
    }

    #[test]
    fn journal_reset_requires_rebuild() {
        let mut sync = VolumeSync::new();
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.apply(SyncEvent::ScanCompleted).unwrap();
        sync.apply(SyncEvent::ReplayCompleted).unwrap();
        sync.observe_journal(7);
        assert_eq!(
            sync.apply(SyncEvent::JournalReset).unwrap().to,
            SyncState::RebuildRequired
        );
        let transition = sync.apply(SyncEvent::Rebuild).unwrap();
        assert_eq!(transition.to, SyncState::InitialScan);
        assert_eq!(sync.index_generation(), 1);
        assert_eq!(sync.journal_id(), None);
    }

    #[test]
    fn journal_mismatch_never_reuses_checkpoint() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(1);
        sync.apply(SyncEvent::Initialize).unwrap();
        assert_eq!(
            sync.observe_journal(2),
            JournalObservation::Mismatch {
                previous: 1,
                observed: 2
            }
        );
        assert_eq!(sync.state(), SyncState::RebuildRequired);
        assert_eq!(sync.journal_id(), Some(1));
    }

    #[test]
    fn restart_state_tracks_paused_source() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(7);
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.apply(SyncEvent::VolumeUnavailable).unwrap();
        assert_eq!(sync.restart_state(), Some(SyncState::InitialScan));

        sync.apply(SyncEvent::Resume).unwrap();
        sync.apply(SyncEvent::ScanCompleted).unwrap();
        sync.apply(SyncEvent::ReplayCompleted).unwrap();
        sync.apply(SyncEvent::VolumeUnavailable).unwrap();
        assert_eq!(sync.restart_state(), None);
    }

    #[test]
    fn pause_and_resume_restore_previous_state() {
        let mut sync = VolumeSync::new();
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.apply(SyncEvent::ScanCompleted).unwrap();
        assert_eq!(
            sync.apply(SyncEvent::VolumeUnavailable).unwrap().to,
            SyncState::Paused
        );
        assert_eq!(
            sync.apply(SyncEvent::Resume).unwrap().to,
            SyncState::CatchUp
        );
    }

    #[test]
    fn shutdown_is_terminal_until_initialize() {
        let mut sync = VolumeSync::new();
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.apply(SyncEvent::Shutdown).unwrap();
        assert_eq!(sync.state(), SyncState::Stopped);
        assert!(sync.apply(SyncEvent::ScanCompleted).is_err());
        assert_eq!(
            sync.apply(SyncEvent::Initialize).unwrap().to,
            SyncState::InitialScan
        );
    }

    #[test]
    fn checkpoint_requires_observed_usn() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(10);
        sync.apply(SyncEvent::Initialize).unwrap();
        let checkpoint = Checkpoint {
            journal_id: 10,
            next_usn: 100,
            index_generation: 0,
            committed_at: 1,
        };
        assert!(matches!(
            sync.commit_checkpoint(&checkpoint),
            Err(IndexError::CheckpointAhead {
                next_usn: 100,
                observed_usn: 0
            })
        ));
        sync.observe_usn(100);
        sync.commit_checkpoint(&checkpoint).unwrap();
        assert_eq!(sync.committed_usn(), 100);
    }

    #[test]
    fn checkpoint_is_monotonic() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(10);
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.observe_usn(200);
        sync.commit_checkpoint(&Checkpoint {
            journal_id: 10,
            next_usn: 100,
            index_generation: 0,
            committed_at: 1,
        })
        .unwrap();
        assert!(matches!(
            sync.commit_checkpoint(&Checkpoint {
                journal_id: 10,
                next_usn: 99,
                index_generation: 0,
                committed_at: 2,
            }),
            Err(IndexError::CheckpointRegression {
                next_usn: 99,
                committed_usn: 100
            })
        ));
    }

    #[test]
    fn checkpoint_rejects_wrong_journal_and_generation() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(10);
        sync.apply(SyncEvent::Initialize).unwrap();
        sync.observe_usn(200);
        assert!(matches!(
            sync.commit_checkpoint(&Checkpoint {
                journal_id: 11,
                next_usn: 100,
                index_generation: 0,
                committed_at: 1,
            }),
            Err(IndexError::JournalMismatch { .. })
        ));
        assert!(matches!(
            sync.commit_checkpoint(&Checkpoint {
                journal_id: 10,
                next_usn: 100,
                index_generation: 1,
                committed_at: 1,
            }),
            Err(IndexError::InvalidData(_))
        ));
    }

    #[test]
    fn checkpoint_not_allowed_before_initialize() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(10);
        sync.observe_usn(100);
        assert!(matches!(
            sync.commit_checkpoint(&Checkpoint {
                journal_id: 10,
                next_usn: 100,
                index_generation: 0,
                committed_at: 1,
            }),
            Err(IndexError::CheckpointNotAllowed { .. })
        ));
    }

    #[test]
    fn restore_preserves_committed_position() {
        let sync = VolumeSync::restore(SyncState::LiveSync, Some(3), 4, 500);
        assert_eq!(sync.committed_usn(), 500);
        assert_eq!(sync.observed_usn(), 500);
        assert_eq!(sync.index_generation(), 4);
    }

    #[test]
    fn default_is_uninitialized() {
        let sync = VolumeSync::default();
        assert_eq!(sync.state(), SyncState::Uninitialized);
        assert_eq!(sync.journal_id(), None);
        assert_eq!(sync.committed_usn(), 0);
    }

    #[test]
    fn failed_state_can_rebuild() {
        let mut sync = VolumeSync::restore(SyncState::LiveSync, Some(3), 4, 500);
        assert_eq!(sync.apply(SyncEvent::Fail).unwrap().to, SyncState::Failed);
        let transition = sync.apply(SyncEvent::Rebuild).unwrap();
        assert_eq!(transition.to, SyncState::InitialScan);
        assert_eq!(sync.index_generation(), 5);
        assert_eq!(sync.journal_id(), None);
        assert_eq!(sync.committed_usn(), 0);
    }

    #[test]
    fn fail_event_is_explicit() {
        let mut sync = VolumeSync::new();
        sync.apply(SyncEvent::Initialize).unwrap();
        assert_eq!(sync.apply(SyncEvent::Fail).unwrap().to, SyncState::Failed);
        assert!(sync.apply(SyncEvent::Fail).is_err());
    }

    #[test]
    fn checkpoint_builder_requires_a_journal() {
        let mut sync = VolumeSync::new();
        assert_eq!(sync.checkpoint(1), None);
        sync.observe_journal(7);
        sync.observe_usn(42);
        assert_eq!(
            sync.checkpoint(99),
            Some(Checkpoint {
                journal_id: 7,
                next_usn: 0,
                index_generation: 0,
                committed_at: 99,
            })
        );
    }

    #[test]
    fn observed_usn_is_monotonic() {
        let mut sync = VolumeSync::new();
        sync.observe_usn(10);
        sync.observe_usn(5);
        assert_eq!(sync.observed_usn(), 10);
    }

    #[test]
    fn journal_mismatch_in_paused_state_does_not_change_state() {
        let mut sync = VolumeSync::new();
        sync.observe_journal(1);
        sync.apply(SyncEvent::VolumeUnavailable).unwrap();
        assert_eq!(
            sync.observe_journal(2),
            JournalObservation::Mismatch {
                previous: 1,
                observed: 2
            }
        );
        assert_eq!(sync.state(), SyncState::Paused);
    }

    #[test]
    fn sync_state_strings_roundtrip() {
        for state in [
            SyncState::Uninitialized,
            SyncState::InitialScan,
            SyncState::CatchUp,
            SyncState::LiveSync,
            SyncState::RebuildRequired,
            SyncState::Paused,
            SyncState::Stopped,
            SyncState::Failed,
        ] {
            assert_eq!(SyncState::parse(state.as_str()), Some(state));
        }
        assert_eq!(SyncState::parse("unknown"), None);
    }

    #[test]
    fn resume_catchup_from_stopped_or_paused() {
        for start in [SyncState::Paused, SyncState::Stopped] {
            let mut sync = VolumeSync::restore(start, Some(7), 2, 100);
            assert_eq!(
                sync.apply(SyncEvent::ResumeCatchUp).unwrap().to,
                SyncState::CatchUp
            );
            assert_eq!(sync.committed_usn(), 100);
        }
        let mut sync = VolumeSync::new();
        assert!(sync.apply(SyncEvent::ResumeCatchUp).is_err());
    }
}
