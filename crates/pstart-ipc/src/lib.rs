//! pSTART IPC wire protocol and session layer.
//!
//! The transport moves opaque frames. Framing, decoding, validation and session
//! authentication are implemented here and are independent of Windows APIs.

pub mod error;
pub mod frame;
pub mod message;
pub mod session;
pub mod transport;

#[cfg(windows)]
pub mod af_unix;

pub use error::IpcError;
pub use frame::{
    decode_frame, decode_payload, encode_frame, encode_payload, read_frame, read_payload,
    write_frame,
};
pub use message::IpcMessage;
pub use session::{Session, SessionAction, SessionState};
pub use transport::{
    MemoryTransport, PeerIdentity, StreamTransport, Transport, TransportKind, MAX_PEER_USER_LEN,
};

#[cfg(windows)]
pub use af_unix::{UnixClient, UnixListener, UnixStream};
