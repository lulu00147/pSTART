//! Hard protocol and resource limits.
//!
//! Every value here is an upper bound that must be enforced at the boundary
//! that receives the input. Runtime code must not multiply these limits
//! without documenting the resulting total budget.

/// Wire protocol version. This is independent of any Rust struct layout.
pub const PROTOCOL_VERSION: u16 = 2;

/// Maximum acceptable frame payload size, excluding the 4-byte length prefix.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Maximum query length in Unicode scalar values.
///
/// Manual input never exceeds 256 characters; keep the protocol bound at the
/// real interaction limit instead of 512.
pub const MAX_QUERY_CHARS: usize = 256;

/// Maximum query length in UTF-8 bytes, enforced before decoding work grows.
pub const MAX_QUERY_BYTES: usize = MAX_QUERY_CHARS * 4;

/// Maximum number of file results returned for one query.
pub const MAX_RESULTS: u32 = 200;

/// Maximum client version string length in bytes.
pub const MAX_CLIENT_VERSION_LEN: usize = 64;

/// Maximum server version string length in bytes.
pub const MAX_SERVER_VERSION_LEN: usize = 64;

/// Maximum volume identifier length in bytes.
pub const MAX_VOLUME_ID_LEN: usize = 64;

/// Maximum NTFS file name length in UTF-16 code units.
pub const MAX_FILE_NAME_UTF16: usize = 255;

/// Maximum displayed path length in UTF-16 code units.
pub const MAX_PATH_UTF16: usize = 4096;

/// Maximum number of plugin results accepted from one plugin call.
pub const MAX_PLUGIN_RESULTS: u32 = 50;

/// Maximum serialized plugin input accepted for one call.
pub const MAX_PLUGIN_INPUT_BYTES: usize = 64 * 1024;

/// Maximum serialized plugin output accepted from one plugin call.
pub const MAX_PLUGIN_OUTPUT_BYTES: usize = 64 * 1024;

/// Maximum concurrent IPC client sessions.
pub const MAX_IPC_CONNECTIONS: usize = 64;
