//! Slot-based name order: sorted slots plus a pending delta.

/// A slot plus the generation of its name. Reused slots get a new generation,
/// which invalidates older sorted references without a removal scan.
pub(crate) fn pack(slot: u32, generation: u32) -> u64 {
    u64::from(slot) | (u64::from(generation) << 32)
}

pub(crate) fn unpack(reference: u64) -> (u32, u32) {
    (reference as u32, (reference >> 32) as u32)
}

/// Holds slot references ordered by lowercase name.
#[derive(Debug, Default)]
pub(crate) struct NameIndex {
    pub(crate) sorted: Vec<u64>,
    pub(crate) dirty: Vec<u64>,
    pub(crate) live: usize,
    pub(crate) pending_dead: usize,
    pub(crate) needs_rebuild: bool,
}

impl NameIndex {
    pub(crate) fn insert(&mut self, slot: u32, generation: u32) {
        self.dirty.push(pack(slot, generation));
        self.live += 1;
    }

    pub(crate) fn rename(&mut self, slot: u32, generation: u32) {
        self.dirty.push(pack(slot, generation));
        self.needs_rebuild = true;
    }

    pub(crate) fn reuse(&mut self, slot: u32, generation: u32) {
        self.dirty.push(pack(slot, generation));
        self.live += 1;
        self.needs_rebuild = true;
    }

    pub(crate) fn remove(&mut self, _slot: u32) {
        self.live = self.live.saturating_sub(1);
        self.pending_dead += 1;
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn bytes(&self) -> usize {
        (self.sorted.capacity() + self.dirty.capacity()) * std::mem::size_of::<u64>()
    }

    #[allow(dead_code)] // exercised by invariant and unit tests
    pub(crate) fn contains(&self, reference: u64) -> bool {
        self.sorted.contains(&reference) || self.dirty.contains(&reference)
    }

    #[cfg(test)]
    pub(crate) fn forget(&mut self, slot: u32) {
        self.sorted.retain(|reference| unpack(*reference).0 != slot);
        self.dirty.retain(|reference| unpack(*reference).0 != slot);
        self.live = self.live.saturating_sub(1);
    }
}
