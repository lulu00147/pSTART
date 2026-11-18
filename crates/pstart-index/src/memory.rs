//! In-memory index with bounded search and FRN path reconstruction.

use std::collections::{BTreeMap, HashMap};

use pstart_core::limits::{MAX_PATH_UTF16, MAX_RESULTS};
use pstart_core::{FileId, FileItem, Frn, VolumeId};
use pstart_score::apply_penalty;

use crate::error::IndexError;
use crate::model::{FileRecord, IndexEvent};
use crate::path::{PathError, RecordLookup};
use crate::score::{
    score_name, SCORE_DEPTH_PENALTY_MAX, SCORE_DIRECTORY_PENALTY, SCORE_EXACT, SCORE_PREFIX,
};
use pstart_core::text::contains_lowercase_needle;
use pstart_core::walker::{walk_named, NamedWalkError};

/// Maximum number of prefix candidates inspected for one query.
#[cfg(not(test))]
pub const MAX_PREFIX_CANDIDATES: usize = 20_000;
/// Test-sized prefix candidate budget, so the cap path is cheap to exercise.
#[cfg(test)]
pub const MAX_PREFIX_CANDIDATES: usize = 10;

/// Maximum names inspected and records examined by the substring fallback.
#[cfg(not(test))]
pub const MAX_SUBSTRING_SCAN: usize = 200_000;
/// Test-sized substring scan budget, so the cap path is cheap to exercise.
#[cfg(test)]
pub const MAX_SUBSTRING_SCAN: usize = 100;

/// Sentinel for a record whose parent is the volume root.
const ROOT_SLOT: u32 = u32::MAX;
/// Sentinel for a parent FRN not currently present in the index.
const MISSING_SLOT: u32 = u32::MAX - 1;
/// Bloom bits for the substring negative filter.
#[cfg(not(test))]
const TRIGRAM_BITS: usize = 1 << 27;
#[cfg(test)]
const TRIGRAM_BITS: usize = 1 << 16;

/// Bloom bits for the four-byte negative filter.
#[cfg(not(test))]
const QUADGRAM_BITS: usize = 1 << 26;
#[cfg(test)]
const QUADGRAM_BITS: usize = 1 << 16;

fn trigram_bit(window: &[u8]) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in window {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    (hash as usize) & (TRIGRAM_BITS - 1)
}

fn quadgram_hash(window: &[u8]) -> u64 {
    let mut hash = 0x243f_6a88_85a3_08d3u64;
    for byte in window {
        hash ^= u64::from(*byte);
        hash = hash.rotate_left(5).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    hash
}

fn quadgram_bit(window: &[u8]) -> usize {
    (quadgram_hash(window) as usize) & (QUADGRAM_BITS - 1)
}

fn quadgram_signature_bytes(bytes: &[u8]) -> u64 {
    let mut signature = 0u64;
    for window in bytes.windows(4) {
        signature |= 1u64 << (quadgram_hash(window) & 63);
    }
    signature
}

fn trigram_signature(name_lower: &str) -> u64 {
    trigram_signature_bytes(name_lower.as_bytes())
}

fn trigram_signature_bytes(bytes: &[u8]) -> u64 {
    let mut signature = 0u64;
    for window in bytes.windows(3) {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in window {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        signature |= 1u64 << (hash & 63);
    }
    signature
}

/// One ranked search hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// Wire-ready file item.
    pub item: FileItem,
    /// Explainable score.
    pub score: i32,
}

/// Search result plus whether the bounded scan stopped early.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    /// Ranked hits.
    pub hits: Vec<SearchHit>,
    /// True when a candidate or scan budget was reached.
    pub truncated: bool,
}

/// Mutable in-memory file index for one volume.
#[derive(Debug)]
pub struct MemoryIndex {
    volume: VolumeId,
    root: String,
    root_frn: Frn,
    max_depth: usize,
    slots: Vec<Option<FileRecord>>,
    free_slots: Vec<usize>,
    by_frn: HashMap<u64, usize>,
    by_name: BTreeMap<String, Vec<usize>>,
    parent_slots: Vec<u32>,
    slot_present: Vec<bool>,
    slot_name_valid: Vec<bool>,
    children: HashMap<u64, Vec<usize>>,
    trigram_bits: Vec<u64>,
    quadgram_bits: Vec<u64>,
    slot_signatures: Vec<u64>,
    slot_quad_signatures: Vec<u64>,
}

