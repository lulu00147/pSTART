//! Host-side AF_UNIX query client.

use std::path::Path;
use std::time::Duration;

use pstart_core::limits::{MAX_QUERY_CHARS, MAX_RESULTS};
use pstart_core::{FileItem, PROTOCOL_VERSION};
use pstart_ipc::{IpcError, IpcMessage, UnixClient};

/// Default timeout for handshake and each query response.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Query result returned to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientResponse {
    /// Bounded result list.
    pub items: Vec<FileItem>,
    /// Server stopped early because of a result or time budget.
    pub truncated: bool,
}

/// Host client failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Transport/framing failure.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// Server rejected the request with a stable code.
    #[error("server error code {0}")]
    Server(u32),
    /// Unexpected protocol message.
    #[error("unexpected protocol message: {0}")]
    Unexpected(String),
    /// Server negotiated a different protocol version.
    #[error("protocol version mismatch: server {0}")]
    Protocol(u16),
    /// Invalid local request.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// Request identifier overflow.
    #[error("request id exhausted")]
    RequestIdExhausted,
    /// Socket option setup failed.
    #[error("socket setup failed: {0}")]
    Io(#[from] std::io::Error),
}

impl ClientError {
    /// Whether the server is unreachable or the socket failed.
    #[must_use]
    pub fn is_offline(&self) -> bool {
        match self {
            Self::Ipc(IpcError::Io(_)) | Self::Io(_) => true,
            Self::Ipc(_)
            | Self::Server(_)
            | Self::Unexpected(_)
            | Self::Protocol(_)
            | Self::InvalidQuery(_)
            | Self::RequestIdExhausted => false,
        }
    }
}

/// Connected host client.
pub struct Client {
    stream: UnixClient,
    next_request_id: u64,
    session_id: u64,
}

/// One-volume status snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusResponse {
    /// Volume identifier.
    pub volume_id: String,
    /// Number of indexed files.
    pub indexed_files: u64,
    /// Whether synchronization is currently active.
    pub is_syncing: bool,
}

