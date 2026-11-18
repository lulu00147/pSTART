//! Transport abstraction.
//!
//! Transports move opaque framed payloads. Authentication inputs (peer PID and
//! OS user identity) are supplied by the transport implementation, never by the
//! client's Hello message.

use std::io::{Read, Write};

use pstart_core::limits::MAX_FRAME_LEN;

use crate::error::IpcError;
use crate::frame::{effective_frame_limit, read_payload};

/// Maximum accepted peer user identifier length.
pub const MAX_PEER_USER_LEN: usize = 256;

/// Logical transport kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    /// Windows AF_UNIX socket.
    UnixSocket,
    /// In-memory test transport.
    InMemory,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixSocket => formatter.write_str("af_unix"),
            Self::InMemory => formatter.write_str("in_memory"),
        }
    }
}

/// OS-verified identity of the connected peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Process ID obtained from the OS transport.
    pub pid: u32,
    /// OS user identifier (SID string on Windows, uid on Unix).
    pub user: String,
    /// Transport that supplied this identity.
    pub transport: TransportKind,
}

impl PeerIdentity {
    /// Construct and validate a peer identity.
    pub fn new(
        pid: u32,
        user: impl Into<String>,
        transport: TransportKind,
    ) -> Result<Self, IpcError> {
        let user = user.into();
        if pid == 0 {
            return Err(IpcError::InvalidPeer("pid must be non-zero".to_owned()));
        }
        if user.is_empty() {
            return Err(IpcError::InvalidPeer("user must not be empty".to_owned()));
        }
        if user.len() > MAX_PEER_USER_LEN {
            return Err(IpcError::InvalidPeer(format!(
                "user is {} bytes, maximum is {MAX_PEER_USER_LEN}",
                user.len()
            )));
        }
        if user.chars().any(char::is_control) {
            return Err(IpcError::InvalidPeer(
                "user contains control characters".to_owned(),
            ));
        }
        Ok(Self {
            pid,
            user,
            transport,
        })
    }
}

/// Opaque, bounded frame transport.
pub trait Transport: Send {
    /// Transport kind.
    fn kind(&self) -> TransportKind;

    /// Send one payload. The implementation adds/removes framing.
    fn send_frame(&mut self, payload: &[u8]) -> Result<(), IpcError>;

    /// Receive one payload, enforcing the frame limit before allocation.
    fn receive_frame(&mut self) -> Result<Vec<u8>, IpcError>;

    /// Return the OS-verified peer identity.
    fn peer_identity(&self) -> Result<PeerIdentity, IpcError>;

    /// Close the transport. Implementations should be idempotent.
    fn close(&mut self) -> Result<(), IpcError>;
}

/// Length-prefixed stream transport over any `Read + Write`.
#[derive(Debug)]
pub struct StreamTransport<S> {
    stream: S,
    peer: PeerIdentity,
    max_frame_len: usize,
}

impl<S: Read + Write> StreamTransport<S> {
    /// Wrap a stream. `max_frame_len` may only lower the global limit.
    pub fn new(stream: S, peer: PeerIdentity, max_frame_len: usize) -> Result<Self, IpcError> {
        let max_frame_len = effective_frame_limit(max_frame_len);
        if max_frame_len == 0 {
            return Err(IpcError::InvalidMessage(
                "max_frame_len must be non-zero".to_owned(),
            ));
        }
        Ok(Self {
            stream,
            peer,
            max_frame_len,
        })
    }

    /// Borrow the underlying stream.
    pub fn stream(&self) -> &S {
        &self.stream
    }

    /// Mutably borrow the underlying stream.
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }
}

impl<S: Read + Write + Send> Transport for StreamTransport<S> {
    fn kind(&self) -> TransportKind {
        self.peer.transport
    }

