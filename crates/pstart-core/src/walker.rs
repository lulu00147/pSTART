//! Parent-chain walking for path reconstruction.

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
        if !present.get(index).copied().unwrap_or(false) {
            return Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: current },
                visited,
            });
        }
        if !name_valid.get(index).copied().unwrap_or(false) {
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
    fn named_walk_reports_absent_presence_slot() {
        let parents = [0, 0, 1, 2];
        let present = [true, true];
        let valid = [true, true, true, true];
        assert_eq!(
            walk_named(&parents, &present, &valid, 0, 3, 8),
            Err(NamedWalkFailure {
                error: NamedWalkError::MissingParent { node: 3 },
                visited: Vec::new(),
            })
        );
    }
}
