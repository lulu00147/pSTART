use super::trigram::*;
use super::*;
use pstart_core::text::contains_case_insensitive;

fn record(frn: u64, parent: u64, name: &str, usn: u64) -> FileRecord {
    FileRecord {
        frn,
        parent_frn: parent,
        name: name.into(),
        attributes: 0,
        is_dir: false,
        last_usn: usn,
    }
}

fn index() -> MemoryIndex {
    MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 64).unwrap()
}

fn populated() -> MemoryIndex {
    let mut index = index();
    for value in [
        record(5, 5, ".", 1),
        record(10, 5, "Users", 2),
        record(11, 10, "Documents", 3),
        record(12, 11, "Report.docx", 4),
        record(13, 11, "annual-report.txt", 5),
        record(14, 11, "myreport.txt", 6),
        record(15, 11, "Report", 7),
    ] {
        assert!(index.upsert(value).unwrap());
    }
    index
}

#[test]
fn inserts_and_resolves_paths() {
    let index = populated();
    assert_eq!(index.len(), 7);
    assert_eq!(
        index.resolve_record_path(Frn::new(12)).unwrap(),
        "C:\\Users\\Documents\\Report.docx"
    );
    index.assert_consistent();
}

#[test]
fn consistency_error_detects_signature_mismatch() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slot_signatures[slot] ^= 1;
    assert_eq!(index.consistency_error(), Some("slot signature mismatch"));
}

#[test]
fn consistency_error_detects_signature_count_mismatch() {
    let mut index = populated();
    index.slot_signatures.pop();
    assert_eq!(
        index.consistency_error(),
        Some("slot signature count mismatch")
    );
}

#[test]
fn consistency_error_detects_quad_signature_mismatch() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slot_quad_signatures[slot] ^= 1;
    assert_eq!(
        index.consistency_error(),
        Some("slot quadgram signature mismatch")
    );
}

#[test]
fn consistency_error_detects_quad_signature_count_mismatch() {
    let mut index = populated();
    index.slot_quad_signatures.pop();
    assert_eq!(
        index.consistency_error(),
        Some("slot quadgram signature count mismatch")
    );
}

#[test]
fn consistency_error_detects_missing_bucket() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.names.forget(slot as u32);
    assert_eq!(
        index.consistency_error(),
        Some("frn count does not match name buckets")
    );
}

#[test]
fn consistency_error_detects_count_mismatch() {
    let mut index = populated();
    index.by_frn.clear();
    assert_eq!(
        index.consistency_error(),
        Some("frn count does not match name buckets")
    );
}

#[test]
fn consistency_error_detects_free_slot() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    assert_eq!(index.consistency_error(), Some("frn points at a free slot"));
}

#[test]
fn consistency_error_detects_slot_frn_mismatch() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slots[slot].frn = 99;
    assert_eq!(index.consistency_error(), Some("slot frn mismatch"));
}

#[test]
fn consistency_error_detects_bucket_without_slot() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    let other = index.by_frn.get(13).unwrap() as usize;
    index.names.forget(slot as u32);
    index
        .names
        .insert(other as u32, index.slots[other].name_offset);
    assert_eq!(
        index.consistency_error(),
        Some("slot missing from name bucket")
    );
}

#[test]
fn missing_start_slot_reports_start_as_parent() {
    let mut index = index();
    assert!(index.upsert(record(12, 5, "a", 1)).unwrap());
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    assert_eq!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent {
            child: Frn::new(12),
            parent: Frn::new(12),
        })
    );
}

#[test]
fn search_skips_stale_name_bucket() {
    let mut index = index();
    assert!(index.upsert(record(10, 5, "stale.txt", 1)).unwrap());
    let slot = index.by_frn.get(10).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    assert!(index.search("stale", 10).hits.is_empty());
}

#[test]
fn root_sequence_variant_is_treated_as_root() {
    let mut index = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 64).unwrap();
    let root_variant = Frn::new(0x0005_0000_0000_0005);
    assert!(index
        .upsert(record(10, root_variant.get(), "a", 1))
        .unwrap());
    assert_eq!(index.resolve_record_path(Frn::new(10)).unwrap(), "C:\\a");
}