    fn send_frame(&mut self, payload: &[u8]) -> Result<(), IpcError> {
        if payload.is_empty() {
            return Err(IpcError::EmptyFrame);
        }
        if payload.len() > self.max_frame_len {
            return Err(IpcError::FrameTooLarge {
                actual: payload.len(),
                max: self.max_frame_len,
            });
        }
        // The constructor caps the limit at MAX_FRAME_LEN, so this cast cannot
        // truncate for any payload that passed the length check above.
        let length = payload.len() as u32;
        self.stream.write_all(&length.to_le_bytes())?;
        self.stream.write_all(payload)?;
        self.stream.flush()?;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Vec<u8>, IpcError> {
        read_payload(&mut self.stream, self.max_frame_len)
    }

    fn peer_identity(&self) -> Result<PeerIdentity, IpcError> {
        Ok(self.peer.clone())
    }

    fn close(&mut self) -> Result<(), IpcError> {
        self.stream.flush()?;
        Ok(())
    }
}

/// Deterministic in-memory transport used by tests and local integration tests.
#[derive(Debug)]
pub struct MemoryTransport {
    peer: PeerIdentity,
    sent: Vec<Vec<u8>>,
    incoming: std::collections::VecDeque<Vec<u8>>,
    closed: bool,
    max_frame_len: usize,
}

impl MemoryTransport {
    /// Create a transport with an empty inbound queue.
    #[must_use]
    pub fn new(peer: PeerIdentity) -> Self {
        Self {
            peer,
            sent: Vec::new(),
            incoming: std::collections::VecDeque::new(),
            closed: false,
            max_frame_len: MAX_FRAME_LEN,
        }
    }

    /// Queue one inbound payload.
    pub fn push_incoming(&mut self, payload: Vec<u8>) {
        self.incoming.push_back(payload);
    }

    /// Return payloads sent through this transport.
    #[must_use]
    pub fn sent(&self) -> &[Vec<u8>] {
        &self.sent
    }

    /// Whether the transport has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Transport for MemoryTransport {
    fn kind(&self) -> TransportKind {
        self.peer.transport
    }

