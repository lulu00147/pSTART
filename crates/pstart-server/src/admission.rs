//! Shared atomic admission primitives.
//!
//! These functions are used by the runtime and by Loom tests, so the tested
//! algorithm is the actual admission algorithm rather than a copy.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimal atomic counter operations needed by admission.
pub trait Counter {
    /// Load the current value with acquire ordering.
    fn load_acquire(&self) -> usize;
    /// Weak compare-and-exchange with AcqRel success / Acquire failure.
    fn compare_exchange_weak(&self, current: usize, new: usize) -> Result<usize, usize>;
    /// Release one unit with AcqRel ordering.
    fn release_one(&self);
}

impl Counter for AtomicUsize {
    fn load_acquire(&self) -> usize {
        self.load(Ordering::Acquire)
    }

    fn compare_exchange_weak(&self, current: usize, new: usize) -> Result<usize, usize> {
        AtomicUsize::compare_exchange_weak(self, current, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn release_one(&self) {
        self.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Acquire one slot without exceeding `max_connections`.
pub fn try_acquire<C: Counter + ?Sized>(active: &C, max_connections: usize) -> bool {
    let mut current = active.load_acquire();
    loop {
        if current >= max_connections {
            return false;
        }
        match active.compare_exchange_weak(current, current + 1) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// Consume one unit without underflowing zero.
pub fn try_consume<C: Counter + ?Sized>(remaining: &C) -> bool {
    let mut current = remaining.load_acquire();
    loop {
        if current == 0 {
            return false;
        }
        match remaining.compare_exchange_weak(current, current - 1) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_paths_cover_limit_and_release() {
        let active = AtomicUsize::new(0);
        assert!(try_acquire(&active, 1));
        assert!(!try_acquire(&active, 1));
        assert_eq!(active.load_acquire(), 1);
        active.release_one();
        assert_eq!(active.load_acquire(), 0);
        assert!(!try_acquire(&active, 0));
    }

    #[test]
    fn compare_exchange_retry_paths_are_covered() {
        struct FlakyCounter<'a> {
            value: AtomicUsize,
            attempts: &'a AtomicUsize,
        }

        impl Counter for FlakyCounter<'_> {
            fn load_acquire(&self) -> usize {
                self.value.load(Ordering::Acquire)
            }

            fn compare_exchange_weak(&self, current: usize, new: usize) -> Result<usize, usize> {
                if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                    return Err(current);
                }
                self.value
                    .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire)
            }

            fn release_one(&self) {
                self.value.fetch_sub(1, Ordering::AcqRel);
            }
        }

        let attempts = AtomicUsize::new(0);
        let counter = FlakyCounter {
            value: AtomicUsize::new(0),
            attempts: &attempts,
        };
        assert!(try_acquire(&counter, 2));
        let attempts = AtomicUsize::new(0);
        let counter = FlakyCounter {
            value: AtomicUsize::new(2),
            attempts: &attempts,
        };
        assert!(try_consume(&counter));
        counter.release_one();
        assert_eq!(counter.load_acquire(), 0);
    }

    #[test]
    fn budget_paths_cover_zero_and_success() {
        let remaining = AtomicUsize::new(0);
        assert!(!try_consume(&remaining));
        let remaining = AtomicUsize::new(1);
        assert!(try_consume(&remaining));
        assert!(!try_consume(&remaining));
        assert_eq!(remaining.load_acquire(), 0);
    }
}
