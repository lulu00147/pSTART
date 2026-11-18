//! Windows NTFS volume discovery and journal access.
//!
//! This module is compiled only on Windows. All Win32 calls are isolated here;
//! record parsing and index conversion live in `usn`.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS,
    ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    DRIVE_UNKNOWN,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::error::NtfsError;
use crate::usn::{parse_journal_buffer, parse_mft_buffer, JournalBatch, JournalInfo, MftBatch};

/// Default IOCTL output buffer size.
pub const DEFAULT_IOCTL_BUFFER_SIZE: usize = 64 * 1024;
/// Minimum accepted IOCTL output buffer size.
pub const MIN_IOCTL_BUFFER_SIZE: usize = 4096;
/// Maximum accepted IOCTL output buffer size.
pub const MAX_IOCTL_BUFFER_SIZE: usize = 1024 * 1024;

/// Win32 drive type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveType {
    /// Unknown drive type.
    Unknown,
    /// Path is not a root directory.
    NoRootDir,
    /// Removable drive.
    Removable,
    /// Fixed drive.
    Fixed,
    /// Network drive.
    Remote,
    /// CD-ROM drive.
    CdRom,
    /// RAM disk.
    RamDisk,
    /// A value newer than this crate knows about.
    Other(u32),
}

impl DriveType {
    /// Whether this is a fixed local drive.
    #[must_use]
    pub const fn is_fixed(self) -> bool {
        matches!(self, Self::Fixed)
    }
}

impl From<u32> for DriveType {
    fn from(value: u32) -> Self {
        match value {
            DRIVE_UNKNOWN => Self::Unknown,
            DRIVE_NO_ROOT_DIR => Self::NoRootDir,
            DRIVE_REMOVABLE => Self::Removable,
            DRIVE_FIXED => Self::Fixed,
            DRIVE_REMOTE => Self::Remote,
            DRIVE_CDROM => Self::CdRom,
            DRIVE_RAMDISK => Self::RamDisk,
            other => Self::Other(other),
        }
    }
}

/// Static information about one drive root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Drive letter.
    pub drive_letter: char,
    /// Normalized root, for example `C:\`.
    pub root: String,
    /// Volume label, possibly empty.
    pub volume_name: String,
    /// Filesystem name reported by Windows.
    pub filesystem: String,
    /// Volume serial number.
    pub serial_number: u32,
    /// Drive type.
    pub drive_type: DriveType,
}

impl VolumeInfo {
    /// Whether the volume uses NTFS.
    #[must_use]
    pub fn is_ntfs(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("NTFS")
    }
}

/// A volume root that could not be inspected.
#[derive(Debug)]
pub struct SkippedVolume {
    /// Root that failed.
    pub root: String,
    /// Failure reason.
    pub error: NtfsError,
}

/// Result of enumerating local drive roots.
#[derive(Debug, Default)]
pub struct VolumeEnumeration {
    /// Successfully inspected volumes.
    pub volumes: Vec<VolumeInfo>,
    /// Volumes that failed inspection; retained for diagnostics.
    pub skipped: Vec<SkippedVolume>,
}

impl VolumeEnumeration {
    /// NTFS volumes on fixed local drives.
    #[must_use]
    pub fn ntfs_fixed(&self) -> Vec<&VolumeInfo> {
        self.volumes
            .iter()
            .filter(|volume| volume.is_ntfs() && volume.drive_type.is_fixed())
            .collect()
    }
}

/// Enumerate drive roots and their filesystems.
pub fn list_volumes() -> Result<VolumeEnumeration, NtfsError> {
    // SAFETY: GetLogicalDrives has no pointer arguments and returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(last_error("GetLogicalDrives"));
    }
    Ok(enumerate_volumes_with(mask, volume_info_for_root))
}