#[test]
fn missing_parent_slot_falls_back_without_panic() {
    let mut index = index();
    assert!(index.upsert(record(10, 5, "a", 1)).unwrap());
    let slot = index.by_frn.get(10).unwrap() as usize;
    index.parent_slots[slot] = MISSING_SLOT;
    index.missing_parents.insert(slot as u32, 5);
    assert_eq!(index.resolve_record_path(Frn::new(10)).unwrap(), "C:\\a");
}

#[test]
fn deleting_last_child_removes_parent_bucket() {
    let mut index = index();
    assert!(index.upsert(record(10, 5, "dir", 1)).unwrap());
    assert!(index.upsert(record(11, 10, "file", 2)).unwrap());
    assert!(index.delete(Frn::new(11), 3).unwrap());
    assert!(!index.children.contains_key(&10));
}

#[test]
fn missing_cycle_slot_reports_cycle_node() {
    let mut index = index();
    assert!(index.upsert(record(12, 20, "c", 1)).unwrap());
    assert!(index.upsert(record(20, 21, "a", 2)).unwrap());
    assert!(index.upsert(record(21, 20, "b", 3)).unwrap());
    let slot = index.by_frn.get(20).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    // Slot graph still detects the cycle before slot validation.
    assert_eq!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent {
            child: Frn::new(12),
            parent: Frn::new(20)
        })
    );
}

#[test]
fn invalid_name_before_cycle_matches_generic_path() {
    let mut index = index();
    assert!(index.upsert(record(10, 11, "a", 1)).unwrap());
    assert!(index.upsert(record(11, 10, "b", 2)).unwrap());
    assert!(index.upsert(record(12, 11, "c", 3)).unwrap());
    let slot = index.by_frn.get(10).unwrap() as usize;
    index.force_slot_name(slot, "bad/name");
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_NAME_VALID;
    let indexed = index.resolve_record_path(Frn::new(12));
    let generic =
        crate::path::resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(12), index.max_depth);
    assert_eq!(indexed, Err(PathError::InvalidName { frn: Frn::new(10) }));
    assert_eq!(indexed, generic);
}

#[test]
fn consistency_error_detects_name_validity_count_mismatch() {
    let mut index = populated();
    index.slot_flags.pop();
    assert_eq!(index.consistency_error(), Some("slot state count mismatch"));
}

#[test]
fn consistency_error_detects_presence_count_mismatch() {
    let mut index = populated();
    index.slot_flags.pop();
    assert_eq!(index.consistency_error(), Some("slot state count mismatch"));
}

#[test]
fn consistency_error_detects_presence_and_name_validity_mismatch() {
    let mut index = populated();
    let slot = index.by_frn.get(12).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    assert_eq!(index.consistency_error(), Some("frn points at a free slot"));
    index.slot_flags[slot] |= pstart_core::walker::SLOT_PRESENT;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_NAME_VALID;
    assert_eq!(
        index.consistency_error(),
        Some("slot name validity mismatch")
    );
}

#[test]
fn absent_slot_presence_reports_missing_parent() {
    let mut index = index();
    assert!(index.upsert(record(10, 5, "a", 1)).unwrap());
    assert!(index.upsert(record(11, 10, "b", 2)).unwrap());
    assert!(index.upsert(record(12, 11, "c", 3)).unwrap());
    let slot = index.by_frn.get(11).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    match index.resolve_record_path(Frn::new(12)) {
        Err(PathError::MissingParent { child, .. }) => {
            assert_eq!(child, Frn::new(12));
        }
        other => panic!("expected missing parent, got {other:?}"),
    }
}

#[test]
fn missing_parent_slot_reports_child_and_parent() {
    let mut index = index();
    assert!(index.upsert(record(12, 11, "c", 1)).unwrap());
    assert!(index.upsert(record(11, 5, "b", 2)).unwrap());
    let slot = index.by_frn.get(11).unwrap() as usize;
    index.slot_flags[slot] &= !pstart_core::walker::SLOT_PRESENT;
    assert_eq!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent {
            child: Frn::new(12),
            parent: Frn::new(11),
        })
    );
}

