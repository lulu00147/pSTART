//! Component action path policy.

use std::path::{Path, PathBuf};

/// Whether `path` is inside one of the canonical `roots`.
#[must_use]
pub fn action_allowed(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_roots_deny_everything() {
        let dir = TempDir::new().unwrap();
        assert!(!action_allowed(&dir.path().join("a.txt"), &[]));
    }

    #[test]
    fn only_children_of_roots_are_allowed() {
        let root = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let roots = vec![root.path().to_path_buf()];
        assert!(action_allowed(&root.path().join("a.txt"), &roots));
        assert!(!action_allowed(&other.path().join("a.txt"), &roots));
        assert!(action_allowed(root.path(), &roots));
    }
}
