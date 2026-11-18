//! Trigram and quadgram prefilter helpers.

/// Bloom bits for the substring negative filter.
#[cfg(not(test))]
pub(super) const TRIGRAM_BITS: usize = 1 << 24;
#[cfg(test)]
pub(super) const TRIGRAM_BITS: usize = 1 << 16;

/// Bloom bits for the four-byte negative filter.
#[cfg(not(test))]
pub(super) const QUADGRAM_BITS: usize = 1 << 23;
#[cfg(test)]
pub(super) const QUADGRAM_BITS: usize = 1 << 16;

pub(super) fn trigram_bit(window: &[u8]) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in window {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    (hash as usize) & (TRIGRAM_BITS - 1)
}

pub(super) fn quadgram_hash(window: &[u8]) -> u64 {
    let mut hash = 0x243f_6a88_85a3_08d3u64;
    for byte in window {
        hash ^= u64::from(*byte);
        hash = hash.rotate_left(5).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    hash
}

pub(super) fn quadgram_bit(window: &[u8]) -> usize {
    (quadgram_hash(window) as usize) & (QUADGRAM_BITS - 1)
}

pub(super) fn quadgram_signature_bytes(bytes: &[u8]) -> u64 {
    let mut signature = 0u64;
    for window in bytes.windows(4) {
        signature |= 1u64 << (quadgram_hash(window) & 63);
    }
    signature
}

pub(super) fn trigram_signature(name_lower: &str) -> u64 {
    trigram_signature_bytes(name_lower.as_bytes())
}

pub(super) fn trigram_signature_bytes(bytes: &[u8]) -> u64 {
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
