//! Pure selection navigation for the result list.

/// Move the selected index by `delta`, clamping to the list bounds.
#[must_use]
pub fn move_selection(current: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = current.unwrap_or(0).min(len - 1) as i128;
    let max = len as i128 - 1;
    Some(current.saturating_add(delta as i128).clamp(0, max) as usize)
}

/// Compute the scroll offset that keeps `selected` inside the visible window.
#[must_use]
pub fn scroll_for_selection(scroll: usize, selected: usize, visible_rows: usize) -> usize {
    if selected < scroll {
        selected
    } else if visible_rows > 0 && selected >= scroll.saturating_add(visible_rows) {
        selected - visible_rows + 1
    } else {
        scroll
    }
}

/// Return the page-aligned start that contains `selected`.
#[must_use]
pub fn page_for_selection(len: usize, selected: usize, visible_rows: usize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let page = selected / visible_rows;
    let last_page = (len - 1) / visible_rows;
    page.min(last_page) * visible_rows
}

/// Clamp a page-aligned scroll offset to the last page.
#[must_use]
pub fn clamp_page(len: usize, scroll: usize, visible_rows: usize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let last_page = (len - 1) / visible_rows;
    (scroll / visible_rows).min(last_page) * visible_rows
}

/// Move one page forward (`delta >= 0`) or backward.
#[must_use]
pub fn page_by(len: usize, scroll: usize, visible_rows: usize, delta: isize) -> usize {
    if len == 0 || visible_rows == 0 {
        return 0;
    }
    let current = scroll / visible_rows;
    let last_page = (len - 1) / visible_rows;
    let page = if delta < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(last_page)
    };
    page * visible_rows
}

/// Clamp a requested index into `[0, len)`; empty lists have no index.
#[must_use]
pub fn selection_index(index: usize, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(index.min(len - 1))
    }
}

/// Move the scroll offset by `delta`, clamped to the scrollable range.
#[must_use]
pub fn scroll_by(scroll: usize, len: usize, visible_rows: usize, delta: isize) -> usize {
    let max = len.saturating_sub(visible_rows) as i128;
    (scroll as i128).saturating_add(delta as i128).clamp(0, max) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_has_no_selection() {
        assert_eq!(move_selection(None, 0, 1), None);
        assert_eq!(move_selection(Some(5), 0, -1), None);
        assert_eq!(selection_index(3, 0), None);
    }

    #[test]
    fn stale_selection_is_clamped() {
        assert_eq!(move_selection(Some(9), 5, 0), Some(4));
    }

    #[test]
    fn movement_is_clamped() {
        assert_eq!(move_selection(None, 3, -1), Some(0));
        assert_eq!(move_selection(Some(0), 3, 1), Some(1));
        assert_eq!(move_selection(Some(2), 3, 1), Some(2));
        assert_eq!(move_selection(Some(1), 3, -5), Some(0));
    }

    #[test]
    fn page_selection_stays_visible() {
        for len in [1, 9, 10, 18, 19, 50] {
            for selected in 0..len {
                let scroll = page_for_selection(len, selected, 9);
                assert!(selected >= scroll);
                assert!(selected < scroll + 9);
            }
        }
        assert_eq!(page_for_selection(50, 49, 9), 45);
        assert_eq!(page_for_selection(50, 0, 9), 0);
    }

    #[test]
    fn page_helpers_stay_aligned() {
        assert_eq!(clamp_page(50, 99, 9), 45);
        assert_eq!(clamp_page(9, 9, 9), 0);
        assert_eq!(page_by(50, 0, 9, 1), 9);
        assert_eq!(page_by(50, 45, 9, 1), 45);
        assert_eq!(page_by(50, 9, 9, -1), 0);
    }

    #[test]
    fn scrolling_keeps_selection_visible() {
        assert_eq!(scroll_for_selection(0, 2, 5), 0);
        assert_eq!(scroll_for_selection(10, 2, 5), 2);
        assert_eq!(scroll_for_selection(10, 15, 5), 11);
        assert_eq!(scroll_for_selection(10, 10, 5), 10);
    }

    #[test]
    fn selection_index_clamps() {
        assert_eq!(selection_index(0, 5), Some(0));
        assert_eq!(selection_index(9, 5), Some(4));
        assert_eq!(selection_index(3, 0), None);
    }

    #[test]
    fn scroll_by_clamps() {
        assert_eq!(scroll_by(10, 20, 5, 0), 10);
        assert_eq!(scroll_by(0, 20, 5, -5), 0);
        assert_eq!(scroll_by(99, 20, 5, 0), 15);
        assert_eq!(scroll_by(15, 20, 5, 5), 15);
        assert_eq!(scroll_by(0, 3, 5, 2), 0);
    }
}
