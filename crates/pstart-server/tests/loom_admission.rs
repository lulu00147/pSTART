#![cfg(feature = "loom")]

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;
use pstart_server::admission::{self, Counter};

struct LoomCounter(AtomicUsize);

impl Counter for LoomCounter {
    fn load_acquire(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    fn compare_exchange_weak(&self, current: usize, new: usize) -> Result<usize, usize> {
        self.0
            .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn release_one(&self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire(active: &LoomCounter, max_connections: usize) -> bool {
    admission::try_acquire(active, max_connections)
}

fn try_consume(remaining: &LoomCounter) -> bool {
    admission::try_consume(remaining)
}

#[test]
fn concurrent_admission_never_exceeds_limit() {
    loom::model(|| {
        let active = Arc::new(LoomCounter(AtomicUsize::new(0)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let active = Arc::clone(&active);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_acquire(&active, 1) {
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(successes.load(Ordering::Acquire) <= 1);
        assert!(active.load_acquire() <= 1);
    });
}

#[test]
fn concurrent_admission_limit_two_release() {
    loom::model(|| {
        let active = Arc::new(LoomCounter(AtomicUsize::new(0)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let active = Arc::clone(&active);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_acquire(&active, 2) {
                    let _slot = Slot(Arc::clone(&active));
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(successes.load(Ordering::Acquire), 2);
        assert_eq!(active.load_acquire(), 0);
    });
}

#[test]
fn concurrent_admission_limit_two_two_threads() {
    loom::model(|| {
        let active = Arc::new(LoomCounter(AtomicUsize::new(0)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let active = Arc::clone(&active);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_acquire(&active, 2) {
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(successes.load(Ordering::Acquire), 2);
        assert_eq!(active.load_acquire(), 2);
    });
}

#[test]
fn concurrent_budget_two_two_threads() {
    loom::model(|| {
        let remaining = Arc::new(LoomCounter(AtomicUsize::new(2)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let remaining = Arc::clone(&remaining);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_consume(&remaining) {
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(successes.load(Ordering::Acquire), 2);
        assert_eq!(remaining.load_acquire(), 0);
    });
}

#[test]
fn concurrent_budget_never_negative() {
    loom::model(|| {
        let remaining = Arc::new(LoomCounter(AtomicUsize::new(1)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let remaining = Arc::clone(&remaining);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_consume(&remaining) {
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(successes.load(Ordering::Acquire) <= 1);
        assert_eq!(remaining.load_acquire(), 0);
    });
}

struct Slot(Arc<LoomCounter>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.release_one();
    }
}

#[test]
fn concurrent_release_balances_slots() {
    loom::model(|| {
        let active = Arc::new(LoomCounter(AtomicUsize::new(0)));
        let successes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let active = Arc::clone(&active);
            let successes = Arc::clone(&successes);
            workers.push(thread::spawn(move || {
                if try_acquire(&active, 1) {
                    let _slot = Slot(Arc::clone(&active));
                    assert!(active.load_acquire() <= 1);
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(successes.load(Ordering::Acquire) >= 1);
        assert_eq!(active.load_acquire(), 0);
    });
}
