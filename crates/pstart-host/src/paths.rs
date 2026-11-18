//! Query-result path validation.

use std::path::Path;

use pstart_core::FileItem;

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Drop results whose Windows absolute path no longer exists.
///
/// Non-file pseudo paths such as `plugin://calculator/1` are preserved.
/// Returns true when at least one result was dropped.
pub fn retain_existing_paths(items: &mut Vec<FileItem>) -> bool {
    let before = items.len();
    items.retain(|item| !is_windows_absolute(&item.path) || Path::new(&item.path).exists());
    items.len() != before
}

#[cfg(test)]
mod tests {
    use super::retain_existing_paths;
    use pstart_core::{FileId, FileItem, Frn, VolumeId};

    fn item(path: &str) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(1)),
            name: "x".to_owned(),
            path: path.to_owned(),
            attributes: 0,
            is_dir: false,
        }
    }

    #[test]
    fn removes_missing_windows_paths_and_keeps_pseudo_paths() {
        let mut items = vec![
            item("C:\\pstart-missing-path\\x.txt"),
            item("plugin://calculator/1"),
        ];
        assert!(retain_existing_paths(&mut items));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "plugin://calculator/1");
    }

    #[cfg(windows)]
    #[test]
    fn keeps_existing_windows_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("present.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut items = vec![item(&file.to_string_lossy())];
        assert!(!retain_existing_paths(&mut items));
        assert_eq!(items.len(), 1);
    }
}