#[test]
fn missing_parent_reports_immediate_child_and_parent() {
    let mut index = index();
    assert!(index.upsert(record(12, 11, "c", 1)).unwrap());
    assert_eq!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent {
            child: Frn::new(12),
            parent: Frn::new(11),
        })
    );
    assert!(index.upsert(record(11, 10, "b", 2)).unwrap());
    assert_eq!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent {
            child: Frn::new(11),
            parent: Frn::new(10),
        })
    );
}

#[test]
fn child_before_parent_is_resolved_after_parent_insert() {
    let mut index = index();
    assert!(index.upsert(record(12, 11, "c", 1)).unwrap());
    assert!(matches!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent { .. })
    ));
    assert!(index.upsert(record(11, 10, "b", 2)).unwrap());
    assert!(matches!(
        index.resolve_record_path(Frn::new(12)),
        Err(PathError::MissingParent { .. })
    ));
    assert!(index.upsert(record(10, 5, "a", 3)).unwrap());
    assert_eq!(
        index.resolve_record_path(Frn::new(12)).unwrap(),
        "C:\\a\\b\\c"
    );
    index.assert_consistent();
}

#[test]
fn move_and_delete_update_parent_slots() {
    let mut index = index();
    for value in [
        record(10, 5, "a", 1),
        record(11, 10, "b", 2),
        record(12, 11, "c", 3),
    ] {
        assert!(index.upsert(value).unwrap());
    }
    assert_eq!(
        index.resolve_record_path(Frn::new(12)).unwrap(),
        "C:\\a\\b\\c"
    );
    assert!(index.upsert(record(12, 5, "c", 4)).unwrap());
    assert_eq!(index.resolve_record_path(Frn::new(12)).unwrap(), "C:\\c");
    assert!(index.delete(Frn::new(10), 5).unwrap());
    assert_eq!(
        index.resolve_record_path(Frn::new(11)),
        Err(PathError::MissingParent {
            child: Frn::new(11),
            parent: Frn::new(10),
        })
    );
    assert!(index.upsert(record(10, 5, "a", 6)).unwrap());
    assert_eq!(index.resolve_record_path(Frn::new(11)).unwrap(), "C:\\a\\b");
    index.assert_consistent();
}

#[test]
fn resolve_reports_missing_depth_cycle_and_root_slash() {
    let base = index();
    assert!(matches!(
        base.resolve_record_path(Frn::new(99)),
        Err(PathError::NotFound { .. })
    ));

    let mut depth = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 2).unwrap();
    for value in [
        record(10, 5, "a", 1),
        record(11, 10, "b", 2),
        record(12, 11, "c", 3),
    ] {
        depth.upsert(value).unwrap();
    }
    assert!(matches!(
        depth.resolve_record_path(Frn::new(12)),
        Err(PathError::DepthExceeded { max: 2 })
    ));

    let mut cycle = base;
    cycle.upsert(record(20, 21, "a", 1)).unwrap();
    cycle.upsert(record(21, 20, "b", 2)).unwrap();
    assert!(matches!(
        cycle.resolve_record_path(Frn::new(20)),
        Err(PathError::Cycle { .. })
    ));

    let mut slash = MemoryIndex::new(VolumeId::new("C:").unwrap(), "\\", Frn::ROOT, 8).unwrap();
    slash.upsert(record(10, 5, "a", 1)).unwrap();
    assert_eq!(slash.resolve_record_path(Frn::new(10)).unwrap(), "\\a");
}

#[test]
fn resolve_rejects_too_long_path() {
    let mut index = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 200).unwrap();
    let name = "x".repeat(255);
    let mut parent = 5;
    for value in 0..130 {
        let frn = 100 + value;
        index.upsert(record(frn, parent, &name, value + 1)).unwrap();
        parent = frn;
    }
    assert!(matches!(
        index.resolve_record_path(Frn::new(parent)),
        Err(PathError::PathTooLong { .. })
    ));
}

#[test]
fn generic_lookup_and_indexed_path_agree() {
    let index = populated();
    let generic =
        crate::path::resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(12), index.max_depth)
            .unwrap();
    assert_eq!(generic, index.resolve_record_path(Frn::new(12)).unwrap());
}

