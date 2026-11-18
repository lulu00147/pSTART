//! Reconstruct full paths from FRN parent links.
//!
//! NTFS USN records do not carry a complete path. The index stores names and
//! parent FRNs; resolution walks parent links with cycle and depth guards.

use pstart_core::limits::MAX_PATH_UTF16;
use pstart_core::Frn;
use thiserror::Error;

use crate::model::FileRecord;

/// Lookup interface used by path reconstruction.
pub trait RecordLookup {
    /// Return a record by FRN, or `None` if it is not present.
    fn record(&self, frn: Frn) -> Option<&FileRecord>;
}

/// Path reconstruction failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PathError {
    /// The starting record is absent.
    #[error("record {frn} not found")]
    NotFound {
        /// Starting FRN.
        frn: Frn,
    },

    /// A parent link points at an absent record.
    #[error("parent {parent} of record {child} is missing")]
    MissingParent {
        /// Child FRN.
        child: Frn,
        /// Missing parent FRN.
        parent: Frn,
    },

    /// Parent links form a cycle.
    #[error("cycle detected at record {frn}")]
    Cycle {
        /// FRN where the cycle was detected.
        frn: Frn,
    },

    /// The parent chain exceeded the configured depth.
    #[error("path depth exceeds {max}")]
    DepthExceeded {
        /// Configured maximum depth.
        max: usize,
    },

    /// A name cannot be safely joined into a path.
    #[error("record {frn} has an invalid name")]
    InvalidName {
        /// Offending FRN.
        frn: Frn,
    },

    /// The assembled path exceeded the display limit.
    #[error("path is {actual} UTF-16 units, maximum is {max}")]
    PathTooLong {
        /// Actual UTF-16 code unit count.
        actual: usize,
        /// Maximum allowed.
        max: usize,
    },

    /// The configured root string is invalid.
    #[error("invalid volume root: {0}")]
    InvalidRoot(String),
}

fn lookup_failure(current: Frn, start: Frn, child: Frn) -> PathError {
    if current == start {
        PathError::NotFound { frn: start }
    } else {
        PathError::MissingParent {
            child,
            parent: current,
        }
    }
}

/// Validates the final path length before returning it.
fn finish_path(path: String) -> Result<String, PathError> {
    let length = path.encode_utf16().count();
    if length > MAX_PATH_UTF16 {
        return Err(PathError::PathTooLong {
            actual: length,
            max: MAX_PATH_UTF16,
        });
    }
    Ok(path)
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('\0') && !name.contains('\\') && !name.contains('/')
}

/// Resolve a record's full display path.
///
/// The root record itself is not included as a path component; `root` is
/// appended directly. The separator is always `\` because the product target is
/// Windows/NTFS.
pub fn resolve_path<L: RecordLookup + ?Sized>(
    lookup: &L,
    root: &str,
    root_frn: Frn,
    frn: Frn,
    max_depth: usize,
) -> Result<String, PathError> {
    if root.is_empty() || root.contains('\0') {
        return Err(PathError::InvalidRoot(root.to_owned()));
    }
    if max_depth == 0 {
        return Err(PathError::DepthExceeded { max: 0 });
    }

    let normalized_root = root.trim_end_matches(['\\', '/']);
    let normalized_root = if normalized_root.is_empty() {
        "\\"
    } else {
        normalized_root
    };

    if frn.record_number() == root_frn.record_number() {
        return finish_path(normalized_root.to_owned());
    }

    let components = collect_components(lookup, root_frn, frn, max_depth)?;
    let mut path = String::from(normalized_root);
    for component in components.iter().rev() {
        if !path.ends_with('\\') {
            path.push('\\');
        }
        path.push_str(component);
    }
    finish_path(path)
}