impl MemoryIndex {
    /// Create an empty index for one volume.
    pub fn new(
        volume: VolumeId,
        root: impl Into<String>,
        root_frn: Frn,
        max_depth: usize,
    ) -> Result<Self, IndexError> {
        let root = root.into();
        if root.is_empty() || root.contains('\0') {
            return Err(IndexError::InvalidData(
                "volume root must be non-empty and contain no NUL".to_owned(),
            ));
        }
        if max_depth == 0 {
            return Err(IndexError::InvalidData(
                "max_depth must be non-zero".to_owned(),
            ));
        }
        Ok(Self {
            volume,
            root,
            root_frn,
            max_depth,
            slots: Vec::new(),
            free_slots: Vec::new(),
            by_frn: HashMap::new(),
            by_name: BTreeMap::new(),
            parent_slots: Vec::new(),
            slot_present: Vec::new(),
            slot_name_valid: Vec::new(),
            children: HashMap::new(),
            trigram_bits: vec![0; TRIGRAM_BITS / 64],
            quadgram_bits: vec![0; QUADGRAM_BITS / 64],
            slot_signatures: Vec::new(),
            slot_quad_signatures: Vec::new(),
        })
    }

    /// Volume identifier.
    #[must_use]
    pub fn volume(&self) -> &VolumeId {
        &self.volume
    }

    /// Root path, for example `C:\`.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Root FRN.
    #[must_use]
    pub const fn root_frn(&self) -> Frn {
        self.root_frn
    }

