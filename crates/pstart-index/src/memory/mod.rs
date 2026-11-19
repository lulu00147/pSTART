//! In-memory index with bounded search and FRN path reconstruction.

use smallvec::SmallVec;
use std::cmp::Ordering;
use std::collections::HashMap;

mod frn_map;
mod name_index;

use frn_map::FrnMap;
use name_index::NameIndex;

use pstart_core::limits::{MAX_PATH_UTF16, MAX_RESULTS};
use pstart_core::walker::{SLOT_NAME_VALID, SLOT_PRESENT};
use pstart_core::{FileId, FileItem, Frn, VolumeId};
use pstart_score::apply_penalty;

use crate::error::IndexError;
use crate::model::{FileRecord, IndexEvent};
use crate::path::{PathError, RecordLookup};
use crate::score::{SCORE_DEPTH_PENALTY_MAX, SCORE_DIRECTORY_PENALTY, SCORE_EXACT, SCORE_PREFIX};
use pstart_core::text::contains_lowercase_needle;

mod path;
mod search;
#[cfg(test)]
mod tests;
mod trigram;

pub use search::{SearchHit, SearchOutcome};

use pstart_core::walker::NamedWalkError;
use trigram::*;

/// Maximum number of prefix candidates inspected for one query.
/// Bounds worst-case latency; searches report `truncated` when exceeded.
#[cfg(not(test))]
pub const MAX_PREFIX_CANDIDATES: usize = 5_000;
/// Test-sized prefix candidate budget, so the cap path is cheap to exercise.
#[cfg(test)]
pub const MAX_PREFIX_CANDIDATES: usize = 10;

/// Maximum names inspected and records examined by the substring fallback.
/// Bounds worst-case latency; searches report `truncated` when exceeded.
#[cfg(not(test))]
pub const MAX_SUBSTRING_SCAN: usize = 50_000;
/// Test-sized substring scan budget, so the cap path is cheap to exercise.
#[cfg(test)]
pub const MAX_SUBSTRING_SCAN: usize = 100;

/// Sentinel for a record whose parent is the volume root.
const ROOT_SLOT: u32 = u32::MAX;
/// Sentinel for a parent FRN not currently present in the index.
const MISSING_SLOT: u32 = u32::MAX - 1;

/// Approximate heap usage of an index, for diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryReport {
    /// Live records.
    pub records: usize,
    /// Slot vector plus record names.
    pub slots: usize,
    /// FRN lookup table.
    pub by_frn: usize,
    /// Lowercased name buckets.
    pub by_name: usize,
    /// Parent-child waiting lists.
    pub children: usize,
    /// Parent slot and validity vectors.
    pub metadata: usize,
    /// Trigram/quadgram bitmaps.
    pub bloom: usize,
    /// Per-slot signatures.
    pub signatures: usize,
    /// Free slot list.
    pub free_slots: usize,
}

impl MemoryReport {
    /// Total estimated heap bytes.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.slots
            + self.by_frn
            + self.by_name
            + self.children
            + self.metadata
            + self.bloom
            + self.signatures
            + self.free_slots
    }
}