fn enumerate_volumes_with<F>(mask: u32, mut inspect: F) -> VolumeEnumeration
where
    F: FnMut(&str) -> Result<VolumeInfo, NtfsError>,
{
    let mut enumeration = VolumeEnumeration::default();
    for index in 0..26u32 {
        if mask & (1u32 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root = format!("{letter}:\\");
        match inspect(&root) {
            Ok(volume) => enumeration.volumes.push(volume),
            Err(error) => enumeration.skipped.push(SkippedVolume { root, error }),
        }
    }
    enumeration
}

/// A live NTFS volume handle.
pub struct VolumeHandle {
    handle: HANDLE,
    root: String,
    device_path: String,
}

// SAFETY: a Win32 HANDLE is a process-wide kernel handle and may be moved to
// another thread. Methods take &self and do not retain pointers into caller
// stack frames after the IOCTL returns.
unsafe impl Send for VolumeHandle {}

impl VolumeHandle {
    /// Open a local NTFS volume root such as `C:` or `C:\`.
    pub fn open(root: &str) -> Result<Self, NtfsError> {
        let (letter, normalized_root) = normalize_drive_root(root)?;
        let info = volume_info_for_root(&normalized_root)?;
        ensure_ntfs(&info)?;

        let device_path = format!("\\\\.\\{letter}:");
        let handle = open_device(&device_path)?;
        Ok(Self {
            handle,
            root: normalized_root,
            device_path,
        })
    }

    /// Normalized volume root.
    #[must_use]
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Device path used to open the volume.
    #[must_use]
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Query current USN journal metadata.
    pub fn query_journal(&self) -> Result<JournalInfo, NtfsError> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        // SAFETY: `data` is a writable struct; the slice covers exactly its
        // initialized memory and is not aliased during the call.
        let output = unsafe {
            std::slice::from_raw_parts_mut(
                (&mut data as *mut USN_JOURNAL_DATA_V0).cast::<u8>(),
                size_of::<USN_JOURNAL_DATA_V0>(),
            )
        };
        let returned = self
            .ioctl(FSCTL_QUERY_USN_JOURNAL, null(), 0, output)
            .map_err(|error| map_journal_error(&self.root, error))?;
        journal_from_raw(data, returned)
    }

    /// Enumerate one page of MFT records.
    ///
    /// The caller resumes with `MftBatch::next_start_frn` until no progress is
    /// made. `buffer_size` is clamped to the documented 4 KiB..1 MiB range.
    pub fn enum_mft_page(
        &self,
        start_frn: u64,
        journal: &JournalInfo,
        buffer_size: usize,
    ) -> Result<MftBatch, NtfsError> {
        let buffer_size = validate_buffer_size(buffer_size)?;
        let input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: start_frn,
            LowUsn: 0,
            HighUsn: journal.next_usn,
        };
        let mut output = vec![0u8; buffer_size];
        let returned = match self.ioctl(
            FSCTL_ENUM_USN_DATA,
            (&input as *const MFT_ENUM_DATA_V0).cast::<c_void>(),
            size_of::<MFT_ENUM_DATA_V0>() as u32,
            &mut output,
        ) {
            Ok(returned) => returned,
            Err(NtfsError::Windows {
                code: ERROR_HANDLE_EOF,
                ..
            }) => {
                return Ok(MftBatch {
                    next_start_frn: 0,
                    records: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        parse_mft_output(&output[..returned])
    }

    /// Read a journal range.
    ///
    /// `timeout_ms == 0` returns immediately. The caller must not request a USN
    /// below `journal.lowest_valid_usn`.
    pub fn read_journal(
        &self,
        start_usn: i64,
        timeout_ms: u32,
        journal: &JournalInfo,
        buffer_size: usize,
    ) -> Result<JournalBatch, NtfsError> {
        if start_usn < 0 {
            return Err(NtfsError::InvalidRecord(
                "start_usn must be non-negative".to_owned(),
            ));
        }
        if start_usn < journal.lowest_valid_usn {
            return Err(NtfsError::JournalNotActive {
                root: self.root.clone(),
                code: ERROR_JOURNAL_ENTRY_DELETED,
            });
        }
        if start_usn > journal.next_usn {
            return Err(NtfsError::InvalidRecord(format!(
                "start_usn {start_usn} is ahead of journal next_usn {}",
                journal.next_usn
            )));
        }
        let buffer_size = validate_buffer_size(buffer_size)?;
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: start_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: u64::from(timeout_ms).saturating_mul(10_000),
            BytesToWaitFor: 0,
            UsnJournalID: journal.journal_id,
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        let mut output = vec![0u8; buffer_size];
        let returned = self
            .ioctl(
                FSCTL_READ_USN_JOURNAL,
                (&input as *const READ_USN_JOURNAL_DATA_V1).cast::<c_void>(),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                &mut output,
            )
            .map_err(|error| map_journal_error(&self.root, error))?;
        parse_journal_output(&output[..returned], start_usn)
    }

    fn ioctl(
        &self,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: &mut [u8],
    ) -> Result<usize, NtfsError> {
        if output.is_empty() || output.len() > MAX_IOCTL_BUFFER_SIZE {
            return Err(NtfsError::InvalidRecord(format!(
                "IOCTL output buffer {} is outside supported bounds",
                output.len()
            )));
        }
        let mut returned = 0u32;
        // SAFETY: the caller supplies a valid input pointer/size pair and an
        // output slice covering writable memory for the duration of the call.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                code,
                input,
                input_size,
                output.as_mut_ptr().cast::<c_void>(),
                output.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("DeviceIoControl"));
        }
        // DeviceIoControl promises not to report more bytes than the output
        // buffer holds; no defensive clamp is needed here.
        Ok(returned as usize)
    }
}

fn ensure_ntfs(info: &VolumeInfo) -> Result<(), NtfsError> {
    if info.is_ntfs() {
        Ok(())
    } else {
        Err(NtfsError::NotNtfs {
            root: info.root.clone(),
            filesystem: info.filesystem.clone(),
        })
    }
}

fn open_device(device_path: &str) -> Result<HANDLE, NtfsError> {
    let wide = wide(device_path);
    // SAFETY: `wide` is NUL-terminated and lives for the call. Null security
    // attributes and template handle are explicitly allowed.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            windows_sys::Win32::Foundation::GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error("CreateFileW"));
    }
    Ok(handle)
}