#[test]
fn root_variants_match_generic_path() {
    for root in ["C:", "C:\\", "C:/", "/", "\\"] {
        let mut index =
            MemoryIndex::new(VolumeId::new("C:").unwrap(), root, Frn::ROOT, 64).unwrap();
        index.upsert(record(10, 5, "a", 1)).unwrap();
        let indexed = index.resolve_record_path(Frn::new(10));
        let generic =
            crate::path::resolve_path(&index, root, Frn::ROOT, Frn::new(10), index.max_depth);
        assert_eq!(indexed, generic, "root {root:?}");
    }
}

#[test]
fn indexed_and_generic_paths_agree_on_invalid_names() {
    for name in [
        String::new(),
        "a/b".to_owned(),
        "a\\b".to_owned(),
        "\0".to_owned(),
        "x".repeat(300),
    ] {
        let mut index = index();
        index.upsert(record(12, 5, "base", 1)).unwrap();
        let slot = index.by_frn.get(12).unwrap() as usize;
        index.force_slot_name(slot, &name);
        let indexed = index.resolve_record_path(Frn::new(12));
        let generic =
            crate::path::resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(12), index.max_depth);
        assert_eq!(indexed, generic, "name length {}", name.len());
    }
}

#[test]
fn corrupted_stored_state_is_reported_not_ignored() {
    let mut index = index();
    index.upsert(record(10, 5, "a", 1)).unwrap();
    let slot = index.by_frn.get(10).unwrap() as usize;
    index.force_slot_name(slot, "bad/name");
    assert!(matches!(
        index.resolve_record_path(Frn::new(10)),
        Err(PathError::InvalidName { .. })
    ));
    index.force_slot_name(slot, "a");
    index.children.remove(&5);
    assert!(index.delete(Frn::new(10), 2).unwrap());
}

proptest! {
    #[test]
    fn indexed_paths_agree_with_generic_walker(
        operations in proptest::collection::vec(
            (0u8..16, 6u64..96, 6u64..96),
            0..60,
        ),
    ) {
        let mut index = index();
        for (kind, frn, parent) in operations {
            if kind % 5 == 0 {
                let _ = index.delete(Frn::new(frn), 1_000_000);
            } else {
                let name = format!("n{frn}");
                let _ = index.upsert(record(frn, parent, &name, u64::from(kind) + 1));
            }
        }
        for frn in 6u64..=95 {
            let indexed = index.resolve_record_path(Frn::new(frn));
            let generic = crate::path::resolve_path(
                &index,
                "C:\\",
                Frn::ROOT,
                Frn::new(frn),
                index.max_depth,
            );
            match (indexed, generic) {
                (Ok(left), Ok(right)) => prop_assert_eq!(left, right),
                (Err(left), Err(right)) => prop_assert_eq!(left, right),
                (left, right) => prop_assert!(
                    false,
                    "result mismatch for frn {frn}: indexed={left:?} generic={right:?}"
                ),
            }
        }
    }
}

#[test]
fn search_ranks_and_resolves_paths() {
    let outcome = populated().search("report", 10);
    let names: Vec<&str> = outcome
        .hits
        .iter()
        .map(|hit| hit.item.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["Report", "Report.docx", "annual-report.txt", "myreport.txt"]
    );
    assert_eq!(
        outcome.hits[1].item.path,
        "C:\\Users\\Documents\\Report.docx"
    );
    assert!(!outcome.truncated);
}

#[test]
fn shallower_paths_rank_first_on_equal_names() {
    let mut index = index();
    for value in [
        record(10, 5, "Users", 1),
        record(11, 10, "Documents", 2),
        record(100, 5, "Report.txt", 3),
        record(101, 11, "Report.txt", 4),
    ] {
        assert!(index.upsert(value).unwrap());
    }
    let outcome = index.search("report", 10);
    assert_eq!(outcome.hits.len(), 2);
    assert_eq!(outcome.hits[0].item.path, "C:\\Report.txt");
    assert_eq!(
        outcome.hits[1].item.path,
        "C:\\Users\\Documents\\Report.txt"
    );
    assert!(outcome.hits[0].score > outcome.hits[1].score);
}

