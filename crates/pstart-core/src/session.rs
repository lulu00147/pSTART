//! Pure IPC session state machine.

/// Pure mirror of the IPC session state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Transport authenticated, protocol handshake pending.
    Connected,
    /// Protocol handshake completed.
    Authenticated,
    /// Session closed.
    Closed,
}

/// Decoded message class relevant to session policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// Handshake with a protocol version.
    Hello { version: u16 },
    /// Liveness ping.
    Ping,
    /// Query request.
    Query,
    /// Status request.
    StatusRequest,
    /// Any other client or server-to-client message.
    Other,
}

/// Action class selected by the session policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAction {
    /// Reply with a HelloAck.
    HelloAck,
    /// Close with unsupported-version error.
    CloseUnsupportedVersion,
    /// Reply with Pong.
    Pong,
    /// Reply with Unauthenticated.
    UnauthenticatedReply,
    /// Dispatch a validated query.
    DispatchQuery,
    /// Dispatch a status request.
    DispatchStatus,
    /// Close with invalid-request error.
    CloseInvalidRequest,
    /// Reject because the session is already closed.
    RejectClosed,
}

/// Pure session decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionDecision {
    /// State after the decision.
    pub next: SessionState,
    /// Action class for the adapter.
    pub action: SessionAction,
}

/// Decide the next session state/action without IO or allocation.
#[must_use]
pub fn session_step(
    state: SessionState,
    message: MessageKind,
    protocol_version: u16,
) -> SessionDecision {
    match state {
        SessionState::Closed => SessionDecision {
            next: SessionState::Closed,
            action: SessionAction::RejectClosed,
        },
        SessionState::Connected => match message {
            MessageKind::Hello { version } if version == protocol_version => SessionDecision {
                next: SessionState::Authenticated,
                action: SessionAction::HelloAck,
            },
            MessageKind::Hello { .. } => SessionDecision {
                next: SessionState::Closed,
                action: SessionAction::CloseUnsupportedVersion,
            },
            MessageKind::Ping => SessionDecision {
                next: SessionState::Connected,
                action: SessionAction::Pong,
            },
            MessageKind::Query | MessageKind::StatusRequest | MessageKind::Other => {
                SessionDecision {
                    next: SessionState::Connected,
                    action: SessionAction::UnauthenticatedReply,
                }
            }
        },
        SessionState::Authenticated => match message {
            MessageKind::Query => SessionDecision {
                next: SessionState::Authenticated,
                action: SessionAction::DispatchQuery,
            },
            MessageKind::StatusRequest => SessionDecision {
                next: SessionState::Authenticated,
                action: SessionAction::DispatchStatus,
            },
            MessageKind::Ping => SessionDecision {
                next: SessionState::Authenticated,
                action: SessionAction::Pong,
            },
            MessageKind::Hello { .. } | MessageKind::Other => SessionDecision {
                next: SessionState::Closed,
                action: SessionAction::CloseInvalidRequest,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_policy_covers_all_states_and_messages() {
        use MessageKind::*;
        let states = [
            SessionState::Connected,
            SessionState::Authenticated,
            SessionState::Closed,
        ];
        let kinds = [
            Hello { version: 2 },
            Hello { version: 1 },
            Ping,
            Query,
            StatusRequest,
            Other,
        ];
        for state in states {
            for kind in kinds {
                let decision = session_step(state, kind, 2);
                if decision.action == SessionAction::DispatchQuery {
                    assert_eq!(state, SessionState::Authenticated);
                }
                if state == SessionState::Closed {
                    assert_eq!(decision.action, SessionAction::RejectClosed);
                }
            }
        }
    }
}