fn journal_from_raw(data: USN_JOURNAL_DATA_V0, returned: usize) -> Result<JournalInfo, NtfsError> {
    if returned < size_of::<USN_JOURNAL_DATA_V0>() {
        return Err(NtfsError::BufferTooSmall {
            actual: returned,
            minimum: size_of::<USN_JOURNAL_DATA_V0>(),
        });
    }
    let info = JournalInfo {
        journal_id: data.UsnJournalID,
        first_usn: data.FirstUsn,
        next_usn: data.NextUsn,
        lowest_valid_usn: data.LowestValidUsn,
        max_usn: data.MaxUsn,
        maximum_size: data.MaximumSize,
        allocation_delta: data.AllocationDelta,
    };
    info.validate()?;
    Ok(info)
}

fn parse_journal_output(bytes: &[u8], start_usn: i64) -> Result<JournalBatch, NtfsError> {
    if bytes.len() < 8 {
        return Err(NtfsError::BufferTooSmall {
            actual: bytes.len(),
            minimum: 8,
        });
    }
    let batch = parse_journal_buffer(bytes)?;
    if batch.next_usn < start_usn {
        return Err(NtfsError::InvalidRecord(format!(
            "journal returned next_usn {} behind start_usn {start_usn}",
            batch.next_usn
        )));
    }
    Ok(batch)
}

fn parse_mft_output(bytes: &[u8]) -> Result<MftBatch, NtfsError> {
    if bytes.len() < 8 {
        return Err(NtfsError::BufferTooSmall {
            actual: bytes.len(),
            minimum: 8,
        });
    }
    parse_mft_buffer(bytes)
}