#[test]
fn files_rank_before_directories_on_equal_names() {
    let mut index = index();
    let mut directory = record(100, 5, "Report", 1);
    directory.is_dir = true;
    assert!(index.upsert(directory).unwrap());
    assert!(index.upsert(record(101, 5, "Report", 2)).unwrap());
    let outcome = index.search("report", 10);
    assert_eq!(outcome.hits.len(), 2);
    assert!(!outcome.hits[0].item.is_dir);
    assert!(outcome.hits[1].item.is_dir);
    assert!(outcome.hits[0].score > outcome.hits[1].score);
}

#[test]
fn search_limit_is_bounded() {
    let outcome = populated().search("report", 2);
    assert_eq!(outcome.hits.len(), 2);
    let outcome = populated().search("report", u32::MAX);
    assert!(outcome.hits.len() <= MAX_RESULTS as usize);
}

#[test]
fn empty_query_returns_no_hits() {
    assert!(populated().search("", 10).hits.is_empty());
    assert!(populated().search("   ", 10).hits.is_empty());
    assert!(populated().search("report", 0).hits.is_empty());
}

#[test]
fn case_insensitive_prefilter_matches_lowercase_semantics() {
    assert!(contains_case_insensitive("REPORT.TXT", "report"));
    assert!(contains_case_insensitive("项目报告.docx", "报告"));
    assert!(!contains_case_insensitive("abc", "x"));
    assert!(contains_case_insensitive("abc", ""));
}

#[test]
fn stale_updates_are_ignored() {
    let mut index = populated();
    assert!(!index.upsert(record(12, 11, "OldName.txt", 1)).unwrap());
    assert_eq!(
        index.record(Frn::new(12)).unwrap().name.as_ref(),
        "Report.docx"
    );
    index.assert_consistent();
}

#[test]
fn rename_updates_name_index_and_path() {
    let mut index = populated();
    assert!(index.upsert(record(12, 11, "Renamed.txt", 10)).unwrap());
    assert!(index.search("Report.docx", 10).hits.is_empty());
    assert_eq!(index.search("Renamed", 10).hits.len(), 1);
    assert_eq!(
        index.resolve_record_path(Frn::new(12)).unwrap(),
        "C:\\Users\\Documents\\Renamed.txt"
    );
    index.assert_consistent();
}

#[test]
fn move_updates_parent_and_path() {
    let mut index = populated();
    assert!(index.upsert(record(12, 10, "Report.docx", 10)).unwrap());
    assert_eq!(
        index.resolve_record_path(Frn::new(12)).unwrap(),
        "C:\\Users\\Report.docx"
    );
}

#[test]
fn delete_removes_from_all_indexes() {
    let mut index = populated();
    assert!(index.delete(Frn::new(12), 10).unwrap());
    assert!(index.record(Frn::new(12)).is_none());
    assert!(index.search("Report.docx", 10).hits.is_empty());
    index.assert_consistent();
}

#[test]
fn stale_delete_is_ignored() {
    let mut index = populated();
    assert!(!index.delete(Frn::new(12), 1).unwrap());
    assert!(index.record(Frn::new(12)).is_some());
}

#[test]
fn deleted_slots_are_reused() {
    let mut index = populated();
    for usn in 20..120 {
        assert!(index
            .upsert(record(1000 + usn, 11, "temp.txt", usn))
            .unwrap());
        assert!(index.delete(Frn::new(1000 + usn), usn + 1).unwrap());
    }
    assert!(index.slots.len() <= 9);
    index.assert_consistent();
}

#[test]
fn orphan_results_are_skipped() {
    let mut index = index();
    index.upsert(record(5, 5, ".", 1)).unwrap();
    index.upsert(record(20, 99, "orphan.txt", 2)).unwrap();
    assert!(index.search("orphan", 10).hits.is_empty());
}

#[test]
fn duplicate_names_are_allowed() {
    let mut index = populated();
    index.upsert(record(30, 11, "report", 8)).unwrap();
    let outcome = index.search("report", 10);
    assert_eq!(
        outcome
            .hits
            .iter()
            .filter(|hit| hit.item.name.eq_ignore_ascii_case("report"))
            .count(),
        2
    );
}

#[test]
fn deleting_primary_duplicate_promotes_overflow_slot() {
    let mut index = index();
    index.upsert(record(10, 5, "dup", 1)).unwrap();
    index.upsert(record(11, 5, "dup", 2)).unwrap();
    assert!(index.delete(Frn::new(10), 3).unwrap());
    index.assert_consistent();
    let outcome = index.search("dup", 10);
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].item.name, "dup");
}

