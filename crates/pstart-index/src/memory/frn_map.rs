//! Compact open-addressed map from FRN to slot index.

const EMPTY: u8 = 0;
const OCCUPIED: u8 = 1;
const INITIAL_CAPACITY: usize = 16;

/// Maps 64-bit FRNs to 32-bit slot indices with linear probing.
#[derive(Clone, Debug)]
pub(crate) struct FrnMap {
    keys: Vec<u64>,
    values: Vec<u32>,
    states: Vec<u8>,
    len: usize,
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
    }

    fn hash(key: u64) -> u64 {
        let mut value = key;
        value ^= value >> 33;
        value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
        value ^= value >> 33;
        value
    }

    fn start(hash: u64, capacity: usize) -> usize {
        ((u128::from(hash) * capacity as u128) >> 64) as usize
    }

    pub(crate) fn get(&self, key: u64) -> Option<u32> {
        if self.keys.is_empty() {
            return None;
        }
        let capacity = self.keys.len();
        let mut index = Self::start(Self::hash(key), capacity);
        loop {
            match self.states[index] {
                EMPTY => return None,
                OCCUPIED if self.keys[index] == key => return Some(self.values[index]),
                _ => {
                    index += 1;
                    if index == capacity {
                        index = 0;
                    }
                }
            }
        }
    }

    pub(crate) fn insert(&mut self, key: u64, value: usize) {
        if self.keys.is_empty() {
            self.resize(INITIAL_CAPACITY);
        } else if (self.len + 1) * 5 > self.keys.len() * 4 {
            let grown = self.keys.len() + self.keys.len() / 2 + 1;
            self.resize(grown.max(INITIAL_CAPACITY));
        }
        self.insert_no_resize(key, value as u32);
    }

    fn insert_no_resize(&mut self, key: u64, value: u32) {
        let capacity = self.keys.len();
        let mut index = Self::start(Self::hash(key), capacity);
        loop {
            match self.states[index] {
                EMPTY => {
                    self.keys[index] = key;
                    self.values[index] = value;
                    self.states[index] = OCCUPIED;
                    self.len += 1;
                    return;
                }
                OCCUPIED if self.keys[index] == key => {
                    self.values[index] = value;
                    return;
                }
                _ => {
                    index += 1;
                    if index == capacity {
                        index = 0;
                    }
                }
            }
        }
    }

    pub(crate) fn remove(&mut self, key: u64) {
        if self.keys.is_empty() {
            return;
        }
        let capacity = self.keys.len();
        let mut index = Self::start(Self::hash(key), capacity);
        loop {
            match self.states[index] {
                EMPTY => return,
                OCCUPIED if self.keys[index] == key => break,
                _ => {
                    index += 1;
                    if index == capacity {
                        index = 0;
                    }
                }
            }
        }

        self.len -= 1;
        let mut hole = index;
        self.states[hole] = EMPTY;
        let mut next = hole + 1;
        if next == capacity {
            next = 0;
        }
        while self.states[next] == OCCUPIED {
            let ideal = Self::start(Self::hash(self.keys[next]), capacity);
            let distance_from_ideal = (next + capacity - ideal) % capacity;
            let hole_from_ideal = (hole + capacity - ideal) % capacity;
            if hole_from_ideal <= distance_from_ideal {
                self.keys[hole] = self.keys[next];
                self.values[hole] = self.values[next];
                self.states[hole] = OCCUPIED;
                self.states[next] = EMPTY;
                hole = next;
            }
            next += 1;
            if next == capacity {
                next = 0;
            }
            if next == hole {
                break;
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
