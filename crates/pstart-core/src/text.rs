//! Allocation-aware case-insensitive text matching.

/// Case-insensitive substring check with an ASCII allocation-free fast path.
///
/// Non-ASCII inputs use Unicode lowercasing; the case mapping table itself is
/// provided by the standard library.
#[must_use]
pub fn contains_case_insensitive(name: &str, needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    contains_lowercase_needle(name, &needle_lower)
}

/// Case-insensitive substring test with an already-lowercased needle.
///
/// Use this in hot loops to avoid reallocating the needle for every name.
pub fn contains_lowercase_needle(name: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if name.is_ascii() {
        let haystack = name.as_bytes();
        let needle = needle_lower.as_bytes();
        if needle.len() > haystack.len() {
            return false;
        }
        let first = needle[0].to_ascii_lowercase();
        let last_start = haystack.len() - needle.len();
        for start in 0..=last_start {
            if haystack[start].to_ascii_lowercase() != first {
                continue;
            }
            if haystack[start..start + needle.len()].eq_ignore_ascii_case(needle) {
                return true;
            }
        }
        return false;
    }
    name.to_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_prefilter_matches_lowercase_semantics() {
        assert!(contains_case_insensitive("REPORT.TXT", "report"));
        assert!(contains_case_insensitive("项目报告.docx", "报告"));
        assert!(contains_case_insensitive("CAFÉ.TXT", "café"));
        assert!(contains_case_insensitive("café.txt", "CAFÉ"));
        assert!(!contains_case_insensitive("abc", "x"));
        assert!(contains_case_insensitive("abc", ""));
    }
}