fn map_journal_error(root: &str, error: NtfsError) -> NtfsError {
    match error {
        NtfsError::Windows { code, .. } if is_journal_unavailable(code) => {
            NtfsError::JournalNotActive {
                root: root.to_owned(),
                code,
            }
        }
        other => other,
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY: this handle was returned by CreateFileW and is closed once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

fn volume_info_for_root(root: &str) -> Result<VolumeInfo, NtfsError> {
    let (letter, normalized_root) = normalize_drive_root(root)?;
    let wide = wide(&normalized_root);

    // SAFETY: `wide` is NUL-terminated for the duration of the call.
    let drive_type = DriveType::from(unsafe { GetDriveTypeW(wide.as_ptr()) });
    if drive_type == DriveType::NoRootDir {
        return Err(NtfsError::InvalidRoot {
            root: root.to_owned(),
        });
    }

    let mut volume_name = [0u16; 261];
    let mut filesystem = [0u16; 16];
    let mut serial_number = 0u32;
    let mut maximum_component = 0u32;
    let mut filesystem_flags = 0u32;
    // SAFETY: all buffers are valid for their declared lengths; the path is
    // NUL-terminated.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            volume_name.as_mut_ptr(),
            volume_name.len() as u32,
            &mut serial_number,
            &mut maximum_component,
            &mut filesystem_flags,
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("GetVolumeInformationW"));
    }
    let _ = (maximum_component, filesystem_flags);
    let filesystem = decode_nul_terminated(&filesystem);
    Ok(VolumeInfo {
        drive_letter: letter,
        root: normalized_root,
        volume_name: decode_nul_terminated(&volume_name),
        filesystem,
        serial_number,
        drive_type,
    })
}

fn normalize_drive_root(root: &str) -> Result<(char, String), NtfsError> {
    let bytes = root.as_bytes();
    let valid_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if !valid_prefix {
        return Err(NtfsError::InvalidRoot {
            root: root.to_owned(),
        });
    }
    if bytes.len() > 2 {
        let separator = bytes[2];
        if bytes.len() != 3 || (separator != b'\\' && separator != b'/') {
            return Err(NtfsError::InvalidRoot {
                root: root.to_owned(),
            });
        }
    }
    let letter = (bytes[0] as char).to_ascii_uppercase();
    Ok((letter, format!("{letter}:\\")))
}

