use pstart_core::{ErrorCode, PROTOCOL_VERSION};
use pstart_ipc::{IpcMessage, PeerIdentity, Session, SessionAction, SessionState, TransportKind};

fn session() -> Session {
    Session::new(
        PeerIdentity::new(42, "S-1-5-21-1", TransportKind::UnixSocket).unwrap(),
        7,
        "0.1.0",
    )
    .unwrap()
}

fn hello(version: u16) -> IpcMessage {
    IpcMessage::Hello {
        protocol_version: version,
        client_version: "0.1.0".to_owned(),
    }
}

#[test]
fn tla_trace_actions_match_session() {
    // Init/Connected: no query is dispatched before Hello.
    let mut session = session();
    assert_eq!(session.state(), SessionState::Connected);
    let action = session
        .handle(IpcMessage::Query {
            request_id: 1,
            text: "before".to_owned(),
            limit: 10,
        })
        .unwrap();
    assert!(matches!(
        action,
        SessionAction::Reply(IpcMessage::Error {
            request_id: Some(1),
            code,
        }) if code == ErrorCode::Unauthenticated.as_u32()
    ));
    assert_eq!(session.state(), SessionState::Connected);

    // Hello(2): Authenticated.
    let action = session.handle(hello(PROTOCOL_VERSION)).unwrap();
    assert!(matches!(
        action,
        SessionAction::Reply(IpcMessage::HelloAck { session_id: 7, .. })
    ));
    assert_eq!(session.state(), SessionState::Authenticated);

    // Query/Status are dispatched only after authentication.
    let action = session
        .handle(IpcMessage::Query {
            request_id: 2,
            text: "a".to_owned(),
            limit: 10,
        })
        .unwrap();
    assert!(matches!(
        action,
        SessionAction::DispatchQuery { request_id: 2, .. }
    ));
    let action = session
        .handle(IpcMessage::StatusRequest { request_id: 3 })
        .unwrap();
    assert!(matches!(
        action,
        SessionAction::DispatchStatus { request_id: 3 }
    ));

    // A second Hello is a violation and closes the session.
    let action = session.handle(hello(PROTOCOL_VERSION)).unwrap();
    assert!(matches!(action, SessionAction::Close(Some(_))));
    assert_eq!(session.state(), SessionState::Closed);
    assert!(session.handle(hello(PROTOCOL_VERSION)).is_err());
}

#[test]
fn tla_version_guard_rejects_old_protocol() {
    let mut session = session();
    let action = session.handle(hello(PROTOCOL_VERSION - 1)).unwrap();
    assert!(matches!(
        action,
        SessionAction::Close(Some(IpcMessage::Error {
            code,
            ..
        })) if code == ErrorCode::UnsupportedVersion.as_u32()
    ));
    assert_eq!(session.state(), SessionState::Closed);
}

fn protocol_messages() -> Vec<IpcMessage> {
    vec![
        IpcMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "0.1.0".to_owned(),
        },
        IpcMessage::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.1.0".to_owned(),
            session_id: 7,
        },
        IpcMessage::Ping { seq: 1 },
        IpcMessage::Pong { seq: 1 },
        IpcMessage::Query {
            request_id: 1,
            text: "x".to_owned(),
            limit: 10,
        },
        IpcMessage::QueryResult {
            request_id: 1,
            items: Vec::new(),
            truncated: false,
        },
        IpcMessage::UsnSyncStatus {
            volume_id: "C:".to_owned(),
            indexed_files: 0,
            is_syncing: false,
        },
        IpcMessage::Error {
            request_id: None,
            code: 1,
        },
        IpcMessage::StatusRequest { request_id: 2 },
        IpcMessage::StatusResult {
            request_id: 2,
            volume_id: "C:".to_owned(),
            indexed_files: 0,
            is_syncing: false,
        },
    ]
}

#[test]
fn tla_session_message_matrix_never_dispatches_before_auth() {
    for state in [
        SessionState::Connected,
        SessionState::Authenticated,
        SessionState::Closed,
    ] {
        for message in protocol_messages() {
            let mut session = session();
            if state == SessionState::Authenticated {
                session.handle(hello(PROTOCOL_VERSION)).unwrap();
            }
            if state == SessionState::Closed {
                session.handle(hello(PROTOCOL_VERSION)).unwrap();
                session.handle(hello(PROTOCOL_VERSION)).unwrap();
            }
            let before = session.state();
            let result = session.handle(message);
            let dispatched = matches!(
                result,
                Ok(SessionAction::DispatchQuery { .. }) | Ok(SessionAction::DispatchStatus { .. })
            );
            if dispatched {
                assert_eq!(before, SessionState::Authenticated);
            }
            if before == SessionState::Connected {
                assert!(!dispatched);
            }
            if before == SessionState::Closed {
                assert!(result.is_err());
            }
        }
    }
}
