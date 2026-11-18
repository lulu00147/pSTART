//! Query generation tracking for the host UI.

/// Tracks the newest query and rejects stale responses.
#[derive(Clone, Debug, Default)]
pub struct QueryScheduler {
    generation: u64,
    pending: bool,
}

impl QueryScheduler {
    /// Start a new query, invalidating all older generations.
    pub fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.pending = true;
        self.generation
    }

    /// Invalidate the current query without starting a new one.
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
    }

    /// Accept a response only when it belongs to the current query.
    pub fn accept(&mut self, generation: u64) -> bool {
        if generation != self.generation || !self.pending {
            return false;
        }
        self.pending = false;
        true
    }

    /// Whether the current query is still awaiting a response.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Current generation, for diagnostics.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_response_is_rejected() {
        let mut scheduler = QueryScheduler::default();
        let first = scheduler.begin();
        let second = scheduler.begin();
        assert!(!scheduler.accept(first));
        assert!(scheduler.is_pending());
        assert!(scheduler.accept(second));
        assert!(!scheduler.is_pending());
    }

    #[test]
    fn invalidate_rejects_everything_until_next_query() {
        let mut scheduler = QueryScheduler::default();
        let generation = scheduler.begin();
        scheduler.invalidate();
        assert!(!scheduler.accept(generation));
        let next = scheduler.begin();
        assert!(scheduler.accept(next));
    }

    #[test]
    fn duplicate_response_is_rejected() {
        let mut scheduler = QueryScheduler::default();
        let generation = scheduler.begin();
        assert!(scheduler.accept(generation));
        assert!(!scheduler.accept(generation));
    }

    #[test]
    fn generation_wraps_without_panicking() {
        let mut scheduler = QueryScheduler {
            generation: u64::MAX,
            pending: false,
        };
        assert_eq!(scheduler.begin(), 0);
        assert_eq!(scheduler.generation(), 0);
    }
}
