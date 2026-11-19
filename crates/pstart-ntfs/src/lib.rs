//! NTFS volume and USN Journal access.
//!
//! Record parsing is platform-independent and unit-tested on every platform.
//! Volume handles and IOCTLs are compiled only on Windows.

#![deny(unsafe_code)]

pub mod error;
pub mod usn;

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 volume/IOCTL FFI boundary
mod volume;

pub use error::NtfsError;
pub use usn::{
    parse_journal_buffer, parse_mft_buffer, parse_usn_records, JournalBatch, JournalInfo, MftBatch,
    UsnRecord, MAX_RECORDS_PER_BUFFER, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME, USN_RECORD_V2_HEADER_LEN,
};

#[cfg(windows)]
pub use volume::{
    list_volumes, DriveType, SkippedVolume, VolumeEnumeration, VolumeHandle, VolumeInfo,
    DEFAULT_IOCTL_BUFFER_SIZE, MAX_IOCTL_BUFFER_SIZE, MIN_IOCTL_BUFFER_SIZE,
};

/// Whether this build can access NTFS APIs.
#[must_use]
pub const fn platform_supported() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_flag_matches_compile_target() {
        assert_eq!(platform_supported(), cfg!(windows));
    }
}