fn validate_buffer_size(requested: usize) -> Result<usize, NtfsError> {
    let size = if requested == 0 {
        DEFAULT_IOCTL_BUFFER_SIZE
    } else {
        requested
    };
    if !(MIN_IOCTL_BUFFER_SIZE..=MAX_IOCTL_BUFFER_SIZE).contains(&size) {
        return Err(NtfsError::InvalidRecord(format!(
            "buffer size {size} is outside {MIN_IOCTL_BUFFER_SIZE}..={MAX_IOCTL_BUFFER_SIZE}"
        )));
    }
    Ok(size)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn decode_nul_terminated(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn is_journal_unavailable(code: u32) -> bool {
    code == ERROR_JOURNAL_NOT_ACTIVE
        || code == ERROR_JOURNAL_ENTRY_DELETED
        || code == ERROR_JOURNAL_DELETE_IN_PROGRESS
}

fn last_error(operation: &'static str) -> NtfsError {
    // SAFETY: GetLastError has no arguments and reads thread-local state.
    let code = unsafe { GetLastError() };
    windows_error(operation, code)
}

fn windows_error(operation: &'static str, code: u32) -> NtfsError {
    let message = std::io::Error::from_raw_os_error(code as i32).to_string();
    NtfsError::Windows {
        operation,
        code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_type_mapping_is_stable() {
        assert_eq!(DriveType::from(DRIVE_UNKNOWN), DriveType::Unknown);
        assert_eq!(DriveType::from(DRIVE_REMOVABLE), DriveType::Removable);
        assert_eq!(DriveType::from(DRIVE_FIXED), DriveType::Fixed);
        assert_eq!(DriveType::from(DRIVE_REMOTE), DriveType::Remote);
        assert_eq!(DriveType::from(DRIVE_CDROM), DriveType::CdRom);
        assert_eq!(DriveType::from(DRIVE_RAMDISK), DriveType::RamDisk);
        assert_eq!(DriveType::from(99), DriveType::Other(99));
        assert!(DriveType::Fixed.is_fixed());
        assert!(!DriveType::Remote.is_fixed());
    }

    #[test]
    fn drive_root_normalization() {
        assert_eq!(
            normalize_drive_root("C:").unwrap(),
            ('C', "C:\\".to_owned())
        );
        assert_eq!(
            normalize_drive_root("c:\\").unwrap(),
            ('C', "C:\\".to_owned())
        );
        assert_eq!(
            normalize_drive_root("D:/").unwrap(),
            ('D', "D:\\".to_owned())
        );
        assert!(normalize_drive_root("C:\\Windows").is_err());
        assert!(normalize_drive_root("").is_err());
        assert!(normalize_drive_root("1:").is_err());
        assert!(normalize_drive_root("C").is_err());
    }

    #[test]
    fn buffer_size_is_clamped_and_validated() {
        assert_eq!(validate_buffer_size(0).unwrap(), DEFAULT_IOCTL_BUFFER_SIZE);
        assert_eq!(
            validate_buffer_size(MIN_IOCTL_BUFFER_SIZE).unwrap(),
            MIN_IOCTL_BUFFER_SIZE
        );
        assert!(validate_buffer_size(MIN_IOCTL_BUFFER_SIZE - 1).is_err());
        assert!(validate_buffer_size(MAX_IOCTL_BUFFER_SIZE + 1).is_err());
    }

    #[test]
    fn nul_terminated_strings_are_decoded() {
        let units = [b'C' as u16, b':' as u16, 0, b'x' as u16];
        assert_eq!(decode_nul_terminated(&units), "C:");
        assert_eq!(decode_nul_terminated(&[]), "");
    }

    #[test]
    fn volume_listing_contains_ntfs_system_drive() {
        let enumeration = list_volumes().expect("list volumes");
        assert!(
            !enumeration.volumes.is_empty(),
            "no volumes enumerated; skipped: {:?}",
            enumeration.skipped
        );
        let system = enumeration
            .volumes
            .iter()
            .find(|volume| volume.root.eq_ignore_ascii_case("C:\\"));
        assert!(
            system.is_some(),
            "C:\\ not found; skipped: {:?}",
            enumeration.skipped
        );
        assert!(system.unwrap().is_ntfs());
        let ntfs = enumeration.ntfs_fixed();
        assert!(!ntfs.is_empty());
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_journal_query() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        assert_eq!(handle.root(), "C:\\");
        assert_eq!(handle.device_path(), "\\\\.\\C:");
        let journal = handle.query_journal().unwrap();
        assert!(journal.journal_id != 0);
        assert!(journal.next_usn >= journal.first_usn);
        assert!(journal.maximum_size >= 4096);
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_mft_page() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        let journal = handle.query_journal().unwrap();
        let page = handle
            .enum_mft_page(0, &journal, DEFAULT_IOCTL_BUFFER_SIZE)
            .unwrap();
        assert!(!page.records.is_empty());
        assert!(page.next_start_frn > 0);
        assert!(page.records.iter().all(|record| !record.name.is_empty()));
        assert!(page.records.iter().any(|record| record.frn >= 5));
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_mft_pages_advance() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        let journal = handle.query_journal().unwrap();
        let mut start = 0u64;
        for _ in 0..3 {
            let page = handle
                .enum_mft_page(start, &journal, DEFAULT_IOCTL_BUFFER_SIZE)
                .unwrap();
            assert!(!page.records.is_empty());
            assert!(
                page.next_start_frn > start,
                "MFT page did not advance: start={start}, next={}",
                page.next_start_frn
            );
            start = page.next_start_frn;
        }
        assert!(start > 0);
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_mft_reaches_eof() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        let journal = handle.query_journal().unwrap();
        let mut start = 0u64;
        let mut pages = 0usize;
        let final_page = loop {
            let page = handle
                .enum_mft_page(start, &journal, DEFAULT_IOCTL_BUFFER_SIZE)
                .unwrap();
            pages += 1;
            if page.next_start_frn == 0 {
                break page;
            }
            assert!(
                page.next_start_frn > start,
                "MFT page did not advance: start={start}, next={}",
                page.next_start_frn
            );
            start = page.next_start_frn;
        };
        assert!(pages > 0);
        assert!(final_page.records.is_empty());
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_journal_read_returns_monotonic_position() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        let journal = handle.query_journal().unwrap();
        let batch = handle
            .read_journal(journal.next_usn, 0, &journal, DEFAULT_IOCTL_BUFFER_SIZE)
            .unwrap();
        assert!(batch.next_usn >= journal.next_usn);
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn live_journal_observes_created_file() {
        let handle = VolumeHandle::open("C:\\").unwrap();
        let journal = handle.query_journal().unwrap();
        let name = format!(
            "pstart-usn-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(&name);
        std::fs::write(&path, b"pstart").unwrap();
        let result =
            handle.read_journal(journal.next_usn, 2000, &journal, DEFAULT_IOCTL_BUFFER_SIZE);
        let _ = std::fs::remove_file(&path);
        let batch = result.unwrap();
        let found = batch.records.iter().any(|record| {
            record.name.eq_ignore_ascii_case(&name)
                && record.reason & crate::USN_REASON_FILE_CREATE != 0
        });
        assert!(found, "created file was not observed in the journal");
    }

    #[test]
    #[ignore = "requires elevated access to a live NTFS volume"]
    fn opening_missing_root_is_rejected() {
        let result = VolumeHandle::open("Z:\\");
        assert!(result.is_err());
    }

    fn fake_volume(filesystem: &str) -> VolumeInfo {
        VolumeInfo {
            drive_letter: 'C',
            root: "C:\\".to_owned(),
            volume_name: "test".to_owned(),
            filesystem: filesystem.to_owned(),
            serial_number: 1,
            drive_type: DriveType::Fixed,
        }
    }

    fn fake_handle() -> VolumeHandle {
        VolumeHandle {
            handle: INVALID_HANDLE_VALUE,
            root: "C:\\".to_owned(),
            device_path: "\\\\.\\C:".to_owned(),
        }
    }

    fn raw_journal() -> USN_JOURNAL_DATA_V0 {
        USN_JOURNAL_DATA_V0 {
            UsnJournalID: 1,
            FirstUsn: 10,
            NextUsn: 20,
            LowestValidUsn: 0,
            MaxUsn: 100,
            MaximumSize: 1024,
            AllocationDelta: 64,
        }
    }

    fn valid_journal() -> JournalInfo {
        JournalInfo {
            journal_id: 1,
            first_usn: 10,
            next_usn: 20,
            lowest_valid_usn: 0,
            max_usn: 100,
            maximum_size: 1024,
            allocation_delta: 64,
        }
    }

    #[test]
    fn ensure_ntfs_rejects_other_filesystems() {
        assert!(ensure_ntfs(&fake_volume("NTFS")).is_ok());
        assert!(matches!(
            ensure_ntfs(&fake_volume("FAT32")),
            Err(NtfsError::NotNtfs { .. })
        ));
    }

    #[test]
    fn enumeration_handles_success_skip_and_empty_mask() {
        let enumeration = enumerate_volumes_with(0, |_| Ok(fake_volume("NTFS")));
        assert!(enumeration.volumes.is_empty());
        assert!(enumeration.skipped.is_empty());

        let enumeration = enumerate_volumes_with(1 << 2, |_| Ok(fake_volume("NTFS")));
        assert_eq!(enumeration.volumes.len(), 1);
        assert_eq!(enumeration.volumes[0].root, "C:\\");

        let enumeration = enumerate_volumes_with(1 << 25, |root| {
            Err(NtfsError::InvalidRoot {
                root: root.to_owned(),
            })
        });
        assert!(enumeration.volumes.is_empty());
        assert_eq!(enumeration.skipped.len(), 1);
        assert_eq!(enumeration.skipped[0].root, "Z:\\");
    }

    #[test]
    fn open_device_failure_is_reported() {
        let result = open_device("\\\\.\\pstart-no-such-device");
        assert!(matches!(result, Err(NtfsError::Windows { .. })));
    }

    #[test]
    fn invalid_handle_errors_are_reported() {
        let handle = fake_handle();
        assert!(handle.query_journal().is_err());
        let mut buffer = vec![0u8; MIN_IOCTL_BUFFER_SIZE];
        assert!(handle
            .ioctl(FSCTL_QUERY_USN_JOURNAL, null(), 0, &mut buffer)
            .is_err());
    }

    #[test]
    fn ioctl_buffer_bounds_are_enforced() {
        let handle = fake_handle();
        assert!(handle.ioctl(0, null(), 0, &mut []).is_err());
        let mut too_large = vec![0u8; MAX_IOCTL_BUFFER_SIZE + 1];
        assert!(handle.ioctl(0, null(), 0, &mut too_large).is_err());
    }

    #[test]
    fn read_journal_rejects_invalid_positions_and_buffers() {
        let handle = fake_handle();
        let journal = valid_journal();
        assert!(matches!(
            handle.read_journal(-1, 0, &journal, 4096),
            Err(NtfsError::InvalidRecord(_))
        ));
        let mut range_lost = journal.clone();
        range_lost.lowest_valid_usn = 10;
        assert!(matches!(
            handle.read_journal(0, 0, &range_lost, 4096),
            Err(NtfsError::JournalNotActive { .. })
        ));
        assert!(handle.read_journal(5, 0, &range_lost, 4096).is_err());
        assert!(handle.read_journal(101, 0, &journal, 4096).is_err());
        assert!(handle.read_journal(10, 0, &journal, 1).is_err());
    }

    #[test]
    fn enum_mft_rejects_invalid_buffer_before_ioctl() {
        let handle = fake_handle();
        assert!(handle.enum_mft_page(0, &valid_journal(), 1).is_err());
        assert!(handle
            .enum_mft_page(0, &valid_journal(), MIN_IOCTL_BUFFER_SIZE)
            .is_err());
    }

    #[test]
    fn journal_helpers_map_errors_and_boundaries() {
        let mapped = map_journal_error(
            "C:\\",
            NtfsError::Windows {
                operation: "DeviceIoControl",
                code: ERROR_JOURNAL_NOT_ACTIVE,
                message: "not active".to_owned(),
            },
        );
        assert!(matches!(mapped, NtfsError::JournalNotActive { .. }));
        let unchanged = map_journal_error(
            "C:\\",
            NtfsError::Windows {
                operation: "DeviceIoControl",
                code: 6,
                message: "invalid handle".to_owned(),
            },
        );
        assert!(matches!(unchanged, NtfsError::Windows { code: 6, .. }));

        assert!(matches!(
            journal_from_raw(raw_journal(), 0),
            Err(NtfsError::BufferTooSmall { .. })
        ));
        let info = journal_from_raw(raw_journal(), size_of::<USN_JOURNAL_DATA_V0>()).unwrap();
        assert_eq!(info.next_usn, 20);

        assert!(matches!(
            parse_journal_output(&[0; 7], 0),
            Err(NtfsError::BufferTooSmall { .. })
        ));
        let mut behind = 1i64.to_le_bytes().to_vec();
        assert!(matches!(
            parse_journal_output(&behind, 2),
            Err(NtfsError::InvalidRecord(_))
        ));
        behind = 2i64.to_le_bytes().to_vec();
        assert_eq!(parse_journal_output(&behind, 2).unwrap().next_usn, 2);

        assert!(matches!(
            parse_mft_output(&[0; 7]),
            Err(NtfsError::BufferTooSmall { .. })
        ));
        assert_eq!(
            parse_mft_output(&9u64.to_le_bytes())
                .unwrap()
                .next_start_frn,
            9
        );
    }

    #[test]
    fn journal_error_codes_are_classified() {
        assert!(is_journal_unavailable(ERROR_JOURNAL_NOT_ACTIVE));
        assert!(is_journal_unavailable(ERROR_JOURNAL_ENTRY_DELETED));
        assert!(is_journal_unavailable(ERROR_JOURNAL_DELETE_IN_PROGRESS));
        assert!(!is_journal_unavailable(5));
        let error = windows_error("test", 5);
        assert!(matches!(
            error,
            NtfsError::Windows {
                operation: "test",
                code: 5,
                ..
            }
        ));
    }
}
