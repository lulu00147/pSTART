//! Pure, platform-independent predicates used by pSTART components.
//!
//! This crate has no OS dependency. External semantics (Unicode tables and
//! filesystem lookups) remain adapter boundaries.

/// Error from the pure parent-chain walker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkError {
    /// Depth budget exhausted.
    DepthExceeded,
    /// A node was visited twice.
    Cycle { node: u32 },
    /// A parent index is not present.
    MissingParent { node: u32 },
}

/// Error from the per-step walk guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardError {
    /// Current node was already visited.
    Cycle,
    /// Depth budget exhausted.
    DepthExceeded,
}

/// Guard one step of a parent walk before looking up the next record.
///
/// `depth` is the number of components already collected. Returns `Ok(())`
/// when another lookup is allowed.
pub fn guard_next(
    current: u64,
    visited: &[u64],
    depth: usize,
    max_depth: usize,
) -> Result<(), GuardError> {
    if depth >= max_depth {
        return Err(GuardError::DepthExceeded);
    }
    if visited.contains(&current) {
        return Err(GuardError::Cycle);
    }
    Ok(())
}

/// Walk a parent-index chain until `root`, detecting cycles and depth overflow.
///
/// Parent indices are slot indices; this function replaces trait/HashSet based
/// path walking with a pure index scan.
pub fn walk_parents(
    parents: &[u32],
    root: u32,
    start: u32,
    max_depth: usize,
) -> Result<Vec<u32>, WalkError> {
    let mut current = start;
    let mut visited = Vec::new();
    while current != root {
        if visited.len() >= max_depth {
            return Err(WalkError::DepthExceeded);
        }
        if visited.contains(&current) {
            return Err(WalkError::Cycle { node: current });
        }
        visited.push(current);
        let parent = parents
            .get(current as usize)
            .ok_or(WalkError::MissingParent { node: current })?;
        current = *parent;
    }
    Ok(visited)
}

/// Failure kind for a parent walk that also enforces per-node name validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedWalkError {
    /// Depth budget exhausted before the next node.
    DepthExceeded,
    /// A node was visited twice.
    Cycle { node: u32 },
    /// A parent index is not present in the parent or validity tables.
    MissingParent { node: u32 },
    /// The node's name failed the product name checks.
    InvalidName { node: u32 },
}

/// Failure plus the successfully validated prefix before the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedWalkFailure {
    /// The classified failure.
    pub error: NamedWalkError,
    /// Nodes whose parent and name were valid before the failure.
    pub visited: Vec<u32>,
}

/// Parent walk with name-validity enforcement and the product error priority.
///
/// Order is intentionally depth, cycle, missing parent, invalid name. The
/// returned prefix contains only nodes whose parent lookup and name validation
/// both succeeded.
pub fn walk_named(
    parents: &[u32],
    present: &[bool],
    name_valid: &[bool],
    root: u32,
    start: u32,
    max_depth: usize,
) -> Result<Vec<u32>, NamedWalkFailure> {
    let mut current = start;
    let mut visited = Vec::new();
    while current != root {
        if visited.len() >= max_depth {
            return Err(NamedWalkFailure {
                error: NamedWalkError::DepthExceeded,
                visited,
            });
        }
        if visited.contains(&current) {
            return Err(NamedWalkFailure {
                error: NamedWalkError::Cycle { node: current },
                visited,
            });
        }
        let index = current as usize;
        if index >= parents.len() {
            return Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: current },
                visited,
            });
        }
        let parent = parents[index];
        if index >= present.len() || !present[index] {
            return Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: current },
                visited,
            });
        }
        if index >= name_valid.len() || !name_valid[index] {
            return Err(NamedWalkFailure {
                error: NamedWalkError::InvalidName { node: current },
                visited,
            });
        }
        visited.push(current);
        current = parent;
    }
    Ok(visited)
}

/// Failure when the assembled path exceeds the display limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathTooLong {
    /// Actual UTF-16 unit count.
    pub actual: usize,
    /// Maximum allowed.
    pub max: usize,
}

/// Assemble UTF-8 component bytes into a path, in start-to-root order.
///
/// `names` is a flat arena; `offsets[i]..offsets[i + 1]` is component `i`.
/// `widths[i]` is its UTF-16 length (computed by the std/OS boundary).
/// Components are appended in reverse order, with one `\` separator when the
/// previous byte is not already a separator. The UTF-16 total is computed
/// without re-decoding bytes.
pub fn assemble_path_bytes(
    root: &[u8],
    names: &[u8],
    offsets: &[usize],
    widths: &[usize],
    root_ends_with_sep: bool,
    root_width: usize,
    max_utf16: usize,
) -> Result<Vec<u8>, PathTooLong> {
    let mut out = Vec::new();
    let mut root_index = 0;
    while root_index < root.len() {
        out.push(root[root_index]);
        root_index += 1;
    }
    let mut last_is_sep = root_ends_with_sep;
    let mut total = root_width;
    let mut position = 0;
    while position < widths.len() {
        if !last_is_sep {
            out.push(b'\\');
            total = total.saturating_add(1);
            last_is_sep = true;
        }
        let start = offsets[position];
        let end = offsets[position + 1];
        let mut index = start;
        while index < end {
            out.push(names[index]);
            index += 1;
        }
        if start < end {
            last_is_sep = false;
        }
        total = total.saturating_add(widths[position]);
        position += 1;
    }
    if total > max_utf16 {
        return Err(PathTooLong {
            actual: total,
            max: max_utf16,
        });
    }
    Ok(out)
}

