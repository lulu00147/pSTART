//! USN record parsing and conversion to index events.
//!
//! The parser is deliberately platform-independent so that malformed-record
//! behaviour can be tested on Linux as well as Windows.

use pstart_core::limits::MAX_FILE_NAME_UTF16;
use pstart_core::FILE_ATTRIBUTE_DIRECTORY;
use pstart_index::{FileRecord, IndexEvent};

use crate::error::NtfsError;

/// Fixed portion of a `USN_RECORD_V2` record.
pub const USN_RECORD_V2_HEADER_LEN: usize = 60;

/// Maximum records accepted from one IOCTL buffer.
pub const MAX_RECORDS_PER_BUFFER: usize = 8192;

/// File was created.
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
/// File was deleted.
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
/// Old name of a rename operation.
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// New name of a rename operation.
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;

/// USN journal metadata returned by `FSCTL_QUERY_USN_JOURNAL`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    /// Journal identifier. Must match persisted index state.
    pub journal_id: u64,
    /// First USN that is currently available.
    pub first_usn: i64,
    /// Next USN to be assigned.
    pub next_usn: i64,
    /// Lowest USN still considered valid.
    pub lowest_valid_usn: i64,
    /// Maximum USN the journal can hold.
    pub max_usn: i64,
    /// Maximum journal size in bytes.
    pub maximum_size: u64,
    /// Allocation delta in bytes.
    pub allocation_delta: u64,
}

