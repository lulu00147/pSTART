//! pSTART index core.
//!
//! This crate is platform-independent: NTFS scanning lives in `pstart-ntfs`,
//! while state transitions, path reconstruction, search and SQLite persistence
//! are testable on any platform.

pub mod error;
pub mod memory;
pub mod model;
pub mod path;
pub mod score;
pub mod sqlite_search;
pub mod state;
pub mod store;
pub mod trigram;

pub use error::IndexError;
pub use memory::{
    MemoryIndex, SearchHit, SearchOutcome, MAX_PREFIX_CANDIDATES, MAX_SUBSTRING_SCAN,
};
pub use model::{FileRecord, IndexEvent};
pub use path::{resolve_path, PathError, RecordLookup};
pub use score::score_name;
pub use sqlite_search::SqliteSearch;
pub use state::{
    Checkpoint, JournalObservation, SyncEvent, SyncState, Transition, TransitionError, VolumeSync,
};
pub use store::{
    CheckpointStatus, CommitStats, DeletedFrn, IndexStore, PersistBatch, StoredVolumeState,
    VolumeMeta, SCHEMA_VERSION,
};
pub use trigram::{
    delta_directory, TrigramBuilder, TrigramIndex, TrigramSet, MAX_TRIGRAM_CANDIDATES,
};