    /// Number of live records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_frn.len()
    }

    /// Whether no records are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_frn.is_empty()
    }

    /// Return a record by FRN.
    #[must_use]
    pub fn record(&self, frn: Frn) -> Option<&FileRecord> {
        self.by_frn
            .get(&frn.get())
            .and_then(|slot| self.slots.get(*slot))
            .and_then(Option::as_ref)
    }

    fn parent_slot(&self, parent_frn: u64) -> Option<u32> {
        if Frn::new(parent_frn).record_number() == self.root_frn.record_number() {
            Some(ROOT_SLOT)
        } else {
            self.by_frn.get(&parent_frn).map(|slot| *slot as u32)
        }
    }

    fn attach_child(&mut self, parent_frn: u64, slot: usize) {
        self.children.entry(parent_frn).or_default().push(slot);
    }

    fn detach_child(&mut self, parent_frn: u64, slot: usize) {
        let empty = if let Some(children) = self.children.get_mut(&parent_frn) {
            children.retain(|child| *child != slot);
            children.is_empty()
        } else {
            false
        };
        if empty {
            self.children.remove(&parent_frn);
        }
    }

    fn update_waiting_children(&mut self, parent_frn: u64, parent_slot: u32) {
        if let Some(children) = self.children.get(&parent_frn).cloned() {
            for child in children {
                if self.slots.get(child).and_then(Option::as_ref).is_some() {
                    self.parent_slots[child] = parent_slot;
                }
            }
        }
    }

    /// Insert or update a record.
    ///
    /// Stale events (older `last_usn`) and exact duplicates are ignored. Returns
    /// whether the index changed.
    pub fn upsert(&mut self, record: FileRecord) -> Result<bool, IndexError> {
        record.validate()?;
        if record.frn == 0 {
            return Err(IndexError::InvalidData("FRN 0 is not valid".to_owned()));
        }
        if record.name.contains('\\') || record.name.contains('/') {
            return Err(IndexError::InvalidData(
                "file name contains a path separator".to_owned(),
            ));
        }

        if let Some(slot) = self.by_frn.get(&record.frn).copied() {
            let Some(existing) = self.slots.get(slot).and_then(Option::as_ref) else {
                return Err(IndexError::InvalidData(
                    "index slot reference is dangling".to_owned(),
                ));
            };
            if existing.last_usn > record.last_usn {
                return Ok(false);
            }
            if existing == &record {
                return Ok(false);
            }
            let old_parent = existing.parent_frn;
            let old_name = existing.name.clone();
            if old_name != record.name {
                self.remove_from_name_index(&old_name, slot);
                self.insert_into_name_index(&record.name, slot);
            }
            if old_parent != record.parent_frn {
                self.detach_child(old_parent, slot);
                self.attach_child(record.parent_frn, slot);
            }
            self.parent_slots[slot] = self.parent_slot(record.parent_frn).unwrap_or(MISSING_SLOT);
            self.slot_present[slot] = true;
            self.slot_name_valid[slot] = crate::path::valid_name(&record.name);
            self.slot_signatures[slot] = trigram_signature(&record.name.to_lowercase());
            self.slot_quad_signatures[slot] =
                quadgram_signature_bytes(record.name.to_lowercase().as_bytes());
            self.slots[slot] = Some(record);
            return Ok(true);
        }

        let parent_frn = record.parent_frn;
        let frn = record.frn;
        let slot = if let Some(slot) = self.free_slots.pop() {
            self.slots[slot] = Some(record.clone());
            self.parent_slots[slot] = MISSING_SLOT;
            self.slot_present[slot] = true;
            self.slot_name_valid[slot] = crate::path::valid_name(&record.name);
            slot
        } else {
            let slot = self.slots.len();
            self.slots.push(Some(record.clone()));
            self.parent_slots.push(MISSING_SLOT);
            self.slot_present.push(true);
            self.slot_name_valid
                .push(crate::path::valid_name(&record.name));
            self.slot_signatures.push(0);
            self.slot_quad_signatures.push(0);
            slot
        };
        self.by_frn.insert(frn, slot);
        self.insert_into_name_index(&record.name, slot);
        self.slot_signatures[slot] = trigram_signature(&record.name.to_lowercase());
        self.slot_quad_signatures[slot] =
            quadgram_signature_bytes(record.name.to_lowercase().as_bytes());
        self.parent_slots[slot] = self.parent_slot(parent_frn).unwrap_or(MISSING_SLOT);
        self.attach_child(parent_frn, slot);
        self.update_waiting_children(frn, slot as u32);
        Ok(true)
    }

    /// Apply a delete event. Stale deletes are ignored.
    pub fn delete(&mut self, frn: Frn, usn: u64) -> Result<bool, IndexError> {
        if frn == self.root_frn {
            return Err(IndexError::InvalidData(
                "root record cannot be deleted from the index".to_owned(),
            ));
        }
        let Some(slot) = self.by_frn.get(&frn.get()).copied() else {
            return Ok(false);
        };
        let Some(record) = self.slots.get(slot).and_then(Option::as_ref) else {
            return Err(IndexError::InvalidData(
                "index slot reference is dangling".to_owned(),
            ));
        };
        if record.last_usn > usn {
            return Ok(false);
        }
        let name = record.name.clone();
        let parent_frn = record.parent_frn;
        self.remove_from_name_index(&name, slot);
        self.detach_child(parent_frn, slot);
        self.parent_slots[slot] = MISSING_SLOT;
        self.slot_present[slot] = false;
        self.slot_name_valid[slot] = false;
        if let Some(children) = self.children.get(&frn.get()).cloned() {
            for child in children {
                if self.slots.get(child).and_then(Option::as_ref).is_some() {
                    self.parent_slots[child] = MISSING_SLOT;
                }
            }
        }
        self.by_frn.remove(&frn.get());
        self.slots[slot] = None;
        if let Some(signature) = self.slot_signatures.get_mut(slot) {
            *signature = 0;
        }
        if let Some(signature) = self.slot_quad_signatures.get_mut(slot) {
            *signature = 0;
        }
        self.free_slots.push(slot);
        Ok(true)
    }

    /// Apply one journal event.
    pub fn apply(&mut self, event: IndexEvent) -> Result<bool, IndexError> {
        match event {
            IndexEvent::Upsert(record) => self.upsert(record),
            IndexEvent::Delete { frn, usn } => self.delete(Frn::new(frn), usn),
        }
    }

    /// Search with a bounded prefix scan and bounded substring fallback.
    pub fn search(&self, query: &str, limit: u32) -> SearchOutcome {
        let query = query.trim();
        let limit = limit.min(MAX_RESULTS) as usize;
        if query.is_empty() || limit == 0 {
            return SearchOutcome::default();
        }

        let query_lower = query.to_lowercase();
        let mut candidates: Vec<(i32, usize)> = Vec::new();
        let mut truncated = false;

        for (name, slots) in self.by_name.range(query_lower.clone()..) {
            if !name.starts_with(&query_lower) {
                break;
            }
            for slot in slots {
                if candidates.len() >= MAX_PREFIX_CANDIDATES {
                    truncated = true;
                    break;
                }
                if let Some(record) = self.slots.get(*slot).and_then(Option::as_ref) {
                    let base = if name == &query_lower {
                        SCORE_EXACT
                    } else {
                        SCORE_PREFIX
                    };
                    let score = apply_penalty(base, record.name.chars().count());
                    candidates.push((score, *slot));
                }
            }
            if truncated {
                break;
            }
        }

        if candidates.len() < limit && self.might_contain_substring(&query_lower) {
            let query_signature = trigram_signature(&query_lower);
            let query_quad_signature = quadgram_signature_bytes(query_lower.as_bytes());
            // Prefix buckets are disjoint, so only the fallback needs deduplication.
            let mut seen: Vec<usize> = candidates.iter().map(|(_, slot)| *slot).collect();
            seen.sort_unstable();
            let mut scanned = 0usize;
            for (slot, record) in self.slots.iter().enumerate() {
                let Some(record) = record.as_ref() else {
                    continue;
                };
                if seen.binary_search(&slot).is_ok() {
                    continue;
                }
                if scanned >= MAX_SUBSTRING_SCAN {
                    truncated = true;
                    break;
                }
                scanned += 1;
                if query_signature != 0
                    && self.slot_signatures.get(slot).copied().unwrap_or(0) & query_signature
                        != query_signature
                {
                    continue;
                }
                if query_quad_signature != 0
                    && self.slot_quad_signatures.get(slot).copied().unwrap_or(0)
                        & query_quad_signature
                        != query_quad_signature
                {
                    continue;
                }
                if !contains_lowercase_needle(&record.name, &query_lower) {
                    continue;
                }
                if let Some(score) = score_name(&record.name, query_lower.as_str()) {
                    candidates.push((score, slot));
                }
            }
        }

        let mut compare = |(left_score, left_slot): &(i32, usize),
                           (right_score, right_slot): &(i32, usize)| {
            right_score
                .cmp(left_score)
                .then_with(|| self.slot_name(*left_slot).cmp(self.slot_name(*right_slot)))
                .then_with(|| left_slot.cmp(right_slot))
        };
        let mut sorted_end = candidates.len();
        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, &mut compare);
            candidates[..limit].sort_by(&mut compare);
            sorted_end = limit;
        } else {
            candidates.sort_by(&mut compare);
        }

        let mut hits = Vec::with_capacity(limit.min(candidates.len()));
        let mut position = 0usize;
        while position < candidates.len() {
            if hits.len() >= limit {
                break;
            }
            if position == sorted_end && sorted_end < candidates.len() {
                candidates[sorted_end..].sort_by(&mut compare);
                sorted_end = candidates.len();
            }
            let (score, slot) = candidates[position];
            position += 1;
            let record = self
                .slots
                .get(slot)
                .and_then(Option::as_ref)
                .expect("search candidate slot is live");
            let path = match self.resolve_record_path(Frn::new(record.frn)) {
                Ok(path) => path,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            let depth = path.matches('\\').count() as i32;
            let directory = if record.is_dir {
                SCORE_DIRECTORY_PENALTY
            } else {
                0
            };
            hits.push(SearchHit {
                item: FileItem {
                    id: FileId::new(self.volume.clone(), Frn::new(record.frn)),
                    name: record.name.clone(),
                    path,
                    attributes: record.attributes,
                    is_dir: record.is_dir,
                },
                score: (score - depth.min(SCORE_DEPTH_PENALTY_MAX) - directory).max(1),
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.item.name.cmp(&right.item.name))
                .then_with(|| left.item.path.cmp(&right.item.path))
        });

        SearchOutcome { hits, truncated }
    }

    /// Resolve one record's display path through the monomorphic slot core.
    pub fn resolve_record_path(&self, frn: Frn) -> Result<String, PathError> {
        let Some(start) = self.by_frn.get(&frn.get()).copied() else {
            return Err(PathError::NotFound { frn });
        };
        let slots = match walk_named(
            &self.parent_slots,
            &self.slot_present,
            &self.slot_name_valid,
            ROOT_SLOT,
            start as u32,
            self.max_depth,
        ) {
            Ok(slots) => slots,
            Err(failure) => match failure.error {
                NamedWalkError::DepthExceeded => {
                    return Err(PathError::DepthExceeded {
                        max: self.max_depth,
                    });
                }
                NamedWalkError::Cycle { node } => {
                    return Err(PathError::Cycle {
                        frn: self.slot_frn_for_error(node, frn),
                    });
                }
                NamedWalkError::MissingParent { node } if node == MISSING_SLOT => {
                    return crate::path::resolve_path(
                        self,
                        &self.root,
                        self.root_frn,
                        frn,
                        self.max_depth,
                    );
                }
                NamedWalkError::MissingParent { node } => {
                    let child = failure
                        .visited
                        .last()
                        .and_then(|slot| self.slot_frn(*slot))
                        .unwrap_or(frn);
                    return Err(PathError::MissingParent {
                        child,
                        parent: self.slot_frn_for_error(node, frn),
                    });
                }
                NamedWalkError::InvalidName { node } => {
                    return Err(PathError::InvalidName {
                        frn: self.slot_frn(node).unwrap_or(frn),
                    });
                }
            },
        };

        if let Some(position) = slots.iter().position(|slot| {
            self.slots
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .is_none()
        }) {
            let missing = slots[position];
            let (child, parent) = if position == 0 {
                (frn, self.slot_frn(missing).unwrap_or(frn))
            } else {
                let child_record = self
                    .slots
                    .get(slots[position - 1] as usize)
                    .and_then(Option::as_ref)
                    .expect("path prefix is live");
                (
                    Frn::new(child_record.frn),
                    Frn::new(child_record.parent_frn),
                )
            };
            return Err(PathError::MissingParent { child, parent });
        }

        let normalized_root = self.root.trim_end_matches(['\\', '/']);
        let normalized_root = if normalized_root.is_empty() {
            "\\"
        } else {
            normalized_root
        };
        let mut path = String::from(normalized_root);
        for slot in slots.iter().rev() {
            let record = self
                .slots
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .ok_or(PathError::MissingParent {
                    child: frn,
                    parent: self.slot_frn(*slot).unwrap_or(frn),
                })?;
            if record.name.is_empty()
                || record.name.contains('\0')
                || record.name.contains('\\')
                || record.name.contains('/')
            {
                return Err(PathError::InvalidName {
                    frn: Frn::new(record.frn),
                });
            }
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path.push_str(&record.name);
        }
        let length = path.encode_utf16().count();
        if length > MAX_PATH_UTF16 {
            return Err(PathError::PathTooLong {
                actual: length,
                max: MAX_PATH_UTF16,
            });
        }
        Ok(path)
    }

    fn slot_frn(&self, slot: u32) -> Option<Frn> {
        self.slots
            .get(slot as usize)
            .and_then(Option::as_ref)
            .map(|record| Frn::new(record.frn))
    }

    fn slot_frn_for_error(&self, slot: u32, fallback: Frn) -> Frn {
        if let Some(frn) = self.slot_frn(slot) {
            return frn;
        }
        self.slots
            .iter()
            .enumerate()
            .find_map(|(index, record)| {
                let record = record.as_ref()?;
                (self.parent_slots.get(index).copied() == Some(slot))
                    .then_some(Frn::new(record.parent_frn))
            })
            .unwrap_or(fallback)
    }

    fn slot_name(&self, slot: usize) -> &str {
        self.slots
            .get(slot)
            .and_then(Option::as_ref)
            .map_or("", |record| record.name.as_str())
    }

    fn insert_into_name_index(&mut self, name: &str, slot: usize) {
        let key = name.to_lowercase();
        self.mark_ngrams(&key);
        self.by_name.entry(key).or_default().push(slot);
    }

    fn mark_ngrams(&mut self, name_lower: &str) {
        let bytes = name_lower.as_bytes();
        for window in bytes.windows(3) {
            let bit = trigram_bit(window);
            self.trigram_bits[bit / 64] |= 1u64 << (bit % 64);
        }
        for window in bytes.windows(4) {
            let bit = quadgram_bit(window);
            self.quadgram_bits[bit / 64] |= 1u64 << (bit % 64);
        }
    }

    fn might_contain_substring(&self, query_lower: &str) -> bool {
        let bytes = query_lower.as_bytes();
        if bytes.len() < 3 {
            return true;
        }
        if !bytes.windows(3).all(|window| {
            let bit = trigram_bit(window);
            self.trigram_bits[bit / 64] & (1u64 << (bit % 64)) != 0
        }) {
            return false;
        }
        if bytes.len() < 4 {
            return true;
        }
        bytes.windows(4).all(|window| {
            let bit = quadgram_bit(window);
            self.quadgram_bits[bit / 64] & (1u64 << (bit % 64)) != 0
        })
    }

    fn remove_from_name_index(&mut self, name: &str, slot: usize) {
        let key = name.to_lowercase();
        if let Some(slots) = self.by_name.get_mut(&key) {
            slots.retain(|candidate| *candidate != slot);
            if slots.is_empty() {
                self.by_name.remove(&key);
            }
        }
    }

    #[cfg(any(test, feature = "invariants"))]
    fn consistency_error(&self) -> Option<&'static str> {
        if self.slot_signatures.len() != self.slots.len() {
            return Some("slot signature count mismatch");
        }
        if self.slot_quad_signatures.len() != self.slots.len() {
            return Some("slot quadgram signature count mismatch");
        }
        if self.slot_name_valid.len() != self.slots.len() {
            return Some("slot name validity count mismatch");
        }
        if self.slot_present.len() != self.slots.len() {
            return Some("slot presence count mismatch");
        }
        if self.by_frn.len() != self.by_name.values().map(Vec::len).sum::<usize>() {
            return Some("frn count does not match name buckets");
        }
        for (frn, slot) in &self.by_frn {
            let Some(record) = self.slots.get(*slot).and_then(Option::as_ref) else {
                return Some("frn points at a free slot");
            };
            if record.frn != *frn {
                return Some("slot frn mismatch");
            }
            let key = record.name.to_lowercase();
            let Some(bucket) = self.by_name.get(&key) else {
                return Some("missing name bucket");
            };
            if !bucket.contains(slot) {
                return Some("slot missing from name bucket");
            }
            if self.slot_signatures.get(*slot).copied() != Some(trigram_signature(&key)) {
                return Some("slot signature mismatch");
            }
            if self.slot_present.get(*slot).copied() != Some(true) {
                return Some("slot presence mismatch");
            }
            if self.slot_name_valid.get(*slot).copied()
                != Some(crate::path::valid_name(&record.name))
            {
                return Some("slot name validity mismatch");
            }
            if self.slot_quad_signatures.get(*slot).copied()
                != Some(quadgram_signature_bytes(key.as_bytes()))
            {
                return Some("slot quadgram signature mismatch");
            }
        }
        None
    }

    /// Check internal slot/name-bucket consistency.
    #[cfg(feature = "invariants")]
    pub fn check_consistency(&self) -> Result<(), &'static str> {
        match self.consistency_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn assert_consistent(&self) {
        assert_eq!(self.consistency_error(), None);
    }
}