    fn send_frame(&mut self, payload: &[u8]) -> Result<(), IpcError> {
        if self.closed {
            return Err(IpcError::Closed);
        }
        if payload.is_empty() {
            return Err(IpcError::EmptyFrame);
        }
        if payload.len() > self.max_frame_len {
            return Err(IpcError::FrameTooLarge {
                actual: payload.len(),
                max: self.max_frame_len,
            });
        }
        self.sent.push(payload.to_vec());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Vec<u8>, IpcError> {
        if self.closed {
            return Err(IpcError::Closed);
        }
        self.incoming.pop_front().ok_or(IpcError::Closed)
    }

    fn peer_identity(&self) -> Result<PeerIdentity, IpcError> {
        Ok(self.peer.clone())
    }

    fn close(&mut self) -> Result<(), IpcError> {
        self.closed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn peer() -> PeerIdentity {
        PeerIdentity::new(1234, "S-1-5-21-test", TransportKind::UnixSocket).unwrap()
    }

    #[test]
    fn peer_identity_validation() {
        assert!(PeerIdentity::new(0, "user", TransportKind::UnixSocket).is_err());
        assert!(PeerIdentity::new(1, "", TransportKind::UnixSocket).is_err());
        let too_long = "x".repeat(MAX_PEER_USER_LEN + 1);
        assert!(PeerIdentity::new(1, too_long, TransportKind::UnixSocket).is_err());
        assert!(PeerIdentity::new(1, "a\nb", TransportKind::UnixSocket).is_err());
        assert!(PeerIdentity::new(1, "user", TransportKind::UnixSocket).is_ok());
    }

    #[test]
    fn memory_transport_enforces_state() {
        let mut transport = MemoryTransport::new(peer());
        transport.send_frame(b"frame").unwrap();
        assert_eq!(transport.sent(), &[b"frame".to_vec()]);
        assert!(matches!(transport.receive_frame(), Err(IpcError::Closed)));
        transport.push_incoming(b"inbound".to_vec());
        assert_eq!(transport.receive_frame().unwrap(), b"inbound");
        transport.close().unwrap();
        assert!(transport.is_closed());
        assert!(matches!(transport.send_frame(b"x"), Err(IpcError::Closed)));
        assert!(matches!(transport.receive_frame(), Err(IpcError::Closed)));
    }

    #[test]
    fn memory_transport_rejects_empty_and_oversized_frames() {
        let mut transport = MemoryTransport::new(peer());
        assert!(matches!(
            transport.send_frame(&[]),
            Err(IpcError::EmptyFrame)
        ));
        let oversized = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(
            transport.send_frame(&oversized),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn stream_transport_adds_length_prefix() {
        let writer = Cursor::new(Vec::new());
        let mut transport = StreamTransport::new(writer, peer(), MAX_FRAME_LEN).unwrap();
        transport.send_frame(b"abc").unwrap();
        let bytes = transport.stream().get_ref().clone();
        assert_eq!(&bytes[..4], &3u32.to_le_bytes());
        assert_eq!(&bytes[4..], b"abc");
    }

    #[test]
    fn stream_transport_reads_length_prefixed_payload() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"abc");
        let mut transport =
            StreamTransport::new(Cursor::new(bytes), peer(), MAX_FRAME_LEN).unwrap();
        assert_eq!(transport.receive_frame().unwrap(), b"abc");
    }

    #[test]
    fn configured_frame_limit_is_capped() {
        let transport = StreamTransport::new(Cursor::new(Vec::new()), peer(), usize::MAX).unwrap();
        assert_eq!(transport.max_frame_len, MAX_FRAME_LEN);
    }

    #[test]
    fn transport_kind_display_is_stable() {
        assert_eq!(TransportKind::UnixSocket.to_string(), "af_unix");
        assert_eq!(TransportKind::InMemory.to_string(), "in_memory");
    }

    #[test]
    fn zero_frame_limit_is_rejected() {
        assert!(matches!(
            StreamTransport::new(Cursor::new(Vec::new()), peer(), 0),
            Err(IpcError::InvalidMessage(_))
        ));
    }

    #[test]
    fn stream_transport_exposes_kind_peer_and_mutable_stream() {
        let mut transport =
            StreamTransport::new(Cursor::new(Vec::new()), peer(), MAX_FRAME_LEN).unwrap();
        assert_eq!(transport.kind(), TransportKind::UnixSocket);
        assert_eq!(transport.peer_identity().unwrap(), peer());
        transport.stream_mut().get_mut().extend_from_slice(b"x");
        assert_eq!(transport.stream().get_ref(), b"x");
        transport.close().unwrap();
    }

    #[test]
    fn stream_transport_rejects_empty_and_oversized_frames() {
        let mut transport = StreamTransport::new(Cursor::new(Vec::new()), peer(), 4).unwrap();
        assert!(matches!(
            transport.send_frame(&[]),
            Err(IpcError::EmptyFrame)
        ));
        assert!(matches!(
            transport.send_frame(b"12345"),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn stream_transport_reports_truncated_input() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.push(b'a');
        let mut transport =
            StreamTransport::new(Cursor::new(bytes), peer(), MAX_FRAME_LEN).unwrap();
        assert!(matches!(
            transport.receive_frame(),
            Err(IpcError::TruncatedPayload {
                actual: 1,
                expected: 3
            })
        ));
    }

    #[test]
    fn memory_transport_exposes_kind_and_peer() {
        let transport = MemoryTransport::new(peer());
        assert_eq!(transport.kind(), TransportKind::UnixSocket);
        assert_eq!(transport.peer_identity().unwrap(), peer());
    }

    struct FlushFailure;

    impl Read for FlushFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for FlushFailure {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }

    #[test]
    fn stream_transport_close_surfaces_flush_failure() {
        let mut failure = FlushFailure;
        let mut buffer = [0u8; 1];
        assert_eq!(failure.read(&mut buffer).unwrap(), 0);
        assert_eq!(failure.write(b"x").unwrap(), 1);
        let mut transport = StreamTransport::new(FlushFailure, peer(), MAX_FRAME_LEN).unwrap();
        assert!(matches!(transport.close(), Err(IpcError::Io(_))));
    }
}
