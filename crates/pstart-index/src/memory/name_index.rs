//! Compact lowercase-name index: one inline slot, overflow for duplicates.

use std::collections::{BTreeMap, HashMap};

use smallvec::SmallVec;

/// Maps lowercased names to slots, inlining the common single-slot case.
#[derive(Debug, Default)]
pub(crate) struct NameIndex {
    primary: BTreeMap<Box<str>, u32>,
    duplicates: HashMap<Box<str>, SmallVec<[u32; 2]>>,
}

impl NameIndex {
    pub(crate) fn insert(&mut self, key: Box<str>, slot: u32) {
        match self.primary.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(slot);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if *entry.get() != slot {
                    let duplicate_key = entry.key().clone();
                    self.duplicates.entry(duplicate_key).or_default().push(slot);
                }
                let _ = entry.get_mut();
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &str, slot: u32) {
        if let Some(duplicates) = self.duplicates.get_mut(key) {
            if let Some(position) = duplicates.iter().position(|value| *value == slot) {
                duplicates.swap_remove(position);
                if duplicates.is_empty() {
                    self.duplicates.remove(key);
                }
                return;
            }
            if self.primary.get(key) == Some(&slot) {
                let promoted = duplicates.swap_remove(0);
                if duplicates.is_empty() {
                    self.duplicates.remove(key);
                }
                self.primary.insert(key.into(), promoted);
                return;
            }
        }
        if self.primary.get(key) == Some(&slot) {
            self.primary.remove(key);
        }
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn first(&self, key: &str) -> Option<u32> {
        self.primary.get(key).copied()
    }

    pub(crate) fn duplicates(&self, key: &str) -> &[u32] {
        self.duplicates.get(key).map_or(&[], SmallVec::as_slice)
    }

    pub(crate) fn range(
        &self,
        prefix: &str,
    ) -> std::collections::btree_map::Range<'_, Box<str>, u32> {
        let start: Box<str> = prefix.into();
        self.primary.range(start..)
    }

    pub(crate) fn duplicate_bytes(&self) -> usize {
        self.duplicates
            .iter()
            .map(|(key, slots)| key.len() + slots.capacity() * std::mem::size_of::<u32>() + 16)
            .sum()
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn len(&self) -> usize {
        self.primary.len() + self.duplicates.values().map(SmallVec::len).sum::<usize>()
    }

    pub(crate) fn primary_entries(&self) -> impl Iterator<Item = (&str, u32)> + '_ {
        self.primary.iter().map(|(key, slot)| (key.as_ref(), *slot))
    }
}
