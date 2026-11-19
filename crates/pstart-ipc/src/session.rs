//! Session authentication state machine.
//!
//! `Connected` means the OS transport has already produced a verified peer
//! identity. The client must still send a compatible `Hello` before any Query
//! is dispatched. No message field can override the transport identity.

use pstart_core::session::{
    session_step, MessageKind, SessionAction as CoreAction, SessionState as CoreState,
};
use pstart_core::ErrorCode;

use crate::error::IpcError;
use crate::message::IpcMessage;
use crate::transport::PeerIdentity;

/// Session state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Transport accepted and authenticated peer identity is available.
    Connected,
    /// Protocol handshake completed.
    Authenticated,
    /// Session can no longer process messages.
    Closed,
}

/// Action the server must take after handling one client message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    /// Send exactly one reply and keep the session in its current state.
    Reply(IpcMessage),
    /// Dispatch a validated query. The server is responsible for producing a
    /// correlated `QueryResult` or `Error`.
    DispatchQuery {
        /// Correlated request identifier.
        request_id: u64,
        /// Validated query text.
        text: String,
        /// Validated upper result bound.
        limit: u32,
    },
    /// Dispatch a status request. The server replies with one `UsnSyncStatus`.
    DispatchStatus {
        /// Correlated request identifier.
        request_id: u64,
    },
    /// Send an optional final reply and close the transport.
    Close(Option<IpcMessage>),
}

/// One server-side session.
#[derive(Debug)]
pub struct Session {
    peer: PeerIdentity,
    session_id: u64,
    server_version: String,
    client_version: Option<String>,
    state: SessionState,
}

fn core_state(state: SessionState) -> CoreState {
    match state {
        SessionState::Connected => CoreState::Connected,
        SessionState::Authenticated => CoreState::Authenticated,
        SessionState::Closed => CoreState::Closed,
    }
}

fn session_state(state: CoreState) -> SessionState {
    match state {
        CoreState::Connected => SessionState::Connected,
        CoreState::Authenticated => SessionState::Authenticated,
        CoreState::Closed => SessionState::Closed,
    }
}

fn message_kind(message: &IpcMessage) -> MessageKind {
    match message {
        IpcMessage::Hello {
            protocol_version, ..
        } => MessageKind::Hello {
            version: *protocol_version,
        },
        IpcMessage::Ping { .. } => MessageKind::Ping,
        IpcMessage::Query { .. } => MessageKind::Query,
        IpcMessage::StatusRequest { .. } => MessageKind::StatusRequest,
        _ => MessageKind::Other,
    }
}

fn request_id(message: &IpcMessage) -> Option<u64> {
    match message {
        IpcMessage::Query { request_id, .. } | IpcMessage::StatusRequest { request_id } => {
            Some(*request_id)
        }
        _ => None,
    }
}

