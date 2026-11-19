//! Index records and journal events.

use pstart_core::limits::MAX_FILE_NAME_UTF16;
use pstart_core::ModelError;
use serde::{Deserialize, Serialize};

/// One indexed file-system record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// File reference number within the volume.
    pub frn: u64,
    /// Parent directory FRN; zero for an unattached root.
    pub parent_frn: u64,
    /// File name only.
    pub name: Box<str>,
    /// Win32 file attributes.
    pub attributes: u32,
    /// Whether this record is a directory.
    pub is_dir: bool,
    /// USN of the event that last changed this record.
    pub last_usn: u64,
}

impl FileRecord {
    /// Validate record fields shared with the wire model.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.name.is_empty() {
            return Err(ModelError::EmptyFileName);
        }
        if self.name.contains('\0') {
            return Err(ModelError::FileNameContainsNul);
        }
        let length = self.name.encode_utf16().count();
        if length > MAX_FILE_NAME_UTF16 {
            return Err(ModelError::FileNameTooLong {
                actual: length,
                max: MAX_FILE_NAME_UTF16,
            });
        }
        Ok(())
    }
}

/// A journal event applied to the in-memory index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexEvent {
    /// Create or update a record. Rename and move are updates.
    Upsert(FileRecord),
    /// Delete a record. `usn` guards against applying stale deletes.
    Delete {
        /// File reference number.
        frn: u64,
        /// USN of the delete event.
        usn: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str) -> FileRecord {
        FileRecord {
            frn: 10,
            parent_frn: 5,
            name: name.into(),
            attributes: 0,
            is_dir: false,
            last_usn: 1,
        }
    }

    #[test]
    fn validates_name_bounds() {
        record("ok.txt").validate().unwrap();
        assert_eq!(record("").validate(), Err(ModelError::EmptyFileName));
        assert_eq!(
            record("a\0b").validate(),
            Err(ModelError::FileNameContainsNul)
        );
        let long = "x".repeat(MAX_FILE_NAME_UTF16 + 1);
        assert!(matches!(
            record(&long).validate(),
            Err(ModelError::FileNameTooLong { .. })
        ));
    }
}
