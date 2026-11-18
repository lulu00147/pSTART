//! Stable identifiers.
//!
//! FRNs are only unique within a volume. A file result identifier is therefore
//! always the pair `(VolumeId, Frn)` and its string form is `volume:frn`.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::ModelError;
use crate::limits::MAX_VOLUME_ID_LEN;

/// Validated volume identifier, for example `C:` or a volume GUID string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VolumeId(String);

impl VolumeId {
    /// Construct a validated volume identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ModelError::EmptyVolumeId);
        }
        if value.len() > MAX_VOLUME_ID_LEN {
            return Err(ModelError::VolumeIdTooLong {
                actual: value.len(),
                max: MAX_VOLUME_ID_LEN,
            });
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(ModelError::VolumeIdInvalidCharacter);
        }
        Ok(Self(value))
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for VolumeId {
    type Error = ModelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<VolumeId> for String {
    fn from(value: VolumeId) -> Self {
        value.0
    }
}

/// NTFS file reference number, unique only within its volume.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Frn(u64);

impl Frn {
    /// NTFS root directory record number.
    pub const ROOT: Self = Self(5);

    /// Construct a file reference number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the low 48-bit MFT record number, without the sequence.
    #[must_use]
    pub const fn record_number(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }

    /// Return the raw integer value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Frn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable result identifier: a volume identifier plus a file reference number.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId {
    /// Volume containing the file.
    pub volume: VolumeId,
    /// File reference number within the volume.
    pub frn: Frn,
}

impl FileId {
    /// Construct a stable file identifier.
    #[must_use]
    pub const fn new(volume: VolumeId, frn: Frn) -> Self {
        Self { volume, frn }
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.volume, self.frn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_id_accepts_normal_values() {
        assert_eq!(VolumeId::new("C:").unwrap().as_str(), "C:");
        assert_eq!(
            VolumeId::new("\\\\?\\Volume{abc}").unwrap().as_str(),
            "\\\\?\\Volume{abc}"
        );
    }

    #[test]
    fn volume_id_rejects_empty() {
        assert_eq!(VolumeId::new(""), Err(ModelError::EmptyVolumeId));
    }

    #[test]
    fn volume_id_rejects_too_long() {
        let value = "x".repeat(MAX_VOLUME_ID_LEN + 1);
        assert!(matches!(
            VolumeId::new(value),
            Err(ModelError::VolumeIdTooLong { .. })
        ));
    }

    #[test]
    fn volume_id_rejects_control_and_whitespace() {
        assert_eq!(
            VolumeId::new("C:\u{7}"),
            Err(ModelError::VolumeIdInvalidCharacter)
        );
        assert_eq!(
            VolumeId::new("C:\n"),
            Err(ModelError::VolumeIdInvalidCharacter)
        );
        assert_eq!(
            VolumeId::new("C: "),
            Err(ModelError::VolumeIdInvalidCharacter)
        );
        assert_eq!(
            VolumeId::new(String::from("C:\u{7}")),
            Err(ModelError::VolumeIdInvalidCharacter)
        );
        assert_eq!(
            VolumeId::new(String::from("C: ")),
            Err(ModelError::VolumeIdInvalidCharacter)
        );
    }

    #[test]
    fn file_id_display_is_stable() {
        let id = FileId::new(VolumeId::new("C:").unwrap(), Frn::new(42));
        assert_eq!(id.to_string(), "C::42");
    }

    #[test]
    fn volume_id_serde_validates() {
        let id: VolumeId = serde_json::from_str("\"C:\"").unwrap();
        assert_eq!(id.as_str(), "C:");
        assert!(serde_json::from_str::<VolumeId>("\"\"").is_err());
    }

    #[test]
    fn file_id_postcard_roundtrip() {
        let id = FileId::new(VolumeId::new("D:").unwrap(), Frn::new(99));
        let bytes = postcard::to_allocvec(&id).unwrap();
        let back: FileId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, back);
    }
}