/// Failure plus the successfully looked-up prefix before the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkFailure {
    /// The classified failure.
    pub error: WalkError,
    /// Nodes whose records existed before the failure.
    pub visited: Vec<u32>,
}

/// Index-only parent walk that preserves the successful prefix on failure.
pub fn walk_indices(
    parents: &[u32],
    root: u32,
    start: u32,
    max_depth: usize,
) -> Result<Vec<u32>, WalkFailure> {
    let mut current = start;
    let mut visited = Vec::new();
    while current != root {
        if visited.len() >= max_depth {
            return Err(WalkFailure {
                error: WalkError::DepthExceeded,
                visited,
            });
        }
        if visited.contains(&current) {
            return Err(WalkFailure {
                error: WalkError::Cycle { node: current },
                visited,
            });
        }
        let parent = match parents.get(current as usize) {
            Some(parent) => *parent,
            None => {
                return Err(WalkFailure {
                    error: WalkError::MissingParent { node: current },
                    visited,
                });
            }
        };
        visited.push(current);
        current = parent;
    }
    Ok(visited)
}

/// Whether another path component fits within the configured depth.
#[must_use]
pub fn depth_allows(components_len: usize, max_depth: usize) -> bool {
    components_len < max_depth
}

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

/// Case-insensitive substring check with an ASCII allocation-free fast path.
///
/// Non-ASCII inputs use Unicode lowercasing; the case mapping table itself is
/// provided by the standard library.
#[must_use]
pub fn contains_case_insensitive(name: &str, needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    contains_lowercase_needle(name, &needle_lower)
}

