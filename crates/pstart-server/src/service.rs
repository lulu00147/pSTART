//! Server-side IPC connection handling.
//!
//! The transport only supplies framed bytes and an OS-verified peer identity.
//! This module owns the session handshake, request validation and query dispatch.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pstart_core::ErrorCode;
use pstart_index::SyncState;
use pstart_ipc::{
    decode_payload, encode_payload, IpcError, IpcMessage, Session, SessionAction, Transport,
    TransportKind,
};
use thiserror::Error;

use crate::engine::{SearchResponse, ServerEngine};
use crate::pipeline::VolumeSource;

/// Default query budget per connection window.
pub const DEFAULT_MAX_QUERIES_PER_WINDOW: u32 = 200;
/// Default rate window.
pub const DEFAULT_QUERY_WINDOW: Duration = Duration::from_secs(1);

/// Per-connection request limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Maximum queries accepted in one window.
    pub max_queries: u32,
    /// Window length.
    pub window: Duration,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_queries: DEFAULT_MAX_QUERIES_PER_WINDOW,
            window: DEFAULT_QUERY_WINDOW,
        }
    }
}

impl ConnectionLimits {
    /// Whether the limits are usable.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.max_queries != 0 && !self.window.is_zero()
    }

    fn validate(self) -> Result<(), ServiceError> {
        if !self.is_valid() {
            return Err(ServiceError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct QueryRate {
    started: Instant,
    used: u32,
}

fn rate_decision(used: u32, max_queries: u32, reset: bool) -> (bool, u32) {
    let used = if reset { 0 } else { used };
    if used >= max_queries {
        (false, used)
    } else {
        (true, used + 1)
    }
}

impl QueryRate {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            used: 0,
        }
    }

    fn allow(&mut self, now: Instant, limits: ConnectionLimits) -> bool {
        let reset = now.duration_since(self.started) >= limits.window;
        if reset {
            self.started = now;
        }
        let (allowed, used) = rate_decision(self.used, limits.max_queries, reset);
        self.used = used;
        allowed
    }
}

/// Query budget shared by every connection in one runtime.
#[derive(Clone)]
pub struct SharedQueryLimit {
    inner: Arc<Mutex<QueryRate>>,
    limits: ConnectionLimits,
}

impl SharedQueryLimit {
    /// Create a shared limiter.
    pub fn new(limits: ConnectionLimits) -> Result<Self, ServiceError> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(Mutex::new(QueryRate::new(Instant::now()))),
            limits,
        })
    }

    fn allow(&self) -> bool {
        self.inner
            .lock()
            .map(|mut rate| rate.allow(Instant::now(), self.limits))
            .unwrap_or(false)
    }
}

/// Query backend used by the IPC connection loop.
pub trait QueryBackend {
    /// Execute a validated query.
    fn search(&self, query: &str, limit: u32) -> Result<SearchResponse, ErrorCode>;

    /// Return a one-volume status snapshot.
    fn status(&self) -> Result<BackendStatus, ErrorCode>;
}

/// Status snapshot returned over IPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendStatus {
    /// Volume identifier.
    pub volume_id: String,
    /// Number of indexed files.
    pub indexed_files: u64,
    /// Whether synchronization is currently active.
    pub is_syncing: bool,
}

impl<S: VolumeSource> QueryBackend for ServerEngine<S> {
    fn search(&self, query: &str, limit: u32) -> Result<SearchResponse, ErrorCode> {
        ServerEngine::search(self, query, limit).map_err(|error| error.code())
    }

    fn status(&self) -> Result<BackendStatus, ErrorCode> {
        let status = ServerEngine::status(self);
        Ok(BackendStatus {
            volume_id: self.volume_id().to_owned(),
            indexed_files: status.indexed_files as u64,
            is_syncing: !matches!(status.sync_state, SyncState::LiveSync | SyncState::Stopped),
        })
    }
}

/// Service loop failure.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Framing or encoding failure.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// Invalid per-connection limits.
    #[error("connection limits must have a positive window and query count")]
    InvalidLimits,
}

/// Counters returned when a connection ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServeStats {
    /// Client frames received.
    pub received: usize,
    /// Server frames sent.
    pub sent: usize,
}

/// Serve one already-accepted transport until it closes or the session ends.
///
/// The peer identity must come from the transport (OS-level), never from a
/// client message. `session_id` is for log correlation only.
pub fn serve_connection(
    transport: &mut dyn Transport,
    backend: &dyn QueryBackend,
    session_id: u64,
    server_version: &str,
) -> Result<ServeStats, ServiceError> {
    serve_connection_with_limits(
        transport,
        backend,
        session_id,
        server_version,
        ConnectionLimits::default(),
    )
}