fn collect_components<L: RecordLookup + ?Sized>(
    lookup: &L,
    root_frn: Frn,
    frn: Frn,
    max_depth: usize,
) -> Result<Vec<&str>, PathError> {
    let mut components: Vec<&str> = Vec::new();
    let mut visited: Vec<u64> = Vec::new();
    let mut current = frn;
    let mut child = frn;

    while current.record_number() != root_frn.record_number() {
        if components.len() >= max_depth {
            return Err(PathError::DepthExceeded { max: max_depth });
        }
        if visited.contains(&current.get()) {
            return Err(PathError::Cycle { frn: current });
        }
        visited.push(current.get());

        let record = lookup
            .record(current)
            .ok_or_else(|| lookup_failure(current, frn, child))?;

        if !valid_name(&record.name) {
            return Err(PathError::InvalidName { frn: current });
        }
        components.push(&record.name);
        child = current;
        current = Frn::new(record.parent_frn);
    }

    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use pstart_core::walker::{walk_parents, WalkError};
    use std::collections::HashMap;

    struct MapLookup(HashMap<u64, FileRecord>);

    impl RecordLookup for MapLookup {
        fn record(&self, frn: Frn) -> Option<&FileRecord> {
            self.0.get(&frn.get())
        }
    }

    fn record(frn: u64, parent: u64, name: &str) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.to_owned(),
            attributes: 0,
            is_dir: false,
            last_usn: 1,
        }
    }

    fn lookup(records: Vec<FileRecord>) -> MapLookup {
        MapLookup(records.into_iter().map(|r| (r.frn, r)).collect())
    }

    fn chain() -> MapLookup {
        lookup(vec![
            record(10, 5, "a"),
            record(11, 10, "b"),
            record(12, 11, "c"),
        ])
    }

    #[test]
    fn resolve_path_depth_boundaries() {
        for max_depth in [1, 2, 3, 4, 5, 8, 9, 64, 65, 256] {
            let result = resolve_path(&chain(), "C:\\", Frn::ROOT, Frn::new(12), max_depth);
            if max_depth >= 3 {
                assert_eq!(result.unwrap(), "C:\\a\\b\\c");
            } else {
                assert_eq!(result, Err(PathError::DepthExceeded { max: max_depth }));
            }
        }
    }

    #[test]
    fn resolve_path_unbounded_depth() {
        let lookup = chain();
        assert_eq!(
            resolve_path(&lookup, "C:\\", Frn::ROOT, Frn::new(12), usize::MAX).unwrap(),
            "C:\\a\\b\\c"
        );
    }

    #[test]
    fn resolve_path_reports_missing_and_invalid_cases() {
        assert_eq!(
            resolve_path(&lookup(vec![]), "C:\\", Frn::ROOT, Frn::new(99), usize::MAX),
            Err(PathError::NotFound { frn: Frn::new(99) })
        );
        assert_eq!(
            resolve_path(
                &lookup(vec![record(10, 99, "a")]),
                "C:\\",
                Frn::ROOT,
                Frn::new(10),
                usize::MAX,
            ),
            Err(PathError::MissingParent {
                child: Frn::new(10),
                parent: Frn::new(99),
            })
        );
        assert_eq!(
            resolve_path(
                &lookup(vec![record(10, 5, "bad\\name")]),
                "C:\\",
                Frn::ROOT,
                Frn::new(10),
                usize::MAX,
            ),
            Err(PathError::InvalidName { frn: Frn::new(10) })
        );
    }

    #[test]
    fn resolve_path_reports_cycle_and_depth() {
        let cycle = lookup(vec![record(10, 11, "a"), record(11, 10, "b")]);
        assert_eq!(
            resolve_path(&cycle, "C:\\", Frn::ROOT, Frn::new(10), usize::MAX),
            Err(PathError::Cycle { frn: Frn::new(10) })
        );

        let mut records = Vec::new();
        for frn in 0..=400u64 {
            let parent = if frn == 0 { 5 } else { frn - 1 };
            records.push(record(frn, parent, "n"));
        }
        assert_eq!(
            resolve_path(&lookup(records), "C:\\", Frn::ROOT, Frn::new(400), 257,),
            Err(PathError::DepthExceeded { max: 257 })
        );
    }

    #[test]
    fn max_depth_resolves_long_chain() {
        let mut records = Vec::new();
        for index in 0..300u64 {
            let parent = if index == 0 { 5 } else { index + 9 };
            records.push(record(index + 10, parent, &format!("n{index}")));
        }
        let lookup = lookup(records);
        let expected = format!(
            "C:\\{}",
            (0..300)
                .map(|index| format!("n{index}"))
                .collect::<Vec<_>>()
                .join("\\")
        );
        assert_eq!(
            resolve_path(&lookup, "C:\\", Frn::ROOT, Frn::new(309), 300).unwrap(),
            expected
        );
        assert_eq!(
            resolve_path(&lookup, "C:\\", Frn::ROOT, Frn::new(309), 256),
            Err(PathError::DepthExceeded { max: 256 })
        );
    }

    #[test]
    fn invalid_name_precedes_later_parent_errors() {
        let missing_after_invalid = lookup(vec![record(10, 99, "bad/name")]);
        assert_eq!(
            resolve_path(&missing_after_invalid, "C:\\", Frn::ROOT, Frn::new(10), 8),
            Err(PathError::InvalidName { frn: Frn::new(10) })
        );

        let cycle_after_invalid = lookup(vec![record(10, 11, "bad/name"), record(11, 10, "cycle")]);
        assert_eq!(
            resolve_path(&cycle_after_invalid, "C:\\", Frn::ROOT, Frn::new(10), 8),
            Err(PathError::InvalidName { frn: Frn::new(10) })
        );

        let invalid_before_missing =
            lookup(vec![record(10, 11, "valid"), record(11, 99, "bad\\name")]);
        assert_eq!(
            resolve_path(&invalid_before_missing, "C:\\", Frn::ROOT, Frn::new(10), 8),
            Err(PathError::InvalidName { frn: Frn::new(11) })
        );
    }

    #[test]
    fn exhaustive_four_node_graphs_match_parent_walker() {
        let names: [(u32, &str); 4] = [(10, "a"), (11, "b"), (12, "c"), (13, "d")];
        let choices = [5u32, 10, 11, 12, 13, 99];
        for p10 in choices {
            for p11 in choices {
                for p12 in choices {
                    for p13 in choices {
                        let mut parents = [0u32; 14];
                        parents[10] = p10;
                        parents[11] = p11;
                        parents[12] = p12;
                        parents[13] = p13;
                        let index = lookup(
                            names
                                .iter()
                                .map(|(frn, name)| {
                                    record(u64::from(*frn), u64::from(parents[*frn as usize]), name)
                                })
                                .collect(),
                        );
                        for start in 10u32..=13u32 {
                            let result = resolve_path(
                                &index,
                                "C:\\",
                                Frn::new(5),
                                Frn::new(u64::from(start)),
                                4,
                            );
                            match pstart_core::walker::walk_parents(&parents, 5, start, 4) {
                                Ok(nodes) => {
                                    let mut expected = String::from("C:");
                                    for node in nodes.iter().rev() {
                                        expected.push('\\');
                                        expected.push_str(
                                            names
                                                .iter()
                                                .find(|(frn, _)| *frn == *node)
                                                .map(|(_, name)| *name)
                                                .unwrap_or(""),
                                        );
                                    }
                                    assert_eq!(
                                        result,
                                        Ok(expected),
                                        "parents={parents:?} start={start}"
                                    );
                                }
                                Err(pstart_core::walker::WalkError::DepthExceeded) => {
                                    assert_eq!(
                                        result,
                                        Err(PathError::DepthExceeded { max: 4 }),
                                        "parents={parents:?} start={start}"
                                    );
                                }
                                Err(pstart_core::walker::WalkError::Cycle { node }) => {
                                    assert!(
                                        matches!(
                                            result,
                                            Err(PathError::Cycle { frn })
                                                if frn.get() == u64::from(node)
                                        ),
                                        "parents={parents:?} start={start}"
                                    );
                                }
                                Err(pstart_core::walker::WalkError::MissingParent { node }) => {
                                    let mut expected_child = start;
                                    let mut cursor = start;
                                    while let Some(parent) = parents.get(cursor as usize).copied() {
                                        if parent == node {
                                            expected_child = cursor;
                                            break;
                                        }
                                        cursor = parent;
                                    }
                                    assert!(
                                        matches!(
                                            result,
                                            Err(PathError::MissingParent { child, parent })
                                                if parent.get() == u64::from(node)
                                                    && child.get() == u64::from(expected_child)
                                        ),
                                        "parents={parents:?} start={start} node={node} expected_child={expected_child}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn assert_graph_matches_walker(
        names: &[(u32, &str)],
        parents: &[u32],
        start: u32,
        max_depth: usize,
    ) {
        let index = lookup(
            names
                .iter()
                .map(|(frn, name)| record(u64::from(*frn), u64::from(parents[*frn as usize]), name))
                .collect(),
        );
        let result = resolve_path(
            &index,
            "C:\\",
            Frn::new(5),
            Frn::new(u64::from(start)),
            max_depth,
        );
        match pstart_core::walker::walk_parents(parents, 5, start, max_depth) {
            Ok(nodes) => {
                let mut expected = String::from("C:");
                for node in nodes.iter().rev() {
                    expected.push('\\');
                    expected.push_str(
                        names
                            .iter()
                            .find(|(frn, _)| *frn == *node)
                            .map(|(_, name)| *name)
                            .unwrap_or(""),
                    );
                }
                assert_eq!(result, Ok(expected), "parents={parents:?} start={start}");
            }
            Err(pstart_core::walker::WalkError::DepthExceeded) => {
                assert_eq!(
                    result,
                    Err(PathError::DepthExceeded { max: max_depth }),
                    "parents={parents:?} start={start}"
                );
            }
            Err(pstart_core::walker::WalkError::Cycle { node }) => {
                assert!(
                    matches!(
                        result,
                        Err(PathError::Cycle { frn }) if frn.get() == u64::from(node)
                    ),
                    "parents={parents:?} start={start}"
                );
            }
            Err(pstart_core::walker::WalkError::MissingParent { node }) => {
                let mut expected_child = start;
                let mut cursor = start;
                while let Some(parent) = parents.get(cursor as usize).copied() {
                    if parent == node {
                        expected_child = cursor;
                        break;
                    }
                    cursor = parent;
                }
                assert!(
                    matches!(
                        result,
                        Err(PathError::MissingParent { child, parent })
                            if parent.get() == u64::from(node)
                                && child.get() == u64::from(expected_child)
                    ),
                    "parents={parents:?} start={start} node={node} expected_child={expected_child}"
                );
            }
        }
    }

    fn enumerate_graphs(
        names: &[(u32, &str)],
        parents: &mut [u32],
        choices: &[u32],
        depth: usize,
        max_depth: usize,
    ) {
        if depth == names.len() {
            for (start, _) in names {
                assert_graph_matches_walker(names, parents, *start, max_depth);
            }
            return;
        }
        for choice in choices {
            parents[names[depth].0 as usize] = *choice;
            enumerate_graphs(names, parents, choices, depth + 1, max_depth);
        }
    }

    #[test]
    fn exhaustive_five_node_graphs_match_parent_walker() {
        let names: [(u32, &str); 5] = [(10, "a"), (11, "b"), (12, "c"), (13, "d"), (14, "e")];
        let mut parents = [0u32; 15];
        enumerate_graphs(&names, &mut parents, &[5, 10, 11, 12, 13, 14, 99], 0, 5);
    }

    #[test]
    fn resolves_nested_path() {
        let index = lookup(vec![
            record(5, 5, "."),
            record(10, 5, "Users"),
            record(11, 10, "aa"),
            record(12, 11, "report.txt"),
        ]);
        let path = resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(12), 64).unwrap();
        assert_eq!(path, "C:\\Users\\aa\\report.txt");
    }

    #[test]
    fn depth_limit_precedes_cycle_detection() {
        let index = lookup(vec![record(20, 21, "a"), record(21, 20, "b")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(20), 1),
            Err(PathError::DepthExceeded { max: 1 })
        );
    }

    #[test]
    fn resolves_direct_child() {
        let index = lookup(vec![record(5, 5, "."), record(6, 5, "file.txt")]);
        assert_eq!(
            resolve_path(&index, "D:", Frn::ROOT, Frn::new(6), 64).unwrap(),
            "D:\\file.txt"
        );
    }

    #[test]
    fn root_itself_resolves_to_root() {
        let index = lookup(vec![record(5, 5, ".")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::ROOT, 64).unwrap(),
            "C:"
        );
    }

    #[test]
    fn root_sequence_is_ignored_during_parent_walk() {
        let root = Frn::new(0x0005_0000_0000_0005);
        let child = Frn::new(0x0005_0000_0000_0010);
        let index = lookup(vec![record(child.get(), root.get(), "Windows")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, child, 64).unwrap(),
            "C:\\Windows"
        );
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, root, 64).unwrap(),
            "C:"
        );
    }

    #[test]
    fn missing_record_is_reported() {
        let index = lookup(vec![]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(99), 64),
            Err(PathError::NotFound { frn: Frn::new(99) })
        );
    }

    #[test]
    fn missing_parent_is_reported() {
        let index = lookup(vec![record(10, 99, "orphan.txt")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(10), 64),
            Err(PathError::MissingParent {
                child: Frn::new(10),
                parent: Frn::new(99)
            })
        );
    }

    #[test]
    fn cycles_are_detected() {
        let index = lookup(vec![record(20, 21, "a"), record(21, 20, "b")]);
        assert!(matches!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(20), 64),
            Err(PathError::Cycle { .. })
        ));
    }

    #[test]
    fn depth_limit_is_enforced() {
        let index = lookup(vec![
            record(5, 5, "."),
            record(6, 5, "a"),
            record(7, 6, "b"),
            record(8, 7, "c"),
        ]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(8), 2),
            Err(PathError::DepthExceeded { max: 2 })
        );
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let index = lookup(vec![record(6, 5, "evil\\name")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(6), 64),
            Err(PathError::InvalidName { frn: Frn::new(6) })
        );
    }

    #[test]
    fn path_length_limit_is_enforced() {
        let long = "x".repeat(250);
        let mut records = vec![record(5, 5, ".")];
        let mut parent = 5;
        for index in 0..20 {
            let frn = 10 + index;
            records.push(record(frn, parent, &long));
            parent = frn;
        }
        let index = lookup(records);
        assert!(matches!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(29), 64),
            Err(PathError::PathTooLong { .. })
        ));
    }

    #[test]
    fn invalid_root_is_rejected() {
        let index = lookup(vec![]);
        assert_eq!(
            resolve_path(&index, "", Frn::ROOT, Frn::new(1), 8),
            Err(PathError::InvalidRoot(String::new()))
        );
        assert!(matches!(
            resolve_path(&index, "bad\0root", Frn::ROOT, Frn::new(1), 8),
            Err(PathError::InvalidRoot(_))
        ));
    }

    #[test]
    fn zero_depth_is_rejected() {
        let index = lookup(vec![]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(1), 0),
            Err(PathError::DepthExceeded { max: 0 })
        );
    }

    #[test]
    fn separator_only_root_is_preserved() {
        let index = lookup(vec![]);
        assert_eq!(
            resolve_path(&index, "\\\\", Frn::ROOT, Frn::ROOT, 8).unwrap(),
            "\\"
        );
    }

    #[test]
    fn root_only_path_is_length_bounded() {
        let index = lookup(vec![]);
        let root = "x".repeat(MAX_PATH_UTF16 + 1);
        assert!(matches!(
            resolve_path(&index, &root, Frn::ROOT, Frn::ROOT, 8),
            Err(PathError::PathTooLong { .. })
        ));
    }

    #[test]
    fn self_parent_outside_root_is_a_cycle() {
        let index = lookup(vec![record(20, 20, "loop")]);
        assert_eq!(
            resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(20), 8),
            Err(PathError::Cycle { frn: Frn::new(20) })
        );
    }

    proptest! {
        #[test]
        fn arbitrary_chain_resolves_without_panic(depth in 1usize..=6) {
            let mut records = vec![record(5, 5, ".")];
            let mut parent = 5;
            for index in 0..depth {
                let frn = 10 + index as u64;
                records.push(record(frn, parent, &format!("n{index}")));
                parent = frn;
            }
            let leaf = 10 + depth as u64 - 1;
            let path = resolve_path(&lookup(records), "C:", Frn::ROOT, Frn::new(leaf), 8)
                .expect("valid chain resolves");
            let expected_suffix = format!("n{}", depth - 1);
            prop_assert!(path.starts_with("C:\\"));
            prop_assert!(path.ends_with(&expected_suffix));
        }

        #[test]
        fn arbitrary_parent_graph_matches_walker(
            parents in prop::collection::vec(0u32..70, 61),
            start in 10usize..60,
            max_depth in 1usize..40,
        ) {
            let records: Vec<FileRecord> = (0u64..=60)
                .map(|frn| record(frn, u64::from(parents[frn as usize]), &format!("n{frn}")))
                .collect();
            let expected = walk_parents(&parents, 5, start as u32, max_depth);
            let actual = resolve_path(
                &lookup(records),
                "C:\\",
                Frn::ROOT,
                Frn::new(start as u64),
                max_depth,
            );
            match (actual, expected) {
                (Ok(path), Ok(nodes)) => {
                    let mut expected_path = String::from("C:");
                    for node in nodes.iter().rev() {
                        expected_path.push('\\');
                        expected_path.push_str(&format!("n{node}"));
                    }
                    prop_assert_eq!(path, expected_path);
                }
                (Err(PathError::DepthExceeded { max }), Err(WalkError::DepthExceeded)) => {
                    prop_assert_eq!(max, max_depth);
                }
                (Err(PathError::Cycle { frn }), Err(WalkError::Cycle { node })) => {
                    prop_assert_eq!(frn, Frn::new(u64::from(node)));
                }
                (
                    Err(PathError::MissingParent { parent, .. }),
                    Err(WalkError::MissingParent { node }),
                ) => {
                    prop_assert_eq!(&parent, &Frn::new(u64::from(node)));
                }
                (actual, expected) => {
                    prop_assert!(false, "result mismatch: {:?} vs {:?}", actual, expected);
                }
            }
        }

        #[test]
        fn arbitrary_two_node_cycle_is_detected(start in 10u64..1000) {
            let index = lookup(vec![
                record(start, start + 1, "a"),
                record(start + 1, start, "b"),
            ]);
            let cycle = matches!(
                resolve_path(&index, "C:", Frn::ROOT, Frn::new(start), 8),
                Err(PathError::Cycle { .. })
            );
            prop_assert!(cycle);
        }
    }
}