impl Session {
    /// Create a session for an already OS-authenticated peer.
    pub fn new(
        peer: PeerIdentity,
        session_id: u64,
        server_version: impl Into<String>,
    ) -> Result<Self, IpcError> {
        let server_version = server_version.into();
        IpcMessage::HelloAck {
            protocol_version: pstart_core::PROTOCOL_VERSION,
            server_version: server_version.clone(),
            session_id,
        }
        .validate()?;

        Ok(Self {
            peer,
            session_id,
            server_version,
            client_version: None,
            state: SessionState::Connected,
        })
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// OS-verified peer identity.
    #[must_use]
    pub fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    /// Session identifier for log correlation.
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Client version captured during the handshake, if any.
    #[must_use]
    pub fn client_version(&self) -> Option<&str> {
        self.client_version.as_deref()
    }

    /// Process one decoded client message.
    pub fn handle(&mut self, message: IpcMessage) -> Result<SessionAction, IpcError> {
        message.validate()?;

        let decision = session_step(
            core_state(self.state),
            message_kind(&message),
            pstart_core::PROTOCOL_VERSION,
        );
        self.state = session_state(decision.next);
        self.apply_decision(decision.action, message)
    }

    fn apply_decision(
        &mut self,
        action: CoreAction,
        message: IpcMessage,
    ) -> Result<SessionAction, IpcError> {
        match (action, message) {
            (CoreAction::RejectClosed, _) => Err(IpcError::Closed),
            (CoreAction::HelloAck, IpcMessage::Hello { client_version, .. }) => {
                self.client_version = Some(client_version);
                Ok(SessionAction::Reply(IpcMessage::HelloAck {
                    protocol_version: pstart_core::PROTOCOL_VERSION,
                    server_version: self.server_version.clone(),
                    session_id: self.session_id,
                }))
            }
            (CoreAction::CloseUnsupportedVersion, _) => {
                Ok(SessionAction::Close(Some(IpcMessage::Error {
                    request_id: None,
                    code: ErrorCode::UnsupportedVersion.as_u32(),
                })))
            }
            (CoreAction::Pong, IpcMessage::Ping { seq }) => {
                Ok(SessionAction::Reply(IpcMessage::Pong { seq }))
            }
            (CoreAction::UnauthenticatedReply, message) => {
                Ok(SessionAction::Reply(IpcMessage::Error {
                    request_id: request_id(&message),
                    code: ErrorCode::Unauthenticated.as_u32(),
                }))
            }
            (
                CoreAction::DispatchQuery,
                IpcMessage::Query {
                    request_id,
                    text,
                    limit,
                },
            ) => Ok(SessionAction::DispatchQuery {
                request_id,
                text,
                limit,
            }),
            (CoreAction::DispatchStatus, IpcMessage::StatusRequest { request_id }) => {
                Ok(SessionAction::DispatchStatus { request_id })
            }
            (CoreAction::CloseInvalidRequest, _) => {
                Ok(SessionAction::Close(Some(IpcMessage::Error {
                    request_id: None,
                    code: ErrorCode::InvalidRequest.as_u32(),
                })))
            }
            _ => Ok(SessionAction::Close(Some(IpcMessage::Error {
                request_id: None,
                code: ErrorCode::InvalidRequest.as_u32(),
            }))),
        }
    }

    /// Mark the session closed.
    pub fn close(&mut self) {
        self.state = SessionState::Closed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TransportKind;
    use pstart_core::PROTOCOL_VERSION;

    fn peer() -> PeerIdentity {
        PeerIdentity::new(42, "S-1-5-21-100", TransportKind::UnixSocket).unwrap()
    }

    fn session() -> Session {
        Session::new(peer(), 7, "0.1.0").unwrap()
    }

    #[test]
    fn mismatched_decision_closes_instead_of_panicking() {
        let mut session = session();
        let action = session
            .apply_decision(
                CoreAction::Pong,
                IpcMessage::StatusRequest { request_id: 1 },
            )
            .unwrap();
        assert!(matches!(
            action,
            SessionAction::Close(Some(IpcMessage::Error { .. }))
        ));
    }

    fn hello() -> IpcMessage {
        IpcMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "0.1.0".to_owned(),
        }
    }

    fn make_session(state: SessionState) -> Session {
        let mut session = session();
        match state {
            SessionState::Connected => {}
            SessionState::Authenticated => {
                session.handle(hello()).unwrap();
            }
            SessionState::Closed => session.close(),
        }
        session
    }

    #[test]
    fn session_adapter_matches_core_table() {
        let messages = vec![
            hello(),
            IpcMessage::Hello {
                protocol_version: PROTOCOL_VERSION.saturating_add(1),
                client_version: "0.1.0".to_owned(),
            },
            IpcMessage::Ping { seq: 1 },
            IpcMessage::Query {
                request_id: 11,
                text: "x".to_owned(),
                limit: 2,
            },
            IpcMessage::StatusRequest { request_id: 12 },
            IpcMessage::QueryResult {
                request_id: 13,
                items: Vec::new(),
                truncated: false,
            },
            IpcMessage::StatusResult {
                request_id: 14,
                volume_id: "C:".to_owned(),
                indexed_files: 0,
                is_syncing: false,
            },
            IpcMessage::UsnSyncStatus {
                volume_id: "C:".to_owned(),
                indexed_files: 0,
                is_syncing: false,
            },
            IpcMessage::HelloAck {
                protocol_version: PROTOCOL_VERSION,
                server_version: "0.1.0".to_owned(),
                session_id: 7,
            },
            IpcMessage::Pong { seq: 1 },
            IpcMessage::Error {
                request_id: Some(15),
                code: ErrorCode::Unauthenticated.as_u32(),
            },
        ];
        for state in [
            SessionState::Connected,
            SessionState::Authenticated,
            SessionState::Closed,
        ] {
            for message in &messages {
                let mut session = make_session(state);
                let decision =
                    session_step(core_state(state), message_kind(message), PROTOCOL_VERSION);
                let result = session.handle(message.clone());
                match decision.action {
                    CoreAction::RejectClosed => {
                        assert!(matches!(result, Err(IpcError::Closed)));
                    }
                    CoreAction::HelloAck => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::Reply(IpcMessage::HelloAck {
                                protocol_version: PROTOCOL_VERSION,
                                session_id: 7,
                                ..
                            }))
                        ));
                    }
                    CoreAction::CloseUnsupportedVersion => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::Close(Some(IpcMessage::Error {
                                code,
                                ..
                            }))) if code == ErrorCode::UnsupportedVersion.as_u32()
                        ));
                    }
                    CoreAction::Pong => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::Reply(IpcMessage::Pong { seq: 1 }))
                        ));
                    }
                    CoreAction::UnauthenticatedReply => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::Reply(IpcMessage::Error {
                                request_id: actual,
                                code,
                            })) if actual == request_id(message)
                                && code == ErrorCode::Unauthenticated.as_u32()
                        ));
                    }
                    CoreAction::DispatchQuery => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::DispatchQuery {
                                request_id: 11,
                                limit: 2,
                                ..
                            })
                        ));
                    }
                    CoreAction::DispatchStatus => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::DispatchStatus { request_id: 12 })
                        ));
                    }
                    CoreAction::CloseInvalidRequest => {
                        assert!(matches!(
                            result,
                            Ok(SessionAction::Close(Some(IpcMessage::Error {
                                code,
                                ..
                            }))) if code == ErrorCode::InvalidRequest.as_u32()
                        ));
                    }
                }
                assert_eq!(session.state(), session_state(decision.next));
            }
        }
    }

    #[test]
    fn session_invariants_hold_over_three_steps() {
        fn rank(state: SessionState) -> u8 {
            match state {
                SessionState::Connected => 0,
                SessionState::Authenticated => 1,
                SessionState::Closed => 2,
            }
        }

        let messages = [
            hello(),
            IpcMessage::Ping { seq: 1 },
            IpcMessage::Query {
                request_id: 3,
                text: "x".to_owned(),
                limit: 1,
            },
        ];
        for first in 0..4 {
            for second in 0..4 {
                for third in 0..4 {
                    let mut session = session();
                    let mut ever_auth = false;
                    let mut ever_closed = false;
                    for choice in [first, second, third] {
                        let before = session.state();
                        if choice == 3 {
                            session.close();
                        } else {
                            let result = session.handle(messages[choice].clone());
                            if before == SessionState::Closed {
                                assert!(result.is_err());
                            }
                        }
                        let after = session.state();
                        ever_auth |= after == SessionState::Authenticated;
                        ever_closed |= after == SessionState::Closed;
                        assert!(rank(after) >= rank(before));
                        assert!(!ever_closed || after == SessionState::Closed);
                        assert!(!ever_auth || after != SessionState::Connected);
                        if after == SessionState::Authenticated {
                            assert!(session.client_version().is_some());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn valid_handshake_authenticates() {
        let mut session = session();
        assert_eq!(session.state(), SessionState::Connected);
        let action = session.handle(hello()).unwrap();
        assert!(matches!(
            action,
            SessionAction::Reply(IpcMessage::HelloAck {
                protocol_version: PROTOCOL_VERSION,
                session_id: 7,
                ..
            })
        ));
        assert_eq!(session.state(), SessionState::Authenticated);
        assert_eq!(session.client_version(), Some("0.1.0"));
    }

    #[test]
    fn query_before_hello_is_rejected() {
        let mut session = session();
        let action = session
            .handle(IpcMessage::Query {
                request_id: 11,
                text: "secret".to_owned(),
                limit: 10,
            })
            .unwrap();
        assert_eq!(
            action,
            SessionAction::Reply(IpcMessage::Error {
                request_id: Some(11),
                code: ErrorCode::Unauthenticated.as_u32()
            })
        );
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[test]
    fn status_before_hello_is_rejected() {
        let mut session = session();
        let action = session
            .handle(IpcMessage::StatusRequest { request_id: 12 })
            .unwrap();
        assert_eq!(
            action,
            SessionAction::Reply(IpcMessage::Error {
                request_id: Some(12),
                code: ErrorCode::Unauthenticated.as_u32()
            })
        );
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[test]
    fn unsupported_version_closes_session() {
        let mut session = session();
        let action = session
            .handle(IpcMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                client_version: "0.1.0".to_owned(),
            })
            .unwrap();
        assert_eq!(
            action,
            SessionAction::Close(Some(IpcMessage::Error {
                request_id: None,
                code: ErrorCode::UnsupportedVersion.as_u32()
            }))
        );
        assert_eq!(session.state(), SessionState::Closed);
        assert!(matches!(
            session.handle(IpcMessage::Ping { seq: 1 }),
            Err(IpcError::Closed)
        ));
    }

    #[test]
    fn duplicate_hello_closes_session() {
        let mut session = session();
        session.handle(hello()).unwrap();
        let action = session.handle(hello()).unwrap();
        assert!(matches!(action, SessionAction::Close(Some(_))));
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn query_after_handshake_is_dispatched() {
        let mut session = session();
        session.handle(hello()).unwrap();
        let action = session
            .handle(IpcMessage::Query {
                request_id: 99,
                text: "report".to_owned(),
                limit: 25,
            })
            .unwrap();
        assert_eq!(
            action,
            SessionAction::DispatchQuery {
                request_id: 99,
                text: "report".to_owned(),
                limit: 25
            }
        );
    }

    #[test]
    fn status_request_after_handshake_is_dispatched() {
        let mut session = session();
        session.handle(hello()).unwrap();
        let action = session
            .handle(IpcMessage::StatusRequest { request_id: 7 })
            .unwrap();
        assert_eq!(action, SessionAction::DispatchStatus { request_id: 7 });
    }

    #[test]
    fn ping_is_allowed_before_authentication() {
        let mut session = session();
        assert_eq!(
            session.handle(IpcMessage::Ping { seq: 5 }).unwrap(),
            SessionAction::Reply(IpcMessage::Pong { seq: 5 })
        );
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[test]
    fn server_to_client_types_from_client_close_session() {
        let mut session = session();
        session.handle(hello()).unwrap();
        let action = session
            .handle(IpcMessage::QueryResult {
                request_id: 1,
                items: vec![],
                truncated: false,
            })
            .unwrap();
        assert!(matches!(action, SessionAction::Close(Some(_))));
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn invalid_query_is_rejected_before_dispatch() {
        let mut session = session();
        session.handle(hello()).unwrap();
        let result = session.handle(IpcMessage::Query {
            request_id: 1,
            text: "a".repeat(pstart_core::MAX_QUERY_CHARS + 1),
            limit: 10,
        });
        assert!(matches!(result, Err(IpcError::InvalidMessage(_))));
        assert_eq!(session.state(), SessionState::Authenticated);
    }

    #[test]
    fn empty_server_version_is_rejected_at_construction() {
        assert!(Session::new(peer(), 1, "").is_err());
    }

    #[test]
    fn close_is_idempotent() {
        let mut session = session();
        session.close();
        session.close();
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn session_exposes_verified_identity_and_id() {
        let session = session();
        assert_eq!(session.peer(), &peer());
        assert_eq!(session.session_id(), 7);
        assert_eq!(session.client_version(), None);
    }

    #[test]
    fn connected_session_rejects_server_to_client_messages() {
        let mut session = session();
        let action = session
            .handle(IpcMessage::HelloAck {
                protocol_version: PROTOCOL_VERSION,
                server_version: "0.1.0".to_owned(),
                session_id: 1,
            })
            .unwrap();
        assert_eq!(
            action,
            SessionAction::Reply(IpcMessage::Error {
                request_id: None,
                code: ErrorCode::Unauthenticated.as_u32()
            })
        );
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[test]
    fn authenticated_ping_keeps_session_open() {
        let mut session = session();
        session.handle(hello()).unwrap();
        assert_eq!(
            session.handle(IpcMessage::Ping { seq: 9 }).unwrap(),
            SessionAction::Reply(IpcMessage::Pong { seq: 9 })
        );
        assert_eq!(session.state(), SessionState::Authenticated);
    }

    #[test]
    fn closed_session_rejects_valid_messages() {
        let mut session = session();
        session.close();
        assert!(matches!(
            session.handle(IpcMessage::Ping { seq: 1 }),
            Err(IpcError::Closed)
        ));
    }
}
