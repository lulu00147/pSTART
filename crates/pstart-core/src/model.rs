//! Wire-visible file result model.

use serde::{Deserialize, Serialize};

use crate::error::ModelError;
use crate::ids::FileId;
use crate::limits::{MAX_FILE_NAME_UTF16, MAX_PATH_UTF16};

/// Win32 `FILE_ATTRIBUTE_DIRECTORY`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// A file result that can be rendered by the Host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileItem {
    /// Stable identifier.
    pub id: FileId,
    /// File name only.
    pub name: String,
    /// Display path captured when the result was produced.
    pub path: String,
    /// Win32 file attributes.
    pub attributes: u32,
    /// Whether this record is a directory.
    pub is_dir: bool,
}

impl FileItem {
    /// Validate all hard limits before the item is sent on the wire.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.name.is_empty() {
            return Err(ModelError::EmptyFileName);
        }
        if self.name.contains('\0') {
            return Err(ModelError::FileNameContainsNul);
        }
        let name_len = self.name.encode_utf16().count();
        if name_len > MAX_FILE_NAME_UTF16 {
            return Err(ModelError::FileNameTooLong {
                actual: name_len,
                max: MAX_FILE_NAME_UTF16,
            });
        }
        if self.path.contains('\0') {
            return Err(ModelError::PathContainsNul);
        }
        let path_len = self.path.encode_utf16().count();
        if path_len > MAX_PATH_UTF16 {
            return Err(ModelError::PathTooLong {
                actual: path_len,
                max: MAX_PATH_UTF16,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Frn, VolumeId};

    fn item(name: &str, path: &str) -> FileItem {
        FileItem {
            id: FileId::new(VolumeId::new("C:").unwrap(), Frn::new(42)),
            name: name.to_owned(),
            path: path.to_owned(),
            attributes: 0,
            is_dir: false,
        }
    }

    #[test]
    fn valid_item_passes() {
        item("report.txt", "C:\\Users\\aa\\report.txt")
            .validate()
            .unwrap();
    }

    #[test]
    fn utf16_limits_are_inclusive() {
        let name = "a".repeat(MAX_FILE_NAME_UTF16);
        let path = "x".repeat(MAX_PATH_UTF16);
        item(&name, &path).validate().unwrap();
    }

    #[test]
    fn empty_name_is_rejected() {
        assert_eq!(item("", "C:\\x").validate(), Err(ModelError::EmptyFileName));
    }

    #[test]
    fn nul_is_rejected() {
        assert_eq!(
            item("a\0b", "C:\\x").validate(),
            Err(ModelError::FileNameContainsNul)
        );
    }

    #[test]
    fn long_name_is_rejected() {
        let name = "a".repeat(MAX_FILE_NAME_UTF16 + 1);
        assert!(matches!(
            item(&name, "C:\\x").validate(),
            Err(ModelError::FileNameTooLong { .. })
        ));
    }

    #[test]
    fn astral_name_counts_utf16_units() {
        let name = "😀".repeat(128); // 256 UTF-16 code units
        assert!(matches!(
            item(&name, "C:\\x").validate(),
            Err(ModelError::FileNameTooLong { .. })
        ));
    }

    #[test]
    fn postcard_roundtrip() {
        let value = item("报告.txt", "C:\\Users\\aa\\报告.txt");
        let bytes = postcard::to_allocvec(&value).unwrap();
        let back: FileItem = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn path_nul_is_rejected() {
        assert_eq!(
            item("a.txt", "C:\\bad\0path").validate(),
            Err(ModelError::PathContainsNul)
        );
    }

    #[test]
    fn long_path_is_rejected() {
        let path = "x".repeat(MAX_PATH_UTF16 + 1);
        assert!(matches!(
            item("a.txt", &path).validate(),
            Err(ModelError::PathTooLong { .. })
        ));
    }
}