impl Client {
    /// Connect, negotiate the protocol, and return a ready client.
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        Self::connect_with_timeout(path, DEFAULT_QUERY_TIMEOUT)
    }

    /// Connect with an explicit read timeout.
    pub fn connect_with_timeout(path: &Path, timeout: Duration) -> Result<Self, ClientError> {
        Self::connect_version(path, timeout, PROTOCOL_VERSION)
    }

    /// Connect using an explicit protocol version, for compatibility probes.
    pub fn connect_with_protocol_version(
        path: &Path,
        protocol_version: u16,
    ) -> Result<Self, ClientError> {
        Self::connect_version(path, DEFAULT_QUERY_TIMEOUT, protocol_version)
    }

    fn connect_version(
        path: &Path,
        timeout: Duration,
        protocol_version: u16,
    ) -> Result<Self, ClientError> {
        let mut stream = UnixClient::connect(path)?;
        stream.set_read_timeout(timeout)?;
        stream.send(&IpcMessage::Hello {
            protocol_version,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        })?;
        match stream.receive()? {
            IpcMessage::HelloAck {
                protocol_version: ack_version,
                session_id,
                ..
            } => {
                if ack_version != protocol_version {
                    return Err(ClientError::Protocol(ack_version));
                }
                Ok(Self {
                    stream,
                    next_request_id: 1,
                    session_id,
                })
            }
            IpcMessage::Error { code, .. } => Err(ClientError::Server(code)),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Opaque server session identifier for logging.
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Request the server volume status.
    pub fn status(&mut self) -> Result<StatusResponse, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ClientError::RequestIdExhausted)?;
        self.stream
            .send(&IpcMessage::StatusRequest { request_id })?;
        match self.stream.receive()? {
            IpcMessage::StatusResult {
                request_id: response_id,
                volume_id,
                indexed_files,
                is_syncing,
            } if response_id == request_id => Ok(StatusResponse {
                volume_id,
                indexed_files,
                is_syncing,
            }),
            IpcMessage::Error {
                request_id: Some(response_id),
                code,
            } if response_id == request_id => Err(ClientError::Server(code)),
            IpcMessage::Error {
                request_id: None,
                code,
            } => Err(ClientError::Server(code)),
            other => Err(ClientError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Execute one bounded query.
    pub fn query(&mut self, text: &str, limit: u32) -> Result<ClientResponse, ClientError> {
        validate_query(text, limit)?;
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(ClientError::RequestIdExhausted)?;
        self.stream.send(&IpcMessage::Query {
            request_id,
            text: text.to_owned(),
            limit,
        })?;
        loop {
            match self.stream.receive()? {
                IpcMessage::QueryResult {
                    request_id: response_id,
                    items,
                    truncated,
                } if response_id == request_id => {
                    return Ok(ClientResponse { items, truncated });
                }
                IpcMessage::Error {
                    request_id: Some(response_id),
                    code,
                } if response_id == request_id => return Err(ClientError::Server(code)),
                IpcMessage::Error {
                    request_id: None,
                    code,
                } => return Err(ClientError::Server(code)),
                IpcMessage::UsnSyncStatus { .. } => {}
                other => return Err(ClientError::Unexpected(format!("{other:?}"))),
            }
        }
    }
}

fn validate_query(text: &str, limit: u32) -> Result<(), ClientError> {
    if limit == 0 || limit > MAX_RESULTS {
        return Err(ClientError::InvalidQuery(format!(
            "limit {limit} is outside 1..={MAX_RESULTS}"
        )));
    }
    if text.chars().count() > MAX_QUERY_CHARS {
        return Err(ClientError::InvalidQuery(format!(
            "query exceeds {MAX_QUERY_CHARS} characters"
        )));
    }
    if text.contains('\0') {
        return Err(ClientError::InvalidQuery(
            "query text contains NUL".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::{ErrorCode, FileId, Frn, VolumeId};
    use pstart_ipc::frame::{decode_payload, encode_payload};
    use pstart_ipc::{Transport, UnixListener};
    use pstart_server::service::{serve_connection, BackendStatus, QueryBackend};
    use pstart_server::SearchResponse;
    use std::path::Path;
    use std::thread;
    use tempfile::TempDir;

    fn scripted(path: &Path, replies: Vec<IpcMessage>) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let mut transport = listener.accept().unwrap();
            let _ = decode_payload(&transport.receive_frame().unwrap()).unwrap();
            for (index, reply) in replies.iter().enumerate() {
                if index == 1 {
                    let _ = decode_payload(&transport.receive_frame().unwrap()).unwrap();
                }
                transport
                    .send_frame(&encode_payload(reply).unwrap())
                    .unwrap();
            }
        })
    }

    struct Backend;

    impl QueryBackend for Backend {
        fn search(&self, query: &str, _limit: u32) -> Result<SearchResponse, ErrorCode> {
            if query == "fail" {
                return Err(ErrorCode::ServerBusy);
            }
            Ok(SearchResponse {
                items: vec![FileItem {
                    id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(12)),
                    name: "a.txt".to_owned(),
                    path: "C:\\a.txt".to_owned(),
                    attributes: 0,
                    is_dir: false,
                }],
                truncated: false,
            })
        }

        fn status(&self) -> Result<BackendStatus, ErrorCode> {
            Ok(BackendStatus {
                volume_id: "C:".to_owned(),
                indexed_files: 1,
                is_syncing: false,
            })
        }
    }

    fn start_server(path: &Path) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let mut transport = listener.accept().unwrap();
            let _ = serve_connection(&mut transport, &Backend, 1, "test");
        })
    }

    #[test]
    fn status_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let server = start_server(&path);
        let mut client = Client::connect(&path).unwrap();
        let status = client.status().unwrap();
        assert_eq!(status.volume_id, "C:");
        assert_eq!(status.indexed_files, 1);
        assert!(!status.is_syncing);
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn status_errors_are_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let server = scripted(
            &path,
            vec![
                IpcMessage::HelloAck {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    server_version: "test".to_owned(),
                    session_id: 1,
                },
                IpcMessage::Error {
                    request_id: Some(1),
                    code: ErrorCode::ServerBusy.as_u32(),
                },
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        assert!(matches!(
            client.status(),
            Err(ClientError::Server(code)) if code == ErrorCode::ServerBusy.as_u32()
        ));
        drop(client);
        server.join().unwrap();

        let path = dir.path().join("host2.sock");
        let server = scripted(
            &path,
            vec![
                IpcMessage::HelloAck {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    server_version: "test".to_owned(),
                    session_id: 2,
                },
                IpcMessage::Error {
                    request_id: None,
                    code: ErrorCode::Unauthenticated.as_u32(),
                },
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        assert!(matches!(
            client.status(),
            Err(ClientError::Server(code)) if code == ErrorCode::Unauthenticated.as_u32()
        ));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn previous_protocol_version_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let server = start_server(&path);
        assert!(matches!(
            Client::connect_with_protocol_version(&path, PROTOCOL_VERSION - 1),
            Err(ClientError::Server(code)) if code == ErrorCode::UnsupportedVersion.as_u32()
        ));
        server.join().unwrap();
    }

    #[test]
    fn offline_errors_are_transport_failures() {
        let error = ClientError::Ipc(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        )));
        assert!(error.is_offline());
        assert!(!ClientError::Server(1).is_offline());
        assert!(!ClientError::Protocol(2).is_offline());
    }

    #[test]
    fn status_unexpected_message_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let server = scripted(
            &path,
            vec![
                IpcMessage::HelloAck {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    server_version: "test".to_owned(),
                    session_id: 3,
                },
                IpcMessage::StatusResult {
                    request_id: 999,
                    volume_id: "C:".to_owned(),
                    indexed_files: 0,
                    is_syncing: false,
                },
            ],
        );
        let mut client = Client::connect(&path).unwrap();
        assert!(matches!(client.status(), Err(ClientError::Unexpected(_))));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn query_and_server_error_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("host.sock");
        let server = start_server(&path);
        let mut client = Client::connect(&path).unwrap();
        assert_eq!(client.session_id(), 1);
        let response = client.query("a.txt", 10).unwrap();
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].name, "a.txt");
        assert!(!response.truncated);
        assert!(matches!(
            client.query("fail", 10),
            Err(ClientError::Server(code)) if code == ErrorCode::ServerBusy.as_u32()
        ));
        drop(client);
        let _ = server.join();
    }

    #[test]
    fn invalid_queries_are_rejected_locally() {
        assert!(matches!(
            validate_query("x", 0),
            Err(ClientError::InvalidQuery(_))
        ));
        assert!(matches!(
            validate_query(&"x".repeat(MAX_QUERY_CHARS + 1), 1),
            Err(ClientError::InvalidQuery(_))
        ));
        assert!(matches!(
            validate_query("bad\0query", 1),
            Err(ClientError::InvalidQuery(_))
        ));
        let dir = TempDir::new().unwrap();
        assert!(Client::connect(&dir.path().join("missing.sock")).is_err());
    }

    fn hello_ack(protocol_version: u16) -> IpcMessage {
        IpcMessage::HelloAck {
            protocol_version,
            server_version: "test".to_owned(),
            session_id: 3,
        }
    }

    #[test]
    fn handshake_errors_are_reported() {
        let dir = TempDir::new().unwrap();
        let mismatch = dir.path().join("mismatch.sock");
        let server = scripted(&mismatch, vec![hello_ack(PROTOCOL_VERSION + 1)]);
        let error = Client::connect(&mismatch).err().unwrap();
        assert!(matches!(error, ClientError::Protocol(_)));
        let _ = server.join();

        let refused = dir.path().join("refused.sock");
        let server = scripted(
            &refused,
            vec![IpcMessage::Error {
                request_id: None,
                code: ErrorCode::ServerBusy.as_u32(),
            }],
        );
        let error = Client::connect(&refused).err().unwrap();
        assert!(matches!(error, ClientError::Server(8)));
        let _ = server.join();
    }

    #[test]
    fn query_response_timeout_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("timeout.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let mut transport = listener.accept().unwrap();
            let _ = decode_payload(&transport.receive_frame().unwrap()).unwrap();
            transport
                .send_frame(&encode_payload(&hello_ack(PROTOCOL_VERSION)).unwrap())
                .unwrap();
            thread::sleep(Duration::from_millis(500));
        });
        let mut client = Client::connect_with_timeout(&path, Duration::from_millis(100)).unwrap();
        let started = std::time::Instant::now();
        let error = client.query("x", 1).unwrap_err();
        assert!(matches!(error, ClientError::Ipc(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(client);
        let _ = server.join();
    }

    #[test]
    fn handshake_unexpected_message_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("unexpected-handshake.sock");
        let server = scripted(&path, vec![IpcMessage::Ping { seq: 1 }]);
        let error = Client::connect(&path).err().unwrap();
        assert!(matches!(error, ClientError::Unexpected(_)));
        let _ = server.join();
    }

    #[test]
    fn query_errors_and_unsolicited_status_are_handled() {
        let dir = TempDir::new().unwrap();
        let failed = dir.path().join("query-error.sock");
        let server = scripted(
            &failed,
            vec![
                hello_ack(PROTOCOL_VERSION),
                IpcMessage::Error {
                    request_id: None,
                    code: ErrorCode::ServerBusy.as_u32(),
                },
            ],
        );
        let mut client = Client::connect(&failed).unwrap();
        assert!(matches!(client.query("x", 1), Err(ClientError::Server(8))));
        drop(client);
        let _ = server.join();

        let status = dir.path().join("status.sock");
        let server = scripted(
            &status,
            vec![
                hello_ack(PROTOCOL_VERSION),
                IpcMessage::UsnSyncStatus {
                    volume_id: "C:".to_owned(),
                    indexed_files: 3,
                    is_syncing: true,
                },
                IpcMessage::QueryResult {
                    request_id: 1,
                    items: Vec::new(),
                    truncated: false,
                },
            ],
        );
        let mut client = Client::connect(&status).unwrap();
        assert!(client.query("x", 1).unwrap().items.is_empty());
        drop(client);
        let _ = server.join();

        let unexpected = dir.path().join("unexpected.sock");
        let server = scripted(
            &unexpected,
            vec![hello_ack(PROTOCOL_VERSION), IpcMessage::Ping { seq: 1 }],
        );
        let mut client = Client::connect(&unexpected).unwrap();
        assert!(matches!(
            client.query("x", 1),
            Err(ClientError::Unexpected(_))
        ));
        drop(client);
        let _ = server.join();
    }
}
