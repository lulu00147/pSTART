//! Arithmetic used by the pSTART scoring layer.

/// Bounded penalty for a name's character length.
pub fn length_penalty(chars: usize) -> i32 {
    let quotient = chars / 8;
    if quotient > 40 {
        40
    } else {
        quotient as i32
    }
}

/// Apply the base score minus the bounded length penalty.
pub fn apply_penalty(base: i32, chars: usize) -> i32 {
    (base - length_penalty(chars)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn penalty_is_bounded() {
        assert_eq!(length_penalty(0), 0);
        assert_eq!(length_penalty(319), 39);
        assert_eq!(length_penalty(320), 40);
        assert_eq!(length_penalty(usize::MAX), 40);
    }

    #[test]
    fn score_bounds_hold() {
        for base in [400, 550, 650, 800, 1000] {
            for chars in [0, 7, 8, 319, 320, usize::MAX] {
                let score = apply_penalty(base, chars);
                assert!((1..=base).contains(&score));
            }
        }
    }

    #[test]
    fn penalty_is_monotone() {
        assert!(length_penalty(0) <= length_penalty(319));
        assert!(length_penalty(319) <= length_penalty(320));
        assert!(length_penalty(320) <= length_penalty(usize::MAX));
    }
}
