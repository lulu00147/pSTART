//! Safe opening of indexed files and directories.

use std::io;
use std::path::PathBuf;

/// Open failure.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Indexed path is empty.
    #[error("path is empty")]
    EmptyPath,
    /// Indexed path contains NUL.
    #[error("path contains NUL")]
    ContainsNul,
    /// Path no longer exists.
    #[error("path no longer exists: {0}")]
    Missing(String),
    /// Plugin action target is not a regular file.
    #[error("plugin action target is not a file: {0}")]
    NotFile(String),
    /// Metadata lookup failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Validate that a path is non-empty, NUL-free, and currently exists.
pub fn validate_open_path(path: &str) -> Result<PathBuf, OpenError> {
    if path.is_empty() {
        return Err(OpenError::EmptyPath);
    }
    if path.contains('\0') {
        return Err(OpenError::ContainsNul);
    }
    let path = PathBuf::from(path);
    if !path.exists() {
        return Err(OpenError::Missing(path.display().to_string()));
    }
    let _metadata = std::fs::metadata(&path)?;
    Ok(path)
}

/// Validate a plugin-requested target: it must exist and be a regular file.
pub fn validate_plugin_action_path(path: &str) -> Result<PathBuf, OpenError> {
    let path = validate_open_path(path)?;
    if !std::fs::metadata(&path)?.is_file() {
        return Err(OpenError::NotFile(path.display().to_string()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn invalid_paths_are_rejected() {
        assert!(matches!(validate_open_path(""), Err(OpenError::EmptyPath)));
        assert!(matches!(
            validate_open_path("bad\0path"),
            Err(OpenError::ContainsNul)
        ));
        let missing = TempDir::new().unwrap().path().join("missing-file.txt");
        assert!(matches!(
            validate_open_path(&missing.display().to_string()),
            Err(OpenError::Missing(_))
        ));
    }

    #[test]
    fn existing_file_and_directory_validate() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            validate_open_path(&file.display().to_string()).unwrap(),
            file
        );
        assert_eq!(
            validate_open_path(&dir.path().display().to_string()).unwrap(),
            dir.path()
        );
    }

    #[test]
    fn plugin_actions_require_files() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            validate_plugin_action_path(&file.display().to_string()).unwrap(),
            file
        );
        assert!(matches!(
            validate_plugin_action_path(&dir.path().display().to_string()),
            Err(OpenError::NotFile(_))
        ));
        assert!(matches!(
            validate_plugin_action_path(""),
            Err(OpenError::EmptyPath)
        ));
    }
}