impl RecordLookup for MemoryIndex {
    fn record(&self, frn: Frn) -> Option<&FileRecord> {
        self.record(frn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pstart_core::text::contains_case_insensitive;

    fn record(frn: u64, parent: u64, name: &str, usn: u64) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.to_owned(),
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
        let slot = *index.by_frn.get(&12).unwrap();
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
        let slot = *index.by_frn.get(&12).unwrap();
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
        let slot = *index.by_frn.get(&12).unwrap();
        index.by_name.remove("report.docx");
        index
            .by_name
            .get_mut("annual-report.txt")
            .unwrap()
            .push(slot);
        assert_eq!(index.consistency_error(), Some("missing name bucket"));
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
        let slot = *index.by_frn.get(&12).unwrap();
        index.slots[slot] = None;
        assert_eq!(index.consistency_error(), Some("frn points at a free slot"));
    }

    #[test]
    fn consistency_error_detects_slot_frn_mismatch() {
        let mut index = populated();
        let slot = *index.by_frn.get(&12).unwrap();
        index.slots[slot].as_mut().unwrap().frn = 99;
        assert_eq!(index.consistency_error(), Some("slot frn mismatch"));
    }

    #[test]
    fn consistency_error_detects_bucket_without_slot() {
        let mut index = populated();
        let slot = *index.by_frn.get(&12).unwrap();
        let other = *index.by_frn.get(&13).unwrap();
        let bucket = index.by_name.get_mut("report.docx").unwrap();
        bucket.retain(|entry| *entry != slot);
        bucket.push(other);
        assert_eq!(
            index.consistency_error(),
            Some("slot missing from name bucket")
        );
    }

    #[test]
    fn missing_start_slot_reports_start_as_parent() {
        let mut index = index();
        assert!(index.upsert(record(12, 5, "a", 1)).unwrap());
        let slot = *index.by_frn.get(&12).unwrap();
        index.slots[slot] = None;
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
        let slot = *index.by_frn.get(&10).unwrap();
        index.slots[slot] = None;
        assert!(index.search("stale", 10).hits.is_empty());
    }

    #[test]
    fn root_sequence_variant_is_treated_as_root() {
        let mut index =
            MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 64).unwrap();
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
        let slot = *index.by_frn.get(&10).unwrap();
        index.parent_slots[slot] = MISSING_SLOT;
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
        let slot = *index.by_frn.get(&20).unwrap();
        index.slots[slot] = None;
        // Slot graph still detects the cycle before slot validation.
        assert_eq!(
            index.resolve_record_path(Frn::new(12)),
            Err(PathError::Cycle { frn: Frn::new(20) })
        );
    }