/// Mutable in-memory file index for one volume.
#[derive(Debug)]
pub struct MemoryIndex {
    volume: VolumeId,
    root: String,
    root_frn: Frn,
    max_depth: usize,
    slots: Vec<Option<FileRecord>>,
    free_slots: Vec<u32>,
    by_frn: FrnMap,
    names: NameIndex,
    slot_generations: Vec<u32>,
    parent_slots: Vec<u32>,
    slot_flags: Vec<u8>,
    children: HashMap<u64, SmallVec<[u32; 2]>>,
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
        if root.is_empty() {
            return Err(IndexError::InvalidData(
                "volume root must be non-empty and contain no NUL".to_owned(),
            ));
        }
        if root.contains('\0') {
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
            by_frn: FrnMap::new(),
            names: NameIndex::default(),
            slot_generations: Vec::new(),
            parent_slots: Vec::new(),
            slot_flags: Vec::new(),
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

    /// Estimated heap usage by component.
    #[must_use]
    pub fn memory_report(&self) -> MemoryReport {
        let mut report = MemoryReport {
            records: self.by_frn.len(),
            slots: self.slots.capacity() * std::mem::size_of::<Option<FileRecord>>(),
            by_frn: self.by_frn.capacity()
                * (std::mem::size_of::<u64>() + std::mem::size_of::<u32>() + 1),
            metadata: self.parent_slots.capacity() * std::mem::size_of::<u32>()
                + self.slot_flags.capacity(),
            bloom: (self.trigram_bits.capacity() + self.quadgram_bits.capacity())
                * std::mem::size_of::<u64>(),
            signatures: (self.slot_signatures.capacity() + self.slot_quad_signatures.capacity())
                * std::mem::size_of::<u64>(),
            free_slots: self.free_slots.capacity() * std::mem::size_of::<u32>(),
            ..MemoryReport::default()
        };
        for record in self.slots.iter().flatten() {
            report.slots += record.name.len();
        }
        report.by_name =
            self.names.bytes() + self.slot_generations.capacity() * std::mem::size_of::<u32>();
        for children in self.children.values() {
            report.children += children.capacity() * std::mem::size_of::<u32>()
                + std::mem::size_of::<(u64, SmallVec<[u32; 2]>)>()
                + 16;
        }
        report
    }

    fn slot_flags(name_valid: bool) -> u8 {
        SLOT_PRESENT | if name_valid { SLOT_NAME_VALID } else { 0 }
    }

    fn lower_cmp(left: &str, right: &str) -> Ordering {
        if left.is_ascii() && right.is_ascii() {
            left.bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
        } else {
            left.to_lowercase().cmp(&right.to_lowercase())
        }
    }

    fn lower_starts_with(name: &str, prefix_lower: &str) -> bool {
        if name.is_ascii() && prefix_lower.is_ascii() {
            name.len() >= prefix_lower.len()
                && name
                    .bytes()
                    .zip(prefix_lower.bytes())
                    .all(|(left, right)| left.to_ascii_lowercase() == right)
        } else {
            name.to_lowercase().starts_with(prefix_lower)
        }
    }

    fn ref_name(&self, reference: u64) -> Option<&str> {
        let (slot, generation) = name_index::unpack(reference);
        let slot = slot as usize;
        if self.slot_generations.get(slot).copied() != Some(generation) {
            return None;
        }
        self.slots
            .get(slot)
            .and_then(Option::as_ref)
            .map(|record| record.name.as_ref())
    }

    fn compare_refs(&self, left: u64, right: u64) -> Ordering {
        match (self.ref_name(left), self.ref_name(right)) {
            (Some(left), Some(right)) => Self::lower_cmp(left, right),
            _ => Ordering::Equal,
        }
    }

    fn merge_dirty_names(&mut self) {
        if self.names.dirty.is_empty() {
            return;
        }
        let mut dirty = std::mem::take(&mut self.names.dirty);
        dirty.sort_unstable_by(|left, right| self.compare_refs(*left, *right));
        let sorted = std::mem::take(&mut self.names.sorted);
        let mut merged = Vec::with_capacity(sorted.len() + dirty.len());
        let mut left = sorted.into_iter().peekable();
        let mut right = dirty.into_iter().peekable();
        loop {
            match (left.peek(), right.peek()) {
                (Some(left_ref), Some(right_ref)) => {
                    if self.compare_refs(*left_ref, *right_ref) == Ordering::Greater {
                        if let Some(value) = right.next() {
                            merged.push(value);
                        }
                    } else if let Some(value) = left.next() {
                        merged.push(value);
                    }
                }
                (Some(_), None) => {
                    if let Some(value) = left.next() {
                        merged.push(value);
                    }
                }
                (None, Some(_)) => {
                    if let Some(value) = right.next() {
                        merged.push(value);
                    }
                }
                (None, None) => break,
            }
        }
        self.names.sorted = merged;
    }

    fn rebuild_names(&mut self) {
        let mut references = Vec::with_capacity(self.names.live);
        for slot in 0..self.slots.len() {
            if self.slot_flags.get(slot).copied().unwrap_or(0) & SLOT_PRESENT != 0 {
                if self.slots[slot].is_some() {
                    references.push(name_index::pack(slot as u32, self.slot_generations[slot]));
                }
            } else {
                self.slots[slot] = None;
            }
        }
        references.sort_unstable_by(|left, right| self.compare_refs(*left, *right));
        self.names.sorted = references;
        self.names.dirty.clear();
        self.names.needs_rebuild = false;
        self.names.pending_dead = 0;
    }

    /// Merge pending name changes into the sorted index.
    pub fn compact_names(&mut self) {
        if self.names.needs_rebuild || self.names.pending_dead > 4096 {
            self.rebuild_names();
        } else if !self.names.dirty.is_empty() {
            self.merge_dirty_names();
        }
        self.names.sorted.shrink_to_fit();
        self.names.dirty.shrink_to_fit();
        self.slots.shrink_to_fit();
        self.slot_generations.shrink_to_fit();
        self.parent_slots.shrink_to_fit();
        self.slot_flags.shrink_to_fit();
        self.slot_signatures.shrink_to_fit();
        self.slot_quad_signatures.shrink_to_fit();
        self.free_slots.shrink_to_fit();
    }

    /// Return a record by FRN.
    #[must_use]
    pub fn record(&self, frn: Frn) -> Option<&FileRecord> {
        self.by_frn
            .get(frn.get())
            .and_then(|slot| self.slots.get(slot as usize))
            .and_then(Option::as_ref)
    }

    fn parent_slot(&self, parent_frn: u64) -> Option<u32> {
        if Frn::new(parent_frn).record_number() == self.root_frn.record_number() {
            Some(ROOT_SLOT)
        } else {
            self.by_frn.get(parent_frn)
        }
    }

    fn attach_child(&mut self, parent_frn: u64, slot: usize) {
        self.children
            .entry(parent_frn)
            .or_default()
            .push(slot as u32);
    }

    fn detach_child(&mut self, parent_frn: u64, slot: usize) {
        let empty = if let Some(children) = self.children.get_mut(&parent_frn) {
            children.retain(|child| *child as usize != slot);
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
                let child = child as usize;
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
        if record.name.contains('\\') {
            return Err(IndexError::InvalidData(
                "file name contains a path separator".to_owned(),
            ));
        }
        if record.name.contains('/') {
            return Err(IndexError::InvalidData(
                "file name contains a path separator".to_owned(),
            ));
        }

        if let Some(slot) = self.by_frn.get(record.frn) {
            let slot = slot as usize;
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
            let key = record.name.to_lowercase();
            let name_changed = old_name != record.name;
            if name_changed {
                let generation = self.slot_generations[slot].wrapping_add(1);
                self.slot_generations[slot] = generation;
                self.names.rename(slot as u32, generation);
            }
            if old_parent != record.parent_frn {
                self.detach_child(old_parent, slot);
                self.attach_child(record.parent_frn, slot);
            }
            self.parent_slots[slot] = self.parent_slot(record.parent_frn).unwrap_or(MISSING_SLOT);
            self.slot_flags[slot] = Self::slot_flags(crate::path::valid_name(&record.name));
            self.mark_ngrams(&key);
            self.slot_signatures[slot] = trigram_signature(&key);
            self.slot_quad_signatures[slot] = quadgram_signature_bytes(key.as_bytes());
            self.slots[slot] = Some(record);
            if name_changed {
                self.rebuild_names();
            }
            return Ok(true);
        }

        let parent_frn = record.parent_frn;
        let frn = record.frn;
        let key = record.name.to_lowercase();
        let name_valid = crate::path::valid_name(&record.name);
        let reused = self.free_slots.last().copied();
        let slot = if let Some(slot) = self.free_slots.pop() {
            let slot = slot as usize;
            self.slots[slot] = Some(record);
            self.parent_slots[slot] = MISSING_SLOT;
            self.slot_flags[slot] = Self::slot_flags(name_valid);
            slot
        } else {
            let slot = self.slots.len();
            self.slots.push(Some(record));
            self.parent_slots.push(MISSING_SLOT);
            self.slot_flags.push(Self::slot_flags(name_valid));
            self.slot_signatures.push(0);
            self.slot_quad_signatures.push(0);
            slot
        };
        if reused.is_some() {
            let generation = self.slot_generations[slot].wrapping_add(1);
            self.slot_generations[slot] = generation;
            self.names.reuse(slot as u32, generation);
        } else {
            self.slot_generations.push(1);
            self.names.insert(slot as u32, 1);
        }
        self.by_frn.insert(frn, slot);
        self.mark_ngrams(&key);
        self.slot_signatures[slot] = trigram_signature(&key);
        self.slot_quad_signatures[slot] = quadgram_signature_bytes(key.as_bytes());
        self.parent_slots[slot] = self.parent_slot(parent_frn).unwrap_or(MISSING_SLOT);
        self.attach_child(parent_frn, slot);
        self.update_waiting_children(frn, slot as u32);
        if reused.is_some() {
            self.rebuild_names();
        } else if self.names.dirty.len() >= 65536 {
            self.merge_dirty_names();
        }
        Ok(true)
    }

    /// Apply a delete event. Stale deletes are ignored.
    pub fn delete(&mut self, frn: Frn, usn: u64) -> Result<bool, IndexError> {
        if frn == self.root_frn {
            return Err(IndexError::InvalidData(
                "root record cannot be deleted from the index".to_owned(),
            ));
        }
        let Some(slot) = self.by_frn.get(frn.get()) else {
            return Ok(false);
        };
        let slot = slot as usize;
        let Some(record) = self.slots.get(slot).and_then(Option::as_ref) else {
            return Err(IndexError::InvalidData(
                "index slot reference is dangling".to_owned(),
            ));
        };
        if record.last_usn > usn {
            return Ok(false);
        }
        let parent_frn = record.parent_frn;
        self.names.remove(slot as u32);
        self.detach_child(parent_frn, slot);
        self.parent_slots[slot] = MISSING_SLOT;
        self.slot_flags[slot] = 0;
        if let Some(children) = self.children.get(&frn.get()).cloned() {
            for child in children {
                let child = child as usize;
                if self.slots.get(child).and_then(Option::as_ref).is_some() {
                    self.parent_slots[child] = MISSING_SLOT;
                }
            }
        }
        self.by_frn.remove(frn.get());
        if let Some(signature) = self.slot_signatures.get_mut(slot) {
            *signature = 0;
        }
        if let Some(signature) = self.slot_quad_signatures.get_mut(slot) {
            *signature = 0;
        }
        self.free_slots.push(slot as u32);
        if self.names.pending_dead > 4096 {
            self.rebuild_names();
        }
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

    #[cfg(any(test, feature = "invariants"))]
    fn consistency_error(&self) -> Option<&'static str> {
        if self.slot_signatures.len() != self.slots.len() {
            return Some("slot signature count mismatch");
        }
        if self.slot_quad_signatures.len() != self.slots.len() {
            return Some("slot quadgram signature count mismatch");
        }
        if self.slot_flags.len() != self.slots.len() {
            return Some("slot state count mismatch");
        }
        if self.by_frn.len() != self.names.len() {
            return Some("frn count does not match name buckets");
        }
        for (frn, slot) in self.by_frn.iter() {
            let slot = slot as usize;
            let Some(record) = self.slots.get(slot).and_then(Option::as_ref) else {
                return Some("frn points at a free slot");
            };
            if record.frn != frn {
                return Some("slot frn mismatch");
            }
            let key = record.name.to_lowercase();
            let reference = name_index::pack(slot as u32, self.slot_generations[slot]);
            if !self.names.contains(reference) {
                return Some("slot missing from name bucket");
            }
            if self.slot_signatures.get(slot).copied() != Some(trigram_signature(&key)) {
                return Some("slot signature mismatch");
            }
            if self.slot_quad_signatures.get(slot).copied()
                != Some(quadgram_signature_bytes(key.as_bytes()))
            {
                return Some("slot quadgram signature mismatch");
            }
            let flags = self.slot_flags.get(slot).copied().unwrap_or(0);
            if flags & SLOT_PRESENT == 0 {
                return Some("slot presence mismatch");
            }
            let valid = crate::path::valid_name(&record.name);
            if (flags & SLOT_NAME_VALID != 0) != valid {
                return Some("slot name validity mismatch");
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