#[test]
fn invalid_records_are_rejected() {
    let mut index = index();
    assert!(index.upsert(record(0, 5, "zero", 1)).is_err());
    assert!(index.upsert(record(10, 5, "bad\\name", 1)).is_err());
    assert!(index.upsert(record(10, 5, "", 1)).is_err());
    assert!(index.delete(Frn::ROOT, 1).is_err());
}

#[test]
fn metadata_accessors_report_configuration_and_size() {
    let mut index = index();
    assert_eq!(index.volume().as_str(), "C:");
    assert_eq!(index.root(), "C:\\");
    assert_eq!(index.root_frn(), Frn::ROOT);
    assert!(index.is_empty());
    index.upsert(record(10, 5, "a.txt", 1)).unwrap();
    assert_eq!(index.len(), 1);
    assert!(!index.is_empty());
}

#[test]
fn invalid_configuration_is_rejected() {
    assert!(MemoryIndex::new(VolumeId::new("C:").unwrap(), "", Frn::ROOT, 4).is_err());
    assert!(MemoryIndex::new(VolumeId::new("C:").unwrap(), "bad\0root", Frn::ROOT, 4).is_err());
    assert!(MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 0).is_err());
}

#[test]
fn apply_dispatches_upsert_and_delete() {
    let mut index = index();
    assert!(index
        .apply(IndexEvent::Upsert(record(10, 5, "a.txt", 1)))
        .unwrap());
    assert_eq!(index.len(), 1);
    assert!(index.apply(IndexEvent::Delete { frn: 10, usn: 2 }).unwrap());
    assert!(index.is_empty());
}

#[test]
fn deleting_unknown_record_is_a_noop() {
    let mut index = index();
    assert!(!index.delete(Frn::new(999), 1).unwrap());
}

#[test]
fn exact_duplicate_is_a_noop() {
    let mut index = index();
    assert!(index.upsert(record(10, 5, "a.txt", 1)).unwrap());
    assert!(!index.upsert(record(10, 5, "a.txt", 1)).unwrap());
    assert_eq!(index.len(), 1);
}

#[test]
fn top_k_falls_back_after_path_error() {
    let mut index = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 2).unwrap();
    assert!(index.upsert(record(10, 5, "b", 1)).unwrap());
    assert!(index.upsert(record(11, 10, "c", 2)).unwrap());
    assert!(index.upsert(record(12, 11, "d", 3)).unwrap());
    assert!(index.upsert(record(13, 12, "e", 4)).unwrap());
    assert!(index.upsert(record(14, 13, "a", 5)).unwrap());
    assert!(index.upsert(record(20, 5, "a2", 6)).unwrap());
    assert!(index.resolve_record_path(Frn::new(14)).is_err());

    let outcome = index.search("a", 1);
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].item.name, "a2");
    assert!(outcome.truncated);
}

#[test]
fn prefix_top_k_matches_full_ranking() {
    let names = [
        "a",
        "ab",
        "az",
        "a123456789",
        "a-very-long-name-12345",
        "another-long-prefix-name",
        "a-short",
        "a-mid-length",
        "a00",
        "a-zzzzzzzzzzzz",
    ];
    let mut index = index();
    for (position, name) in names.into_iter().enumerate() {
        assert!(index
            .upsert(record(100 + position as u64, 5, name, position as u64))
            .unwrap());
    }
    let limit = 4usize;
    let outcome = index.search("a", limit as u32);
    let mut expected: Vec<(i32, &str)> = names
        .iter()
        .map(|name| {
            let base = if *name == "a" {
                SCORE_EXACT
            } else {
                SCORE_PREFIX
            };
            (apply_penalty(base, name.chars().count()), *name)
        })
        .collect();
    expected.sort_by(|(left_score, left_name), (right_score, right_name)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_name.cmp(right_name))
    });
    let expected: Vec<&str> = expected
        .into_iter()
        .take(limit)
        .map(|(_, name)| name)
        .collect();
    let actual: Vec<&str> = outcome
        .hits
        .iter()
        .map(|hit| hit.item.name.as_str())
        .collect();
    assert_eq!(actual, expected);
    assert!(!outcome.truncated);
}