/// Serve one connection with explicit per-connection query limits.
pub fn serve_connection_with_limits(
    transport: &mut dyn Transport,
    backend: &dyn QueryBackend,
    session_id: u64,
    server_version: &str,
    limits: ConnectionLimits,
) -> Result<ServeStats, ServiceError> {
    serve_connection_with_global_limit(transport, backend, session_id, server_version, limits, None)
}

/// Serve one connection with per-connection and optional runtime-wide limits.
pub fn serve_connection_with_global_limit(
    transport: &mut dyn Transport,
    backend: &dyn QueryBackend,
    session_id: u64,
    server_version: &str,
    limits: ConnectionLimits,
    global: Option<&SharedQueryLimit>,
) -> Result<ServeStats, ServiceError> {
    limits.validate()?;
    let peer = transport.peer_identity()?;
    let mut session = Session::new(peer, session_id, server_version)?;
    let mut stats = ServeStats::default();
    let mut rate = QueryRate::new(Instant::now());

    loop {
        let payload = match transport.receive_frame() {
            Ok(payload) => payload,
            Err(IpcError::Closed) => break,
            Err(error) => return Err(ServiceError::Ipc(error)),
        };
        stats.received += 1;

        let message = match decode_payload(&payload) {
            Ok(message) => message,
            Err(error) => {
                send_error(transport, None, error.code(), &mut stats)?;
                break;
            }
        };

        match session
            .handle(message)
            .unwrap_or(SessionAction::Close(None))
        {
            SessionAction::Reply(reply) => {
                send(transport, &reply, &mut stats)?;
            }
            SessionAction::DispatchQuery {
                request_id,
                text,
                limit,
            } => {
                let global_allowed = global.is_none_or(SharedQueryLimit::allow);
                if !global_allowed || !rate.allow(Instant::now(), limits) {
                    send_error(
                        transport,
                        Some(request_id),
                        ErrorCode::ResourceExhausted,
                        &mut stats,
                    )?;
                    break;
                }
                match backend.search(&text, limit) {
                    Ok(response) => {
                        send(
                            transport,
                            &IpcMessage::QueryResult {
                                request_id,
                                items: response.items,
                                truncated: response.truncated,
                            },
                            &mut stats,
                        )?;
                    }
                    Err(code) => {
                        send_error(transport, Some(request_id), code, &mut stats)?;
                    }
                }
            }
            SessionAction::DispatchStatus { request_id } => match backend.status() {
                Ok(status) => {
                    send(
                        transport,
                        &IpcMessage::StatusResult {
                            request_id,
                            volume_id: status.volume_id,
                            indexed_files: status.indexed_files,
                            is_syncing: status.is_syncing,
                        },
                        &mut stats,
                    )?;
                }
                Err(code) => {
                    send_error(transport, Some(request_id), code, &mut stats)?;
                }
            },
            SessionAction::Close(reply) => {
                reply
                    .map(|reply| send(transport, &reply, &mut stats))
                    .transpose()?;
                transport.close()?;
                break;
            }
        }
    }
    Ok(stats)
}

fn send(
    transport: &mut dyn Transport,
    message: &IpcMessage,
    stats: &mut ServeStats,
) -> Result<(), ServiceError> {
    let payload = encode_payload(message)?;
    transport.send_frame(&payload)?;
    stats.sent += 1;
    Ok(())
}

fn send_error(
    transport: &mut dyn Transport,
    request_id: Option<u64>,
    code: ErrorCode,
    stats: &mut ServeStats,
) -> Result<(), ServiceError> {
    send(
        transport,
        &IpcMessage::Error {
            request_id,
            code: code.as_u32(),
        },
        stats,
    )
}