/// Case-insensitive substring test with an already-lowercased needle.
///
/// Use this in hot loops to avoid reallocating the needle for every name.
pub fn contains_lowercase_needle(name: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if name.is_ascii() {
        return name
            .as_bytes()
            .windows(needle_lower.len())
            .any(|window| window.eq_ignore_ascii_case(needle_lower.as_bytes()));
    }
    name.to_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_walk_success_chain_and_root() {
        let parents = [0, 0, 1, 2];
        let present = [true, true, true, true];
        let valid = [true, true, true, true];
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 3, 4),
            Ok(vec![3, 2, 1])
        );
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 0, 4),
            Ok(Vec::new())
        );
    }

    #[test]
    fn named_walk_depth_cycle_missing_and_invalid_prefixes() {
        let parents = [0, 0, 1, 2];
        let present = [true, true, true, true];
        let valid = [true, true, true, true];
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 3, 2),
            Err(NamedWalkFailure {
                error: NamedWalkError::DepthExceeded,
                visited: vec![3, 2],
            })
        );

        let cycle_parents = [0, 2, 1, 2];
        assert_eq!(
            walk_named(&cycle_parents, &present, &valid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::Cycle { node: 2 },
                visited: vec![3, 2, 1],
            })
        );

        let missing_parents = [0, 0, 7, 2];
        assert_eq!(
            walk_named(&missing_parents, &present, &valid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: 7 },
                visited: vec![3, 2],
            })
        );

        let invalid = [true, true, false, true];
        assert_eq!(
            walk_named(&parents, &present, &invalid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::InvalidName { node: 2 },
                visited: vec![3],
            })
        );
    }

    #[test]
    fn named_walk_preserves_error_priority() {
        let present = [true, true, true, true];
        let valid = [true, true, true, true];
        let cycle = [0, 2, 1, 2];
        assert_eq!(
            walk_named(&cycle, &present, &valid, 0, 3, 1),
            Err(NamedWalkFailure {
                error: NamedWalkError::DepthExceeded,
                visited: vec![3],
            })
        );

        let invalid_cycle = [true, true, false, true];
        assert_eq!(
            walk_named(&cycle, &present, &invalid_cycle, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::InvalidName { node: 2 },
                visited: vec![3],
            })
        );

        let invalid_missing = [true, true, true, true];
        let missing = [0, 0, 7, 2];
        assert_eq!(
            walk_named(&missing, &present, &invalid_missing, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: 7 },
                visited: vec![3, 2],
            })
        );
    }

    #[test]
    fn named_walk_treats_absent_validity_as_invalid() {
        let parents = [0, 0, 1, 2];
        let present = [true, true, true, true];
        let valid = [true, true];
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::InvalidName { node: 3 },
                visited: Vec::new(),
            })
        );
    }

    #[test]
    fn named_walk_reports_absent_slot_before_name() {
        let parents = [0, 0, 1, 2];
        let present = [true, true, false, true];
        let valid = [true, true, true, true];
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: 2 },
                visited: vec![3],
            })
        );
    }

    #[test]
    fn index_walk_covers_all_stop_paths() {
        let parents = [0, 0, 1, 2];
        assert_eq!(walk_indices(&parents, 0, 0, 4), Ok(Vec::new()));
        assert_eq!(walk_indices(&parents, 0, 3, 4), Ok(vec![3, 2, 1]));
        assert_eq!(
            walk_indices(&parents, 0, 3, 2),
            Err(WalkFailure {
                error: WalkError::DepthExceeded,
                visited: vec![3, 2],
            })
        );
        let cycle = [0, 2, 1, 2];
        assert_eq!(
            walk_indices(&cycle, 0, 3, 8),
            Err(WalkFailure {
                error: WalkError::Cycle { node: 2 },
                visited: vec![3, 2, 1],
            })
        );
        let missing = [0, 0, 7, 2];
        assert_eq!(
            walk_indices(&missing, 0, 3, 8),
            Err(WalkFailure {
                error: WalkError::MissingParent { node: 7 },
                visited: vec![3, 2],
            })
        );
    }

    #[test]
    fn assembles_bytes_in_reverse_with_separators() {
        let names = b"dcb";
        let offsets = [0, 1, 2, 3];
        let widths = [1, 1, 1];
        assert_eq!(
            assemble_path_bytes(b"C:\\", names, &offsets, &widths, true, 3, 100).unwrap(),
            b"C:\\d\\c\\b"
        );
        assert_eq!(
            assemble_path_bytes(b"\\\\", names, &offsets, &widths, true, 1, 100).unwrap(),
            b"\\\\d\\c\\b"
        );
    }

    #[test]
    fn assembly_handles_empty_root_separator_and_exact_limit() {
        assert_eq!(
            assemble_path_bytes(b"C:", b"", &[0], &[], false, 2, 2).unwrap(),
            b"C:"
        );
        assert_eq!(
            assemble_path_bytes(b"C:\\", b"a", &[0, 1], &[1], true, 3, 4).unwrap(),
            b"C:\\a"
        );
        assert_eq!(
            assemble_path_bytes(b"C:", b"a", &[0, 1], &[1], false, 2, 4).unwrap(),
            b"C:\\a"
        );
    }

    #[test]
    fn assembly_reports_utf16_overflow_without_overflowing_usize() {
        let names = b"ab";
        let offsets = [0, 1, 2];
        let widths = [usize::MAX, 1];
        assert_eq!(
            assemble_path_bytes(b"C:", names, &offsets, &widths, false, 2, 10),
            Err(PathTooLong {
                actual: usize::MAX,
                max: 10,
            })
        );
    }

    #[test]
    fn depth_guard_rejects_at_limit() {
        assert!(depth_allows(0, 1));
        assert!(!depth_allows(1, 1));
        assert!(!depth_allows(2, 1));
        assert_eq!(guard_next(2, &[2], 1, 1), Err(GuardError::DepthExceeded));
    }

    #[test]
    fn case_insensitive_prefilter_matches_lowercase_semantics() {
        assert!(contains_case_insensitive("REPORT.TXT", "report"));
        assert!(contains_case_insensitive("项目报告.docx", "报告"));
        assert!(contains_case_insensitive("CAFÉ.TXT", "café"));
        assert!(contains_case_insensitive("café.txt", "CAFÉ"));
        assert!(!contains_case_insensitive("abc", "x"));
        assert!(contains_case_insensitive("abc", ""));
    }

    #[test]
    fn parent_walker_handles_chain_cycle_and_depth() {
        let parents = [0, 0, 1, 2];
        assert_eq!(walk_parents(&parents, 0, 3, 4).unwrap(), vec![3, 2, 1]);
        assert_eq!(
            walk_parents(&parents, 0, 3, 2),
            Err(WalkError::DepthExceeded)
        );
        let cycle = [0, 2, 1];
        assert_eq!(
            walk_parents(&cycle, 9, 1, 4),
            Err(WalkError::Cycle { node: 1 })
        );
        assert_eq!(
            walk_parents(&parents, 0, 7, 4),
            Err(WalkError::MissingParent { node: 7 })
        );
        assert_eq!(guard_next(2, &[2], 0, 4), Err(GuardError::Cycle));
        assert_eq!(guard_next(2, &[], 4, 4), Err(GuardError::DepthExceeded));
        assert_eq!(guard_next(2, &[], 0, 4), Ok(()));
    }

    #[test]
    fn parent_walker_paths_are_parent_chains() {
        let cases: [[u32; 4]; 3] = [[0, 0, 1, 2], [0, 2, 1, 3], [0, 0, 0, 0]];
        for parents in cases {
            for root in 0..4u32 {
                for start in 0..4u32 {
                    for max_depth in 0..5usize {
                        let Ok(path) = walk_parents(&parents, root, start, max_depth) else {
                            continue;
                        };
                        for pair in path.windows(2) {
                            assert_eq!(
                                parents[pair[0] as usize], pair[1],
                                "broken parent edge in {path:?}"
                            );
                        }
                        if let Some(last) = path.last() {
                            assert_eq!(
                                parents[*last as usize], root,
                                "last parent is not root in {path:?}"
                            );
                        }
                    }
                }
            }
        }
    }

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