#[test]
fn prefix_top_k_preserves_best_names() {
    let mut index = index();
    for (position, name) in [
        "a",
        "ab",
        "az",
        "a-very-long-name-12345",
        "another-long-prefix-name",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(index
            .upsert(record(100 + position as u64, 5, name, position as u64))
            .unwrap());
    }
    let outcome = index.search("a", 3);
    let names: Vec<&str> = outcome
        .hits
        .iter()
        .map(|hit| hit.item.name.as_str())
        .collect();
    assert_eq!(names, ["a", "ab", "az"]);
    assert!(!outcome.truncated);
}

#[test]
fn prefix_candidate_budget_marks_truncation() {
    let mut index = index();
    for position in 0..=MAX_PREFIX_CANDIDATES {
        assert!(index
            .upsert(record(
                100 + position as u64,
                5,
                &format!("a{position:04}.txt"),
                1,
            ))
            .unwrap());
    }
    let outcome = index.search("a", 1);
    assert!(outcome.truncated);
    assert_eq!(outcome.hits.len(), 1);
}

#[test]
fn substring_scan_budget_marks_truncation() {
    let mut index = index();
    for position in 0..=MAX_SUBSTRING_SCAN {
        assert!(index
            .upsert(record(
                100 + position as u64,
                5,
                &format!("zz{position:04}.txt"),
                1,
            ))
            .unwrap());
    }
    let outcome = index.search("does-not-match", 1);
    assert!(!outcome.truncated);
    assert!(outcome.hits.is_empty());
    // Force the filter positive so this test still exercises the scan budget.
    index.mark_ngrams("does-not-match");
    let outcome = index.search("does-not-match", 1);
    assert!(outcome.truncated);
    assert!(outcome.hits.is_empty());
}

#[test]
fn quadgram_filter_rejects_absent_four_byte_substring() {
    let mut index = index();
    assert!(index.upsert(record(100, 5, "alpha.txt", 1)).unwrap());
    assert!(index.might_contain_substring("alpha"));
    assert!(!index.might_contain_substring("zzzz"));
}

#[test]
fn rename_and_delete_update_slot_signature() {
    let mut index = index();
    assert!(index.upsert(record(100, 5, "oldreport.txt", 1)).unwrap());
    assert!(!index.search("oldreport", 10).hits.is_empty());

    assert!(index.upsert(record(100, 5, "newreport.txt", 2)).unwrap());
    assert!(index.search("oldreport", 10).hits.is_empty());
    assert!(!index.search("newreport", 10).hits.is_empty());

    assert!(index.delete(Frn::new(100), 3).unwrap());
    assert!(index.search("newreport", 10).hits.is_empty());
}

#[test]
fn chinese_prefix_substring_and_path_work() {
    let mut index = index();
    assert!(index.upsert(record(100, 5, "项目报告.docx", 1)).unwrap());
    assert!(index.upsert(record(101, 5, "年度报告.txt", 2)).unwrap());
    let prefix = index.search("项目", 10);
    assert_eq!(prefix.hits.len(), 1);
    assert_eq!(prefix.hits[0].item.name, "项目报告.docx");
    let substring = index.search("报告", 10);
    assert_eq!(substring.hits.len(), 2);
    assert_eq!(
        index.resolve_record_path(Frn::new(100)).unwrap(),
        "C:\\项目报告.docx"
    );
}

#[test]
fn trigram_filter_keeps_all_substring_hits() {
    let mut index = index();
    for (position, name) in ["notepad.exe", "项目报告.docx", "annual-report.txt"]
        .into_iter()
        .enumerate()
    {
        assert!(index
            .upsert(record(100 + position as u64, 5, name, 1))
            .unwrap());
    }
    for query in ["notepad", "报告", "report", "pad.exe"] {
        assert!(
            index.might_contain_substring(&query.to_lowercase()),
            "false negative for {query}"
        );
        let outcome = index.search(query, 10);
        assert!(
            !outcome.hits.is_empty(),
            "search lost substring hit for {query}"
        );
    }
}