    #[test]
    fn invalid_name_before_cycle_matches_generic_path() {
        let mut index = index();
        assert!(index.upsert(record(10, 11, "a", 1)).unwrap());
        assert!(index.upsert(record(11, 10, "b", 2)).unwrap());
        assert!(index.upsert(record(12, 11, "c", 3)).unwrap());
        let slot = *index.by_frn.get(&10).unwrap();
        index.slots[slot].as_mut().unwrap().name = "bad/name".to_owned();
        index.slot_name_valid[slot] = false;
        let indexed = index.resolve_record_path(Frn::new(12));
        let generic =
            crate::path::resolve_path(&index, "C:\\", Frn::ROOT, Frn::new(12), index.max_depth);
        assert_eq!(indexed, Err(PathError::InvalidName { frn: Frn::new(10) }));
        assert_eq!(indexed, generic);
    }

    #[test]
    fn consistency_error_detects_name_validity_count_mismatch() {
        let mut index = populated();
        index.slot_name_valid.pop();
        assert_eq!(
            index.consistency_error(),
            Some("slot name validity count mismatch")
        );
    }

    #[test]
    fn consistency_error_detects_presence_count_mismatch() {
        let mut index = populated();
        index.slot_present.pop();
        assert_eq!(
            index.consistency_error(),
            Some("slot presence count mismatch")
        );
    }