impl JournalInfo {
    /// Validate relationships between journal fields.
    pub fn validate(&self) -> Result<(), NtfsError> {
        if self.journal_id == 0 {
            return Err(NtfsError::InvalidRecord(
                "journal id must be non-zero".to_owned(),
            ));
        }
        if self.next_usn < self.first_usn {
            return Err(NtfsError::InvalidRecord(format!(
                "next_usn {} is behind first_usn {}",
                self.next_usn, self.first_usn
            )));
        }
        if self.first_usn < self.lowest_valid_usn {
            return Err(NtfsError::InvalidRecord(format!(
                "first_usn {} is behind lowest_valid_usn {}",
                self.first_usn, self.lowest_valid_usn
            )));
        }
        if self.max_usn < self.next_usn {
            return Err(NtfsError::InvalidRecord(format!(
                "max_usn {} is behind next_usn {}",
                self.max_usn, self.next_usn
            )));
        }
        if self.maximum_size == 0 {
            return Err(NtfsError::InvalidRecord(
                "journal maximum size must be non-zero".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Parsed `USN_RECORD_V2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsnRecord {
    /// File reference number.
    pub frn: u64,
    /// Parent directory FRN.
    pub parent_frn: u64,
    /// USN assigned to this change.
    pub usn: i64,
    /// NTFS timestamp.
    pub timestamp: i64,
    /// Change reason bitmask.
    pub reason: u32,
    /// Source information.
    pub source_info: u32,
    /// Security identifier.
    pub security_id: u32,
    /// Win32 file attributes.
    pub attributes: u32,
    /// File name.
    pub name: String,
}

impl UsnRecord {
    /// Whether the record is a directory.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Whether the record represents a deletion.
    #[must_use]
    pub fn is_delete(&self) -> bool {
        self.reason & USN_REASON_FILE_DELETE != 0
    }

    /// Convert to an index record.
    pub fn to_file_record(&self) -> Result<FileRecord, NtfsError> {
        if self.frn == 0 {
            return Err(NtfsError::InvalidRecord(
                "FRN zero is not a valid file".to_owned(),
            ));
        }
        if self.usn < 0 {
            return Err(NtfsError::InvalidRecord(
                "negative USN cannot be indexed".to_owned(),
            ));
        }
        if self.name.is_empty() {
            return Err(NtfsError::InvalidRecord(
                "empty file name cannot be indexed".to_owned(),
            ));
        }
        if self.name.contains('\0') {
            return Err(NtfsError::InvalidRecord(
                "file name contains NUL".to_owned(),
            ));
        }
        if self.name.contains('\\') || self.name.contains('/') {
            return Err(NtfsError::InvalidRecord(
                "file name contains a path separator".to_owned(),
            ));
        }
        if self.name.encode_utf16().count() > MAX_FILE_NAME_UTF16 {
            return Err(NtfsError::InvalidRecord(format!(
                "file name exceeds {MAX_FILE_NAME_UTF16} UTF-16 units"
            )));
        }
        Ok(FileRecord {
            frn: self.frn,
            parent_frn: self.parent_frn,
            name: self.name.clone().into(),
            attributes: self.attributes,
            is_dir: self.is_directory(),
            last_usn: self.usn as u64,
        })
    }

    /// Map this record to an index event.
    ///
    /// The old-name half of a rename is ignored; the following new-name record
    /// carries the current name. A delete record takes priority over rename bits
    /// because the file no longer exists.
    pub fn to_index_event(&self) -> Result<Option<IndexEvent>, NtfsError> {
        if self.is_delete() {
            if self.usn < 0 {
                return Err(NtfsError::InvalidRecord(
                    "negative USN cannot be indexed".to_owned(),
                ));
            }
            return Ok(Some(IndexEvent::Delete {
                frn: self.frn,
                usn: self.usn as u64,
            }));
        }
        if self.reason & USN_REASON_RENAME_OLD_NAME != 0
            && self.reason & USN_REASON_RENAME_NEW_NAME == 0
        {
            return Ok(None);
        }
        self.to_file_record()
            .map(|record| Some(IndexEvent::Upsert(record)))
    }
}

/// A parsed journal read: next position plus records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalBatch {
    /// Next USN to read.
    pub next_usn: i64,
    /// Records returned in this batch.
    pub records: Vec<UsnRecord>,
}

/// A parsed MFT enumeration page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MftBatch {
    /// Start FRN for the next page.
    pub next_start_frn: u64,
    /// Records returned in this page.
    pub records: Vec<UsnRecord>,
}

/// Parse a sequence of `USN_RECORD_V2` records.
pub fn parse_usn_records(bytes: &[u8]) -> Result<Vec<UsnRecord>, NtfsError> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if records.len() >= MAX_RECORDS_PER_BUFFER {
            return Err(NtfsError::TooManyRecords {
                limit: MAX_RECORDS_PER_BUFFER,
            });
        }
        let (record, record_length) = parse_record(&bytes[offset..], offset)?;
        records.push(record);
        offset = offset
            .checked_add(record_length)
            .ok_or(NtfsError::MalformedRecord {
                offset,
                reason: "record offset overflow",
            })?;
    }
    Ok(records)
}

/// Parse `FSCTL_READ_USN_JOURNAL` output: next USN followed by records.
pub fn parse_journal_buffer(bytes: &[u8]) -> Result<JournalBatch, NtfsError> {
    if bytes.len() < 8 {
        return Err(NtfsError::BufferTooSmall {
            actual: bytes.len(),
            minimum: 8,
        });
    }
    let next_usn = i64::from_le_bytes(
        bytes[..8]
            .try_into()
            .expect("journal header length checked"),
    );
    let records = parse_usn_records(&bytes[8..])?;
    Ok(JournalBatch { next_usn, records })
}

/// Parse `FSCTL_ENUM_USN_DATA` output: next start FRN followed by records.
pub fn parse_mft_buffer(bytes: &[u8]) -> Result<MftBatch, NtfsError> {
    if bytes.len() < 8 {
        return Err(NtfsError::BufferTooSmall {
            actual: bytes.len(),
            minimum: 8,
        });
    }
    let next_start_frn =
        u64::from_le_bytes(bytes[..8].try_into().expect("MFT header length checked"));
    let records = parse_usn_records(&bytes[8..])?;
    Ok(MftBatch {
        next_start_frn,
        records,
    })
}

fn parse_record(bytes: &[u8], offset: usize) -> Result<(UsnRecord, usize), NtfsError> {
    if bytes.len() < 4 {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "short record length field",
        });
    }
    let record_length = u32::from_le_bytes(bytes[..4].try_into().expect("checked length")) as usize;
    if record_length < USN_RECORD_V2_HEADER_LEN {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "record shorter than V2 header",
        });
    }
    if record_length % 8 != 0 {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "record length is not 8-byte aligned",
        });
    }
    if record_length > bytes.len() {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "record exceeds buffer",
        });
    }

    let major = u16::from_le_bytes(bytes[4..6].try_into().expect("header checked"));
    if major != 2 {
        return Err(NtfsError::UnsupportedRecordVersion { offset, major });
    }
    let frn = u64::from_le_bytes(bytes[8..16].try_into().expect("header checked"));
    if frn == 0 {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "FRN zero",
        });
    }
    let parent_frn = u64::from_le_bytes(bytes[16..24].try_into().expect("header checked"));
    let usn = i64::from_le_bytes(bytes[24..32].try_into().expect("header checked"));
    if usn < 0 {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "negative USN",
        });
    }
    let timestamp = i64::from_le_bytes(bytes[32..40].try_into().expect("header checked"));
    let reason = u32::from_le_bytes(bytes[40..44].try_into().expect("header checked"));
    let source_info = u32::from_le_bytes(bytes[44..48].try_into().expect("header checked"));
    let security_id = u32::from_le_bytes(bytes[48..52].try_into().expect("header checked"));
    let attributes = u32::from_le_bytes(bytes[52..56].try_into().expect("header checked"));
    let name_length =
        u16::from_le_bytes(bytes[56..58].try_into().expect("header checked")) as usize;
    let name_offset =
        u16::from_le_bytes(bytes[58..60].try_into().expect("header checked")) as usize;

    if name_length == 0 {
        return Err(NtfsError::InvalidName { offset });
    }
    if name_length % 2 != 0 {
        return Err(NtfsError::InvalidName { offset });
    }
    let units = name_length / 2;
    if units > MAX_FILE_NAME_UTF16 {
        return Err(NtfsError::InvalidName { offset });
    }
    if name_offset < USN_RECORD_V2_HEADER_LEN {
        return Err(NtfsError::InvalidName { offset });
    }
    let name_end = name_offset
        .checked_add(name_length)
        .ok_or(NtfsError::InvalidName { offset })?;
    if name_end > record_length {
        return Err(NtfsError::MalformedRecord {
            offset,
            reason: "file name exceeds record",
        });
    }

    let mut utf16 = Vec::with_capacity(units);
    for chunk in bytes[name_offset..name_end].chunks_exact(2) {
        utf16.push(u16::from_le_bytes(
            chunk.try_into().expect("chunks_exact yields 2 bytes"),
        ));
    }
    let name = String::from_utf16(&utf16).map_err(|_| NtfsError::InvalidName { offset })?;
    if name.is_empty() || name.contains('\0') {
        return Err(NtfsError::InvalidName { offset });
    }

    Ok((
        UsnRecord {
            frn,
            parent_frn,
            usn,
            timestamp,
            reason,
            source_info,
            security_id,
            attributes,
            name,
        },
        record_length,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_bytes(
        frn: u64,
        parent_frn: u64,
        usn: i64,
        reason: u32,
        attributes: u32,
        name: &str,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        build_record(frn, parent_frn, usn, reason, attributes, &units)
    }

    fn build_record(
        frn: u64,
        parent_frn: u64,
        usn: i64,
        reason: u32,
        attributes: u32,
        units: &[u16],
    ) -> Vec<u8> {
        let name_length = units.len() * 2;
        let raw_length = USN_RECORD_V2_HEADER_LEN + name_length;
        let record_length = (raw_length + 7) & !7;
        let mut bytes = vec![0u8; record_length];
        bytes[..4].copy_from_slice(&(record_length as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&frn.to_le_bytes());
        bytes[16..24].copy_from_slice(&parent_frn.to_le_bytes());
        bytes[24..32].copy_from_slice(&usn.to_le_bytes());
        bytes[32..40].copy_from_slice(&42i64.to_le_bytes());
        bytes[40..44].copy_from_slice(&reason.to_le_bytes());
        bytes[44..48].copy_from_slice(&7u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&9u32.to_le_bytes());
        bytes[52..56].copy_from_slice(&attributes.to_le_bytes());
        bytes[56..58].copy_from_slice(&(name_length as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&(USN_RECORD_V2_HEADER_LEN as u16).to_le_bytes());
        for (index, unit) in units.iter().enumerate() {
            let start = USN_RECORD_V2_HEADER_LEN + index * 2;
            bytes[start..start + 2].copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn concatenate(records: &[Vec<u8>]) -> Vec<u8> {
        records.iter().flatten().copied().collect()
    }

    #[test]
    fn parses_v2_record() {
        let bytes = record_bytes(10, 5, 123, USN_REASON_FILE_CREATE, 0, "报告.txt");
        let records = parse_usn_records(&bytes).unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.frn, 10);
        assert_eq!(record.parent_frn, 5);
        assert_eq!(record.usn, 123);
        assert_eq!(record.reason, USN_REASON_FILE_CREATE);
        assert_eq!(record.name, "报告.txt");
        assert!(!record.is_delete());
        assert!(!record.is_directory());
    }

    #[test]
    fn parses_multiple_records() {
        let bytes = concatenate(&[
            record_bytes(10, 5, 1, USN_REASON_FILE_CREATE, 0, "a.txt"),
            record_bytes(11, 5, 2, USN_REASON_FILE_DELETE, 0, "b.txt"),
            record_bytes(12, 5, 3, 0, FILE_ATTRIBUTE_DIRECTORY, "dir"),
        ]);
        let records = parse_usn_records(&bytes).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].name, "b.txt");
        assert!(records[1].is_delete());
        assert!(records[2].is_directory());
    }

    #[test]
    fn rejects_short_record_length_field() {
        assert!(matches!(
            parse_usn_records(&[1, 2, 3]),
            Err(NtfsError::MalformedRecord { .. })
        ));
    }

    #[test]
    fn rejects_short_record() {
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        bytes[..4].copy_from_slice(&32u32.to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::MalformedRecord { .. })
        ));
    }

    #[test]
    fn rejects_unaligned_record() {
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        let length = bytes.len() as u32;
        bytes[..4].copy_from_slice(&(length - 1).to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::MalformedRecord { .. })
        ));
    }

    #[test]
    fn rejects_record_exceeding_buffer() {
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        let length = bytes.len() as u32;
        bytes[..4].copy_from_slice(&(length + 8).to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::MalformedRecord { .. })
        ));
    }

    #[test]
    fn rejects_unknown_record_version() {
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::UnsupportedRecordVersion { major: 3, .. })
        ));
    }

    #[test]
    fn rejects_zero_frn_and_negative_usn() {
        let zero_frn = record_bytes(0, 5, 1, 0, 0, "a.txt");
        assert!(matches!(
            parse_usn_records(&zero_frn),
            Err(NtfsError::MalformedRecord { .. })
        ));
        let negative_usn = record_bytes(10, 5, -1, 0, 0, "a.txt");
        assert!(matches!(
            parse_usn_records(&negative_usn),
            Err(NtfsError::MalformedRecord { .. })
        ));
    }

    #[test]
    fn rejects_empty_name_and_odd_name_length() {
        let empty = build_record(10, 5, 1, 0, 0, &[]);
        assert!(matches!(
            parse_usn_records(&empty),
            Err(NtfsError::InvalidName { .. })
        ));
        let mut odd = record_bytes(10, 5, 1, 0, 0, "a.txt");
        odd[56..58].copy_from_slice(&3u16.to_le_bytes());
        assert!(matches!(
            parse_usn_records(&odd),
            Err(NtfsError::InvalidName { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf16_name() {
        let bytes = build_record(10, 5, 1, 0, 0, &[0xD800]);
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::InvalidName { .. })
        ));
    }

    #[test]
    fn rejects_name_outside_record() {
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        bytes[58..60].copy_from_slice(&100u16.to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::MalformedRecord { .. }) | Err(NtfsError::InvalidName { .. })
        ));
        let mut bytes = record_bytes(10, 5, 1, 0, 0, "a.txt");
        bytes[58..60].copy_from_slice(&10u16.to_le_bytes());
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::InvalidName { .. })
        ));
    }

    #[test]
    fn rejects_name_longer_than_ntfs_limit() {
        let units = vec![b'a' as u16; MAX_FILE_NAME_UTF16 + 1];
        let bytes = build_record(10, 5, 1, 0, 0, &units);
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::InvalidName { .. })
        ));
    }

    #[test]
    fn parses_journal_buffer() {
        let mut bytes = 500i64.to_le_bytes().to_vec();
        bytes.extend(record_bytes(10, 5, 400, 0, 0, "a.txt"));
        let batch = parse_journal_buffer(&bytes).unwrap();
        assert_eq!(batch.next_usn, 500);
        assert_eq!(batch.records.len(), 1);
    }

    #[test]
    fn parses_mft_buffer() {
        let mut bytes = 900u64.to_le_bytes().to_vec();
        bytes.extend(record_bytes(10, 5, 400, 0, 0, "a.txt"));
        let batch = parse_mft_buffer(&bytes).unwrap();
        assert_eq!(batch.next_start_frn, 900);
        assert_eq!(batch.records.len(), 1);
    }

    #[test]
    fn buffer_prefixes_are_required() {
        assert!(matches!(
            parse_journal_buffer(&[0; 7]),
            Err(NtfsError::BufferTooSmall { .. })
        ));
        assert!(matches!(
            parse_mft_buffer(&[0; 7]),
            Err(NtfsError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn journal_info_validation() {
        let valid = JournalInfo {
            journal_id: 1,
            first_usn: 10,
            next_usn: 20,
            lowest_valid_usn: 0,
            max_usn: 100,
            maximum_size: 1024,
            allocation_delta: 64,
        };
        valid.validate().unwrap();
        let mut invalid = valid.clone();
        invalid.journal_id = 0;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.next_usn = 9;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.lowest_valid_usn = 11;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.max_usn = 19;
        assert!(invalid.validate().is_err());
        let mut invalid = valid;
        invalid.maximum_size = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn converts_to_index_event() {
        let record = UsnRecord {
            frn: 10,
            parent_frn: 5,
            usn: 3,
            timestamp: 0,
            reason: USN_REASON_FILE_CREATE,
            source_info: 0,
            security_id: 0,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            name: "dir".to_owned(),
        };
        let event = record.to_index_event().unwrap().unwrap();
        assert!(matches!(
            event,
            IndexEvent::Upsert(ref value)
                if value.frn == 10 && value.is_dir && value.last_usn == 3
        ));
    }

    #[test]
    fn delete_takes_priority() {
        let record = UsnRecord {
            frn: 10,
            parent_frn: 5,
            usn: 9,
            timestamp: 0,
            reason: USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: "gone.txt".to_owned(),
        };
        assert_eq!(
            record.to_index_event().unwrap(),
            Some(IndexEvent::Delete { frn: 10, usn: 9 })
        );
    }

    #[test]
    fn rename_old_name_is_ignored() {
        let record = UsnRecord {
            frn: 10,
            parent_frn: 5,
            usn: 9,
            timestamp: 0,
            reason: USN_REASON_RENAME_OLD_NAME,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: "old.txt".to_owned(),
        };
        assert_eq!(record.to_index_event().unwrap(), None);
    }

    #[test]
    fn unsafe_names_are_rejected_for_indexing() {
        let record = UsnRecord {
            frn: 10,
            parent_frn: 5,
            usn: 1,
            timestamp: 0,
            reason: 0,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: "bad\\name".to_owned(),
        };
        assert!(record.to_file_record().is_err());
    }

    fn raw_record() -> UsnRecord {
        UsnRecord {
            frn: 10,
            parent_frn: 5,
            usn: 1,
            timestamp: 0,
            reason: USN_REASON_FILE_CREATE,
            source_info: 0,
            security_id: 0,
            attributes: 0,
            name: "ok.txt".to_owned(),
        }
    }

    #[test]
    fn conversion_rejects_each_invalid_field() {
        let mut record = raw_record();
        record.frn = 0;
        assert!(record.to_file_record().is_err());
        let mut record = raw_record();
        record.usn = -1;
        assert!(record.to_file_record().is_err());
        let mut record = raw_record();
        record.name.clear();
        assert!(record.to_file_record().is_err());
        let mut record = raw_record();
        record.name = "bad/name".to_owned();
        assert!(record.to_file_record().is_err());
        let mut record = raw_record();
        record.name = "x".repeat(MAX_FILE_NAME_UTF16 + 1);
        assert!(record.to_file_record().is_err());
        let mut record = raw_record();
        record.name = "bad\0name".to_owned();
        assert!(record.to_file_record().is_err());
    }

    #[test]
    fn delete_with_negative_usn_is_rejected() {
        let mut record = raw_record();
        record.usn = -1;
        record.reason = USN_REASON_FILE_DELETE;
        assert!(record.to_index_event().is_err());
    }

    #[test]
    fn parser_rejects_nul_in_name() {
        let bytes = build_record(10, 5, 1, 0, 0, &[b'a' as u16, 0, b'b' as u16]);
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::InvalidName { .. })
        ));
    }

    #[test]
    fn empty_record_stream_is_valid() {
        assert!(parse_usn_records(&[]).unwrap().is_empty());
    }

    #[test]
    fn every_error_maps_to_a_category() {
        let cases = [
            (
                NtfsError::Windows {
                    operation: "op",
                    code: 6,
                    message: "invalid handle".to_owned(),
                },
                pstart_core::ErrorCode::Internal,
            ),
            (
                NtfsError::InvalidRoot {
                    root: "C:\\x".to_owned(),
                },
                pstart_core::ErrorCode::InvalidRequest,
            ),
            (
                NtfsError::NotNtfs {
                    root: "D:\\".to_owned(),
                    filesystem: "FAT32".to_owned(),
                },
                pstart_core::ErrorCode::InvalidRequest,
            ),
            (
                NtfsError::UnsupportedPlatform,
                pstart_core::ErrorCode::Internal,
            ),
            (
                NtfsError::BufferTooSmall {
                    actual: 1,
                    minimum: 8,
                },
                pstart_core::ErrorCode::ResourceExhausted,
            ),
            (
                NtfsError::TooManyRecords { limit: 1 },
                pstart_core::ErrorCode::ResourceExhausted,
            ),
            (
                NtfsError::MalformedRecord {
                    offset: 0,
                    reason: "bad",
                },
                pstart_core::ErrorCode::InvalidFrame,
            ),
            (
                NtfsError::UnsupportedRecordVersion {
                    offset: 0,
                    major: 3,
                },
                pstart_core::ErrorCode::InvalidFrame,
            ),
            (
                NtfsError::InvalidName { offset: 0 },
                pstart_core::ErrorCode::InvalidRequest,
            ),
            (
                NtfsError::InvalidRecord("bad".to_owned()),
                pstart_core::ErrorCode::InvalidRequest,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.code(), expected, "{error}");
        }
    }

    #[test]
    fn too_many_records_are_rejected() {
        let record = record_bytes(1, 5, 1, 0, 0, "a");
        let mut bytes = Vec::with_capacity(record.len() * (MAX_RECORDS_PER_BUFFER + 1));
        for _ in 0..=MAX_RECORDS_PER_BUFFER {
            bytes.extend_from_slice(&record);
        }
        assert!(matches!(
            parse_usn_records(&bytes),
            Err(NtfsError::TooManyRecords { .. })
        ));
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            NtfsError::JournalNotActive {
                root: "C:\\".to_owned(),
                code: 1179
            }
            .code(),
            pstart_core::ErrorCode::JournalReset
        );
        assert_eq!(
            NtfsError::Windows {
                operation: "test",
                code: 5,
                message: "denied".to_owned()
            }
            .code(),
            pstart_core::ErrorCode::PermissionDenied
        );
    }
}
