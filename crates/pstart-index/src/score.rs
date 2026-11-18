//! Explainable, deterministic name scoring.
//!
//! Scores are only used for ordering. They are not security decisions.

use pstart_score::apply_penalty;

const SEPARATORS: &[char] = &[' ', '.', '_', '-', '\\', '/', '(', ')', '[', ']', '+', ','];

/// Exact name match.
pub const SCORE_EXACT: i32 = 1000;
/// Name starts with the query.
pub const SCORE_PREFIX: i32 = 800;
/// A word boundary starts with the query.
pub const SCORE_WORD_PREFIX: i32 = 650;
/// A camel-case or digit boundary starts with the query.
pub const SCORE_CAMEL_PREFIX: i32 = 550;
/// The query appears elsewhere in the name.
pub const SCORE_SUBSTRING: i32 = 400;
/// Maximum penalty applied for a deeply nested path.
pub const SCORE_DEPTH_PENALTY_MAX: i32 = 20;
/// Penalty for a directory at the same name-match class.
pub const SCORE_DIRECTORY_PENALTY: i32 = 50;

/// Score one file name against a query.
///
/// Returns `None` when the name does not contain the query. Longer names get a
/// small bounded penalty so that, within the same match class, shorter names
/// rank first.
#[must_use]
pub fn score_name(name: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return None;
    }
    if name.is_empty() {
        return None;
    }
    let name_lower = name.to_lowercase();
    let query_lower = query.to_lowercase();

    let base = if name_lower == query_lower {
        SCORE_EXACT
    } else if name_lower.starts_with(&query_lower) {
        SCORE_PREFIX
    } else if word_prefix(&name_lower, &query_lower) {
        SCORE_WORD_PREFIX
    } else if camel_prefix(name, &query_lower) {
        SCORE_CAMEL_PREFIX
    } else if name_lower.contains(&query_lower) {
        SCORE_SUBSTRING
    } else {
        return None;
    };

    Some(apply_penalty(base, name.chars().count()))
}

fn word_prefix(name_lower: &str, query_lower: &str) -> bool {
    name_lower
        .split(|c: char| SEPARATORS.contains(&c))
        .any(|token| {
            if token.is_empty() {
                return false;
            }
            token.starts_with(query_lower)
        })
}

#[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
fn camel_prefix(name: &str, query_lower: &str) -> bool {
    let mut previous: Option<char> = None;
    for (byte_index, current) in name.char_indices() {
        if let Some(previous) = previous {
            let mut boundary = false;
            if current.is_uppercase() {
                if previous.is_lowercase() {
                    boundary = true;
                }
            }
            if current.is_alphabetic() {
                if previous.is_numeric() {
                    boundary = true;
                }
            }
            if current.is_numeric() {
                if previous.is_alphabetic() {
                    boundary = true;
                }
            }
            if boundary {
                if name[byte_index..].to_lowercase().starts_with(query_lower) {
                    return true;
                }
            }
        }
        previous = Some(current);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn match_classes_are_ordered() {
        assert_eq!(score_name("report", "report"), Some(SCORE_EXACT));
        let prefix = score_name("report.txt", "report").unwrap();
        assert!(prefix < SCORE_PREFIX);
        assert!(prefix > SCORE_WORD_PREFIX);
        let word = score_name("annual_report.txt", "report").unwrap();
        let camel = score_name("annualReport.txt", "report").unwrap();
        let substring = score_name("xxreportxx.txt", "report").unwrap();
        assert!(SCORE_PREFIX > word);
        assert!(word > camel);
        assert!(camel > substring);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(score_name("REPORT.TXT", "report").is_some());
        assert!(score_name("Report.Txt", "REPORT").is_some());
    }

    #[test]
    fn all_ascii_single_char_pairs_match_casefold_contains() {
        for name_byte in 0u8..128 {
            for query_byte in 0u8..128 {
                let name = char::from(name_byte).to_string();
                let query = char::from(query_byte).to_string();
                let expected = name
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase());
                assert_eq!(
                    score_name(&name, &query).is_some(),
                    expected,
                    "name={name_byte} query={query_byte}"
                );
            }
        }
    }

    #[test]
    fn non_matching_name_returns_none() {
        assert_eq!(score_name("report.txt", "invoice"), None);
    }

    #[test]
    fn empty_inputs_return_none() {
        assert_eq!(score_name("", "a"), None);
        assert_eq!(score_name("a", ""), None);
    }

    #[test]
    fn unicode_queries_work() {
        assert!(score_name("项目报告.docx", "报告").is_some());
        assert!(score_name("项目报告.docx", "项目").is_some());
    }

    proptest! {
        #[test]
        fn score_some_iff_case_insensitive_contains(
            name in "\\PC{0,40}",
            query in "\\PC{1,8}",
        ) {
            let expected = !name.is_empty()
                && name.to_lowercase().contains(&query.to_lowercase());
            prop_assert_eq!(score_name(&name, &query).is_some(), expected);
        }
    }

    #[test]
    fn score_is_deterministic() {
        let first = score_name("annual_report_final.txt", "report");
        let second = score_name("annual_report_final.txt", "report");
        assert_eq!(first, second);
    }

    #[test]
    fn longer_names_score_lower_within_class() {
        let short = score_name("report.txt", "report").unwrap();
        let long = score_name("report-2026-final-version.txt", "report").unwrap();
        assert!(short > long);
    }

    #[test]
    fn camel_boundary_detects_digit_transition() {
        assert!(score_name("file2report.txt", "report").is_some());
    }

    #[test]
    fn camel_boundaries_cover_all_transitions() {
        assert!(camel_prefix("fileReport", "report"));
        assert!(camel_prefix("2Report", "report"));
        assert!(camel_prefix("report2name", "name"));
        assert!(!camel_prefix("fileXylophone", "report"));
        assert!(!camel_prefix("report", "report"));
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_never_panics(
            name in "\\PC{0,32}",
            query in "\\PC{0,16}",
        ) {
            let _ = score_name(&name, &query);
        }

        #[test]
        fn empty_query_is_always_none(name in "\\PC{0,32}") {
            prop_assert_eq!(score_name(&name, ""), None);
        }
    }
}
