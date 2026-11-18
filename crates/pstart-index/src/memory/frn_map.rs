//! Compact open-addressed map from FRN to slot index.

const EMPTY: u8 = 0;
const OCCUPIED: u8 = 1;
const DELETED: u8 = 2;
const INITIAL_CAPACITY: usize = 16;

/// Maps 64-bit FRNs to 32-bit slot indices with linear probing.
#[derive(Clone, Debug)]
pub(crate) struct FrnMap {
    keys: Vec<u64>,
    values: Vec<u32>,
    states: Vec<u8>,
    len: usize,
    used: usize,
}

impl Default for FrnMap {
    fn default() -> Self {
        Self::new()
    }
}

impl FrnMap {
    pub(crate) fn new() -> Self {
        Self {
            keys: Vec::new(),
            values: Vec::new(),
            states: Vec::new(),
            len: 0,
            used: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn capacity(&self) -> usize {
        self.keys.len()
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn clear(&mut self) {
        self.keys.clear();
        self.values.clear();
        self.states.clear();
        self.len = 0;
        self.used = 0;
    }

    fn hash(key: u64) -> usize {
        let mut value = key;
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value as usize
    }

    pub(crate) fn get(&self, key: u64) -> Option<u32> {
        if self.keys.is_empty() {
            return None;
        }
        let mask = self.keys.len() - 1;
        let mut index = Self::hash(key) & mask;
        loop {
            match self.states[index] {
                EMPTY => return None,
                OCCUPIED if self.keys[index] == key => return Some(self.values[index]),
                _ => index = (index + 1) & mask,
            }
        }
    }

    pub(crate) fn insert(&mut self, key: u64, value: usize) {
        if self.keys.is_empty() {
            self.resize(INITIAL_CAPACITY);
        } else if (self.used + 1) * 10 > self.keys.len() * 7 {
            self.resize(self.keys.len() * 2);
        }
        self.insert_no_resize(key, value as u32);
    }

    fn insert_no_resize(&mut self, key: u64, value: u32) {
        let mask = self.keys.len() - 1;
        let mut index = Self::hash(key) & mask;
        let mut first_deleted = None;
        loop {
            match self.states[index] {
                EMPTY => {
                    let target = first_deleted.unwrap_or(index);
                    let fresh = first_deleted.is_none();
                    self.keys[target] = key;
                    self.values[target] = value;
                    self.states[target] = OCCUPIED;
                    self.len += 1;
                    if fresh {
                        self.used += 1;
                    }
                    return;
                }
                OCCUPIED if self.keys[index] == key => {
                    self.values[index] = value;
                    return;
                }
                DELETED => {
                    first_deleted.get_or_insert(index);
                }
                _ => {}
            }
            index = (index + 1) & mask;
        }
    }

    pub(crate) fn remove(&mut self, key: u64) {
        if self.keys.is_empty() {
            return;
        }
        let mask = self.keys.len() - 1;
        let mut index = Self::hash(key) & mask;
        loop {
            match self.states[index] {
                EMPTY => return,
                OCCUPIED if self.keys[index] == key => {
                    self.states[index] = DELETED;
                    self.len -= 1;
                    return;
                }
                _ => index = (index + 1) & mask,
            }
        }
    }

    fn resize(&mut self, new_capacity: usize) {
        let old_keys = std::mem::take(&mut self.keys);
        let old_values = std::mem::take(&mut self.values);
        let old_states = std::mem::take(&mut self.states);
        self.keys = vec![0; new_capacity];
        self.values = vec![0; new_capacity];
        self.states = vec![EMPTY; new_capacity];
        self.len = 0;
        self.used = 0;
        for (index, state) in old_states.into_iter().enumerate() {
            if state == OCCUPIED {
                self.insert_no_resize(old_keys[index], old_values[index]);
            }
        }
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.states
            .iter()
            .enumerate()
            .filter(|(_, state)| **state == OCCUPIED)
            .map(|(index, _)| (self.keys[index], self.values[index]))
    }
}