    #[test]
    fn consistency_error_detects_presence_and_name_validity_mismatch() {
        let mut index = populated();
        let slot = *index.by_frn.get(&12).unwrap();
        index.slot_present[slot] = false;
        assert_eq!(index.consistency_error(), Some("slot presence mismatch"));
        index.slot_present[slot] = true;
        index.slot_name_valid[slot] = false;
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
        let slot = *index.by_frn.get(&11).unwrap();
        index.slot_present[slot] = false;
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
        let slot = *index.by_frn.get(&11).unwrap();
        index.slots[slot] = None;
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
        assert!(matches!(
            index.resolve_record_path(Frn::new(11)),
            Err(PathError::MissingParent { .. })
        ));
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

        let mut depth =
            MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 2).unwrap();
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
        let mut index =
            MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 200).unwrap();
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
            let slot = *index.by_frn.get(&12).unwrap();
            index.slots[slot].as_mut().unwrap().name = name.clone();
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
        let slot = *index.by_frn.get(&10).unwrap();
        index.slots[slot].as_mut().unwrap().name = "bad/name".to_owned();
        assert!(matches!(
            index.resolve_record_path(Frn::new(10)),
            Err(PathError::InvalidName { .. })
        ));
        index.slots[slot].as_mut().unwrap().name = "a".to_owned();
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
        assert_eq!(index.record(Frn::new(12)).unwrap().name, "Report.docx");
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
        let mut index =
            MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 2).unwrap();
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
        index
            .by_name
            .entry("ghost".to_owned())
            .or_default()
            .push(12_345);
        let outcome = index.search("ghost", 1);
        assert!(outcome.hits.is_empty());
    }

    #[test]
    fn removing_an_absent_name_entry_is_a_noop() {
        let mut index = index();
        index.remove_from_name_index("absent.txt", 0);
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
                    name: format!("{name}{position}"),
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
}