#[test]
fn dangling_slots_are_reported_or_skipped() {
    let mut index = index();
    index.by_frn.insert(999, 12_345);
    assert!(index.upsert(record(999, 5, "x.txt", 1)).is_err());
    assert!(index.delete(Frn::new(999), 1).is_err());
    index.names.insert(12_345, 1);
    let outcome = index.search("ghost", 1);
    assert!(outcome.hits.is_empty());
}

#[test]
fn removing_an_absent_name_entry_is_a_noop() {
    let mut index = index();
    index.names.forget(0);
    index.assert_consistent();
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn trigram_prefilters_keep_generated_substrings(
        prefix in "[a-zA-Z0-9._ -]{0,8}",
        query in "[a-zA-Z0-9]{3,8}",
        suffix in "[a-zA-Z0-9._ -]{0,8}",
    ) {
        let name = format!("{prefix}{query}{suffix}");
        let mut index = index();
        index.upsert(record(100, 5, &name, 1)).unwrap();
        let needle = query.to_lowercase();
        prop_assert!(index.might_contain_substring(&needle));
        let outcome = index.search(&query, 10);
        prop_assert!(!outcome.hits.is_empty());
    }

    #[test]
    fn quadgram_signature_keeps_contained_query(
        prefix in "[a-zA-Z0-9._ -]{0,8}",
        query in "[a-zA-Z0-9]{4,10}",
        suffix in "[a-zA-Z0-9._ -]{0,8}",
    ) {
        let name = format!("{prefix}{query}{suffix}").to_lowercase();
        let needle = query.to_lowercase();
        let name_signature = quadgram_signature_bytes(name.as_bytes());
        let query_signature = quadgram_signature_bytes(needle.as_bytes());
        prop_assert_eq!(name_signature & query_signature, query_signature);
    }

    #[test]
    fn arbitrary_event_sequences_keep_index_consistent(
        events in prop::collection::vec(
            (1u64..30, 0u64..30, "[a-z]{1,8}", 0u64..1000, any::<bool>()),
            0..120,
        )
    ) {
        let mut index = index();
        index.upsert(record(5, 5, ".", 1)).unwrap();
        for (position, (frn, parent, name, usn, is_dir)) in events.into_iter().enumerate() {
            if frn == 5 {
                continue;
            }
            let parent = if parent == 0 { 5 } else { parent };
            let value = FileRecord {
                frn,
                parent_frn: parent,
                name: format!("{name}{position}").into(),
                attributes: 0,
                is_dir,
                last_usn: usn,
            };
            let _ = index.upsert(value);
            index.assert_consistent();
        }
    }

    #[test]
    fn top_k_agrees_with_full_prefix_ranking(
        names in prop::collection::vec("[a-z]{1,8}", 1..=10),
        limit in 1u32..=4,
    ) {
        let mut index = index();
        let mut inserted = Vec::new();
        for (position, suffix) in names.into_iter().enumerate() {
            let name = format!("a{suffix}{position}");
            assert!(index
                .upsert(record(100 + position as u64, 5, &name, position as u64))
                .unwrap());
            inserted.push(name);
        }
        let outcome = index.search("a", limit);
        let mut expected: Vec<(i32, &str)> = inserted
            .iter()
            .map(|name| {
                (
                    apply_penalty(SCORE_PREFIX, name.chars().count()),
                    name.as_str(),
                )
            })
            .collect();
        expected.sort_by(|(left_score, left_name), (right_score, right_name)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_name.cmp(right_name))
        });
        let expected: Vec<&str> = expected
            .into_iter()
            .take(limit as usize)
            .map(|(_, name)| name)
            .collect();
        let actual: Vec<&str> = outcome
            .hits
            .iter()
            .map(|hit| hit.item.name.as_str())
            .collect();
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn arbitrary_deletes_keep_index_consistent(
        events in prop::collection::vec((1u64..30, 0u64..1000), 0..120)
    ) {
        let mut index = index();
        index.upsert(record(5, 5, ".", 1)).unwrap();
        for (frn, usn) in events {
            if frn == 5 {
                continue;
            }
            let _ = index.upsert(record(frn, 5, "candidate.txt", usn));
            let _ = index.delete(Frn::new(frn), usn);
            index.assert_consistent();
        }
    }
}
