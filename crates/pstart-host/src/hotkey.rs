//! Double-Ctrl gesture state machine.

/// Tracks Ctrl key transitions for a double-tap gesture.
#[derive(Clone, Debug, Default)]
pub struct CtrlTapState {
    down: bool,
    last_up: Option<u64>,
    suppress_next_up: bool,
    chord_used: bool,
}

impl CtrlTapState {
    /// Create an idle state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            down: false,
            last_up: None,
            suppress_next_up: false,
            chord_used: false,
        }
    }

    /// Record a Ctrl key-down; return true when a double-tap completes.
    pub fn key_down(&mut self, now: u64, interval: u64) -> bool {
        if self.down {
            return false;
        }
        self.down = true;
        self.chord_used = false;
        if let Some(last_up) = self.last_up {
            if now.saturating_sub(last_up) <= interval {
                self.last_up = None;
                self.suppress_next_up = true;
                return true;
            }
        }
        false
    }

    /// Record a Ctrl key-up.
    pub fn key_up(&mut self, now: u64) {
        if !self.down {
            return;
        }
        self.down = false;
        if self.suppress_next_up {
            self.suppress_next_up = false;
            self.last_up = None;
        } else if self.chord_used {
            self.chord_used = false;
            self.last_up = None;
        } else {
            self.last_up = Some(now);
        }
    }

    /// A non-Ctrl key cancels a pending tap and marks a chord.
    pub fn other_key(&mut self) {
        self.last_up = None;
        self.suppress_next_up = false;
        if self.down {
            self.chord_used = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: u64 = 400;

    #[test]
    fn key_up_without_key_down_is_a_noop() {
        let mut state = CtrlTapState::new();
        state.key_up(0);
    }

    #[test]
    fn double_ctrl_at_tick_zero_triggers() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(0, INTERVAL));
        state.key_up(0);
        assert!(state.key_down(0, INTERVAL));
    }

    #[test]
    fn double_ctrl_triggers_once() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(100, INTERVAL));
        state.key_up(110);
        assert!(state.key_down(200, INTERVAL));
        state.key_up(210);
        assert!(!state.key_down(300, INTERVAL));
    }

    #[test]
    fn double_ctrl_respects_interval() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(100, INTERVAL));
        state.key_up(110);
        assert!(!state.key_down(110 + INTERVAL + 1, INTERVAL));
    }

    #[test]
    fn ctrl_autorepeat_does_not_trigger() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(100, INTERVAL));
        assert!(!state.key_down(110, INTERVAL));
        state.key_up(120);
        assert!(state.key_down(200, INTERVAL));
    }

    #[test]
    fn other_key_cancels_pending_tap() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(100, INTERVAL));
        state.key_up(110);
        state.other_key();
        assert!(!state.key_down(200, INTERVAL));
    }

    #[test]
    fn ctrl_chord_is_not_a_tap() {
        let mut state = CtrlTapState::new();
        assert!(!state.key_down(100, INTERVAL));
        state.other_key();
        state.key_up(110);
        assert!(!state.key_down(200, INTERVAL));
    }
}
