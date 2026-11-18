//! Capability-set policy for plugin Host APIs.

/// `host-record` capability bit.
pub const RECORD: u8 = 1 << 0;
/// `host-config` capability bit.
pub const CONFIG: u8 = 1 << 1;
/// `host-search` capability bit.
pub const SEARCH: u8 = 1 << 2;
/// `host-app-search` capability bit.
pub const APP_SEARCH: u8 = 1 << 3;
/// `host-open` capability bit.
pub const OPEN: u8 = 1 << 4;
/// `host-copy` capability bit.
pub const COPY: u8 = 1 << 5;
/// All known capability bits.
pub const ALL_BITS: u8 = RECORD | CONFIG | SEARCH | APP_SEARCH | OPEN | COPY;

/// A set of Host API capabilities.
pub struct CapabilitySet(pub u8);

impl CapabilitySet {
    /// No capabilities.
    pub const EMPTY: Self = Self(0);
    /// All known capabilities.
    pub const ALL: Self = Self(ALL_BITS);
    /// Search-only capability.
    pub const SEARCH_ONLY: Self = Self(SEARCH);

    /// Build a set, dropping bits outside the known domain.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & ALL_BITS)
    }

    /// Raw bits.
    pub const fn bits(&self) -> u8 {
        self.0
    }

    /// Whether this set grants `capability`.
    pub const fn allows(&self, capability: u8) -> bool {
        (self.0 & capability) == capability
    }

    /// Union of two sets.
    pub const fn union(&self, other: &Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_bits_are_truncated() {
        assert_eq!(CapabilitySet::from_bits(0xFF).bits(), ALL_BITS);
        assert_eq!(
            CapabilitySet::from_bits(SEARCH | OPEN).bits(),
            SEARCH | OPEN
        );
    }

    #[test]
    fn allow_and_union() {
        assert!(CapabilitySet::ALL.allows(OPEN));
        assert!(!CapabilitySet::SEARCH_ONLY.allows(OPEN));
        assert!(CapabilitySet::SEARCH_ONLY
            .union(&CapabilitySet::from_bits(OPEN))
            .allows(OPEN));
    }
}
