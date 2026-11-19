//! Shared, platform-independent types for pSTART.
//!
//! This crate is the single source of truth for protocol limits, stable
//! identifiers and the externally observable error taxonomy. It must not
//! depend on Windows APIs or on any transport implementation.

#![forbid(unsafe_code)]

pub mod error;
pub mod ids;
pub mod limits;
pub mod model;
pub mod session;
pub mod text;
pub mod walker;

pub use error::{ErrorCode, ModelError};
pub use ids::{FileId, Frn, VolumeId};
pub use limits::*;
pub use model::{FileItem, FILE_ATTRIBUTE_DIRECTORY};