/// Helper for tests and local callers: an in-memory session peer.
pub fn local_test_peer(pid: u32) -> Result<pstart_ipc::PeerIdentity, IpcError> {
    pstart_ipc::PeerIdentity::new(pid, "S-1-5-21-pstart-test", TransportKind::InMemory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::{FileId, FileItem, Frn, VolumeId};
    use pstart_ipc::{MemoryTransport, PeerIdentity};

    struct FakeBackend {
        response: Result<SearchResponse, ErrorCode>,
    }

    impl QueryBackend for FakeBackend {
        fn search(&self, _query: &str, _limit: u32) -> Result<SearchResponse, ErrorCode> {
            self.response.clone()
        }

        fn status(&self) -> Result<BackendStatus, ErrorCode> {
            Ok(BackendStatus {
                volume_id: "C:".to_owned(),
                indexed_files: 0,
                is_syncing: false,
            })
        }
    }

    fn item(name: &str) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(10)),
            name: name.into(),
            path: format!("C:\\{name}"),
            attributes: 0,
            is_dir: false,
        }
    }

    fn transport_with(messages: &[IpcMessage]) -> MemoryTransport {
        let peer = PeerIdentity::new(42, "S-1-5-21-test", TransportKind::InMemory).unwrap();
        let mut transport = MemoryTransport::new(peer);
        for message in messages {
            transport.push_incoming(encode_payload(message).unwrap());
        }
        transport
    }

    fn sent_messages(transport: &MemoryTransport) -> Vec<IpcMessage> {
        transport
            .sent()
            .iter()
            .map(|payload| decode_payload(payload).unwrap())
            .collect()
    }

    struct FailingTransport {
        inner: MemoryTransport,
        fail_on: usize,
        sends: usize,
    }

    impl Transport for FailingTransport {
        fn kind(&self) -> TransportKind {
            self.inner.kind()
        }

        fn send_frame(&mut self, payload: &[u8]) -> Result<(), IpcError> {
            self.sends += 1;
            if self.sends == self.fail_on {
                return Err(IpcError::Closed);
            }
            self.inner.send_frame(payload)
        }

        fn receive_frame(&mut self) -> Result<Vec<u8>, IpcError> {
            self.inner.receive_frame()
        }

        fn peer_identity(&self) -> Result<pstart_ipc::PeerIdentity, IpcError> {
            self.inner.peer_identity()
        }

        fn close(&mut self) -> Result<(), IpcError> {
            self.inner.close()
        }
    }

    #[test]
    fn send_failures_are_propagated() {
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        let mut transport = FailingTransport {
            inner: transport_with(&[
                IpcMessage::Hello {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    client_version: "0.1.0".to_owned(),
                },
                IpcMessage::Query {
                    request_id: 1,
                    text: "a".to_owned(),
                    limit: 10,
                },
            ]),
            fail_on: 2,
            sends: 0,
        };
        assert_eq!(transport.kind(), TransportKind::InMemory);
        assert!(matches!(
            serve_connection(&mut transport, &backend, 1, "0.1.0"),
            Err(ServiceError::Ipc(_))
        ));
        transport.close().unwrap();

        let mut limited = FailingTransport {
            inner: transport_with(&[
                IpcMessage::Hello {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    client_version: "0.1.0".to_owned(),
                },
                IpcMessage::Query {
                    request_id: 1,
                    text: "a".to_owned(),
                    limit: 10,
                },
                IpcMessage::Query {
                    request_id: 2,
                    text: "b".to_owned(),
                    limit: 10,
                },
            ]),
            fail_on: 3,
            sends: 0,
        };
        assert!(matches!(
            serve_connection_with_limits(
                &mut limited,
                &backend,
                1,
                "0.1.0",
                ConnectionLimits {
                    max_queries: 1,
                    window: Duration::from_secs(60),
                },
            ),
            Err(ServiceError::Ipc(_))
        ));
    }

    #[test]
    fn duplicate_hello_terminates_connection() {
        let hello = IpcMessage::Hello {
            protocol_version: pstart_core::PROTOCOL_VERSION,
            client_version: "0.1.0".to_owned(),
        };
        let mut transport = transport_with(&[hello.clone(), hello]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        assert_eq!(sent_messages(&transport).len(), 2);
    }

    #[test]
    fn query_before_hello_is_rejected_without_hanging() {
        let mut transport = transport_with(&[IpcMessage::Query {
            request_id: 1,
            text: "a".to_owned(),
            limit: 10,
        }]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        assert!(matches!(
            sent_messages(&transport).first(),
            Some(IpcMessage::Error { .. })
        ));
    }

    #[test]
    fn shared_query_limit_spans_connections() {
        let global = SharedQueryLimit::new(ConnectionLimits {
            max_queries: 1,
            window: Duration::from_secs(60),
        })
        .unwrap();
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        let mut first = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Query {
                request_id: 1,
                text: "a".to_owned(),
                limit: 10,
            },
        ]);
        serve_connection_with_global_limit(
            &mut first,
            &backend,
            1,
            "0.1.0",
            ConnectionLimits::default(),
            Some(&global),
        )
        .unwrap();
        assert!(matches!(
            sent_messages(&first)[1],
            IpcMessage::QueryResult { request_id: 1, .. }
        ));

        let mut second = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Query {
                request_id: 2,
                text: "b".to_owned(),
                limit: 10,
            },
        ]);
        serve_connection_with_global_limit(
            &mut second,
            &backend,
            2,
            "0.1.0",
            ConnectionLimits::default(),
            Some(&global),
        )
        .unwrap();
        assert_eq!(
            sent_messages(&second)[1],
            IpcMessage::Error {
                request_id: Some(2),
                code: ErrorCode::ResourceExhausted.as_u32(),
            }
        );
    }

    #[test]
    fn query_rate_limit_rejects_excess_requests() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Query {
                request_id: 1,
                text: "a".to_owned(),
                limit: 10,
            },
            IpcMessage::Query {
                request_id: 2,
                text: "b".to_owned(),
                limit: 10,
            },
        ]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        let limits = ConnectionLimits {
            max_queries: 1,
            window: Duration::from_secs(60),
        };
        let stats =
            serve_connection_with_limits(&mut transport, &backend, 1, "0.1.0", limits).unwrap();
        assert_eq!(stats.received, 3);
        let sent = sent_messages(&transport);
        assert!(matches!(sent[0], IpcMessage::HelloAck { .. }));
        assert!(matches!(
            sent[1],
            IpcMessage::QueryResult { request_id: 1, .. }
        ));
        assert_eq!(
            sent[2],
            IpcMessage::Error {
                request_id: Some(2),
                code: ErrorCode::ResourceExhausted.as_u32()
            }
        );
    }

    #[test]
    fn invalid_connection_limits_are_rejected() {
        let mut transport = transport_with(&[]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        let error = serve_connection_with_limits(
            &mut transport,
            &backend,
            1,
            "0.1.0",
            ConnectionLimits {
                max_queries: 0,
                window: Duration::from_secs(1),
            },
        )
        .unwrap_err();
        assert!(matches!(error, ServiceError::InvalidLimits));
    }

    #[test]
    fn rate_window_resets_after_elapsed_time() {
        let start = Instant::now();
        let limits = ConnectionLimits {
            max_queries: 2,
            window: Duration::from_millis(100),
        };
        let mut rate = QueryRate::new(start);
        assert!(rate.allow(start, limits));
        assert!(rate.allow(start + Duration::from_millis(50), limits));
        assert!(!rate.allow(start + Duration::from_millis(80), limits));
        assert!(rate.allow(start + Duration::from_millis(101), limits));
    }

    #[test]
    fn handshake_and_query_are_served() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Query {
                request_id: 7,
                text: "a".to_owned(),
                limit: 10,
            },
        ]);
        let backend = FakeBackend {
            response: Ok(SearchResponse {
                items: vec![item("a.txt")],
                truncated: true,
            }),
        };
        let stats = serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        assert_eq!(stats.received, 2);
        let sent = sent_messages(&transport);
        assert!(matches!(sent[0], IpcMessage::HelloAck { .. }));
        assert!(matches!(
            &sent[1],
            IpcMessage::QueryResult {
                request_id,
                items,
                truncated: true,
            } if *request_id == 7 && items.len() == 1
        ));
    }

    #[test]
    fn duplicate_hello_sends_error_and_closes() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
        ]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        assert!(transport.is_closed());
        let sent = sent_messages(&transport);
        assert!(matches!(sent[1], IpcMessage::Error { .. }));
    }

    #[test]
    fn status_request_returns_snapshot() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::StatusRequest { request_id: 8 },
        ]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        let sent = sent_messages(&transport);
        assert_eq!(
            sent[1],
            IpcMessage::StatusResult {
                request_id: 8,
                volume_id: "C:".to_owned(),
                indexed_files: 0,
                is_syncing: false,
            }
        );
    }

    #[test]
    fn status_send_failure_is_propagated() {
        let mut transport = FailingTransport {
            inner: transport_with(&[
                IpcMessage::Hello {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    client_version: "0.1.0".to_owned(),
                },
                IpcMessage::StatusRequest { request_id: 12 },
            ]),
            fail_on: 2,
            sends: 0,
        };
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        assert!(matches!(
            serve_connection(&mut transport, &backend, 1, "0.1.0"),
            Err(ServiceError::Ipc(_))
        ));
    }

    #[test]
    fn status_failure_is_reported() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::StatusRequest { request_id: 11 },
        ]);
        struct Failing;
        impl QueryBackend for Failing {
            fn search(&self, _query: &str, _limit: u32) -> Result<SearchResponse, ErrorCode> {
                Ok(SearchResponse::default())
            }

            fn status(&self) -> Result<BackendStatus, ErrorCode> {
                Err(ErrorCode::ServerBusy)
            }
        }
        assert!(Failing.search("x", 1).is_ok());
        serve_connection(&mut transport, &Failing, 1, "0.1.0").unwrap();
        let sent = sent_messages(&transport);
        assert_eq!(
            sent[1],
            IpcMessage::Error {
                request_id: Some(11),
                code: ErrorCode::ServerBusy.as_u32(),
            }
        );
    }

    #[test]
    fn query_before_hello_is_rejected() {
        let mut transport = transport_with(&[IpcMessage::Query {
            request_id: 9,
            text: "x".to_owned(),
            limit: 10,
        }]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        let sent = sent_messages(&transport);
        assert_eq!(
            sent[0],
            IpcMessage::Error {
                request_id: Some(9),
                code: ErrorCode::Unauthenticated.as_u32()
            }
        );
    }

    #[test]
    fn backend_error_is_returned_with_request_id() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Query {
                request_id: 11,
                text: "x".to_owned(),
                limit: 10,
            },
        ]);
        let backend = FakeBackend {
            response: Err(ErrorCode::ServerBusy),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        let sent = sent_messages(&transport);
        assert_eq!(
            sent[1],
            IpcMessage::Error {
                request_id: Some(11),
                code: ErrorCode::ServerBusy.as_u32()
            }
        );
    }

    #[test]
    fn malformed_frame_produces_invalid_frame_error() {
        let peer = PeerIdentity::new(42, "S-1-5-21-test", TransportKind::InMemory).unwrap();
        let mut transport = MemoryTransport::new(peer);
        transport.push_incoming(vec![0xff, 0xff]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        let sent = sent_messages(&transport);
        assert!(matches!(sent[0], IpcMessage::Error { code: 1, .. }));
    }

    #[test]
    fn protocol_violation_closes_transport() {
        let mut transport = transport_with(&[
            IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::HelloAck {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                server_version: "0.1.0".to_owned(),
                session_id: 1,
            },
        ]);
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        serve_connection(&mut transport, &backend, 1, "0.1.0").unwrap();
        assert!(transport.is_closed());
        let sent = sent_messages(&transport);
        assert!(matches!(sent[1], IpcMessage::Error { code: 4, .. }));
    }

    #[test]
    fn local_test_peer_is_valid() {
        assert!(local_test_peer(1).is_ok());
        assert!(local_test_peer(0).is_err());
    }

    struct ReceiveFailTransport {
        peer: PeerIdentity,
    }

    impl Transport for ReceiveFailTransport {
        fn kind(&self) -> TransportKind {
            TransportKind::InMemory
        }

        fn send_frame(&mut self, _payload: &[u8]) -> Result<(), IpcError> {
            Ok(())
        }

        fn receive_frame(&mut self) -> Result<Vec<u8>, IpcError> {
            Err(IpcError::Io(std::io::Error::other(
                "injected receive failure",
            )))
        }

        fn peer_identity(&self) -> Result<PeerIdentity, IpcError> {
            Ok(self.peer.clone())
        }

        fn close(&mut self) -> Result<(), IpcError> {
            Ok(())
        }
    }

    #[test]
    fn receive_error_is_propagated() {
        let peer = PeerIdentity::new(42, "S-1-5-21-test", TransportKind::InMemory).unwrap();
        let mut transport = ReceiveFailTransport { peer };
        assert_eq!(transport.kind(), TransportKind::InMemory);
        assert!(transport.send_frame(b"x").is_ok());
        assert!(transport.close().is_ok());
        let backend = FakeBackend {
            response: Ok(SearchResponse::default()),
        };
        assert!(matches!(
            serve_connection(&mut transport, &backend, 1, "0.1.0"),
            Err(ServiceError::Ipc(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn query_over_real_unix_socket() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::thread;

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(format!(
            "pstart-service-test-{}-{}.sock",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = pstart_ipc::UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let mut transport = listener.accept().unwrap();
            let backend = FakeBackend {
                response: Ok(SearchResponse {
                    items: vec![item("socket.txt")],
                    truncated: false,
                }),
            };
            let _ = serve_connection(&mut transport, &backend, 1, "0.1.0");
        });

        let mut client = pstart_ipc::UnixClient::connect(&path).unwrap();
        client
            .send(&IpcMessage::Hello {
                protocol_version: pstart_core::PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::HelloAck { .. }
        ));
        client
            .send(&IpcMessage::Query {
                request_id: 21,
                text: "socket".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert!(matches!(
            client.receive().unwrap(),
            IpcMessage::QueryResult {
                request_id: 21,
                items,
                truncated: false,
            } if items.len() == 1
        ));
        drop(client);
        let _ = server.join();
    }
}
