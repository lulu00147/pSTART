//! Immutable file-backed trigram index for substring candidate filtering.
//!
//! The file stores a sorted trigram table and little-endian `u32` document
//! posting lists. Document ids are assigned by the caller; the search layer is
//! responsible for mapping them to SQLite rows and for final verification.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::IndexError;

const MAGIC: &[u8; 8] = b"PSTG0001";
const HEADER_LEN: usize = 12;
const ENTRY_LEN: usize = 16;

/// Maximum candidates returned by one trigram query.
pub const MAX_TRIGRAM_CANDIDATES: usize = 20_000;

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn trigrams(name: &[u8]) -> Vec<u32> {
    let mut result: Vec<u32> = name
        .windows(3)
        .map(|window| ((window[0] as u32) << 16) | ((window[1] as u32) << 8) | window[2] as u32)
        .collect();
    result.sort_unstable();
    result.dedup();
    result
}

struct Posting<'a> {
    bytes: &'a [u8],
}

impl<'a> Posting<'a> {
    fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    fn get(&self, position: usize) -> Option<u32> {
        u32_at(self.bytes, position.checked_mul(4)?)
    }

    fn binary_search(&self, doc: u32) -> bool {
        let mut low = 0usize;
        let mut high = self.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let value = self.get(middle).expect("middle is in bounds");
            match value.cmp(&doc) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }
}

/// Immutable shard set: one full snapshot plus zero or more delta shards.
pub struct TrigramSet {
    shards: Vec<TrigramIndex>,
}

impl std::fmt::Debug for TrigramSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrigramSet")
            .field("shards", &self.shards.len())
            .finish()
    }
}

impl TrigramSet {
    /// Open the full index and all delta shards in `<index>.d`.
    pub fn open(primary: &Path) -> Result<Self, IndexError> {
        let mut shards = Vec::new();
        if primary.exists() {
            shards.push(TrigramIndex::open(primary)?);
        }
        let delta_directory = delta_directory(primary);
        if delta_directory.is_dir() {
            let mut paths = Vec::new();
            for entry in std::fs::read_dir(&delta_directory)
                .map_err(|error| IndexError::InvalidData(format!("read trigram deltas: {error}")))?
                .flatten()
            {
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "bin") {
                    paths.push(path);
                }
            }
            paths.sort_unstable();
            for path in paths {
                shards.push(TrigramIndex::open(&path)?);
            }
        }
        Ok(Self { shards })
    }

    /// Number of shards including the full snapshot.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Append one already-written delta shard without remapping the full set.
    pub fn push(&mut self, path: &Path) -> Result<(), IndexError> {
        self.shards.push(TrigramIndex::open(path)?);
        Ok(())
    }

    /// Union candidates from all shards, bounded by `limit`.
    #[must_use]
    #[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
    pub fn candidates(&self, query_lower: &str, limit: usize) -> Vec<u32> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        'outer: for shard in &self.shards {
            for doc in shard.candidates(query_lower, limit) {
                if seen.insert(doc) {
                    if result.len() < limit {
                        result.push(doc);
                    }
                }
                if result.len() == limit {
                    break 'outer;
                }
            }
        }
        result
    }
}

/// Delta shard directory for a primary index path.
#[must_use]
pub fn delta_directory(primary: &Path) -> std::path::PathBuf {
    primary.with_extension("d")
}

/// Read-only trigram index backed by an in-memory file snapshot.
pub struct TrigramIndex {
    bytes: Vec<u8>,
    keys: usize,
}
impl std::fmt::Debug for TrigramIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrigramIndex")
            .field("bytes", &self.bytes.len())
            .field("keys", &self.keys)
            .finish()
    }
}

impl TrigramIndex {
    /// Open and validate an index file.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let mmap = std::fs::read(path)
            .map_err(|error| IndexError::InvalidData(format!("read trigram index: {error}")))?;
        if mmap.len() < HEADER_LEN {
            return Err(IndexError::InvalidData(
                "trigram index magic mismatch".to_owned(),
            ));
        }
        if &mmap[..MAGIC.len()] != MAGIC {
            return Err(IndexError::InvalidData(
                "trigram index magic mismatch".to_owned(),
            ));
        }
        let keys = u32_at(&mmap, 8)
            .ok_or_else(|| IndexError::InvalidData("trigram index header".to_owned()))?
            as usize;
        let data_start = HEADER_LEN
            .checked_add(
                keys.checked_mul(ENTRY_LEN)
                    .ok_or_else(|| IndexError::InvalidData("trigram table overflow".to_owned()))?,
            )
            .ok_or_else(|| IndexError::InvalidData("trigram table overflow".to_owned()))?;
        if data_start > mmap.len() {
            return Err(IndexError::InvalidData(
                "trigram table exceeds file".to_owned(),
            ));
        }
        let index = Self { bytes: mmap, keys };
        index.validate_postings()?;
        Ok(index)
    }

    fn entry(&self, position: usize) -> Option<(u32, usize, usize)> {
        let offset = HEADER_LEN.checked_add(position.checked_mul(ENTRY_LEN)?)?;
        let key = u32_at(&self.bytes, offset)?;
        let start = u32_at(&self.bytes, offset + 4)? as usize;
        let length = u32_at(&self.bytes, offset + 8)? as usize;
        let end = start.checked_add(length.checked_mul(4)?)?;
        if end > self.bytes.len() {
            return None;
        }
        Some((key, start, length))
    }

    fn validate_postings(&self) -> Result<(), IndexError> {
        for position in 0..self.keys {
            if self.entry(position).is_none() {
                return Err(IndexError::InvalidData(
                    "trigram posting exceeds file".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn posting(&self, trigram: u32) -> Option<Posting<'_>> {
        let mut low = 0usize;
        let mut high = self.keys;
        while low < high {
            let middle = low + (high - low) / 2;
            let (key, start, length) = self.entry(middle)?;
            match key.cmp(&trigram) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let end = start.checked_add(length.checked_mul(4)?)?;
                    return Some(Posting {
                        bytes: self.bytes.get(start..end)?,
                    });
                }
            }
        }
        None
    }

    /// Return up to `limit` document candidates whose lowercased name contains
    /// every trigram of `query_lower`.
    ///
    /// The caller must still verify the full name; this method only intersects
    /// posting lists.
    #[must_use]
    #[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
    pub fn candidates(&self, query_lower: &str, limit: usize) -> Vec<u32> {
        let limit = limit.min(MAX_TRIGRAM_CANDIDATES);
        if limit == 0 {
            return Vec::new();
        }
        let terms = trigrams(query_lower.as_bytes());
        if terms.is_empty() {
            return Vec::new();
        }
        let mut lists = Vec::with_capacity(terms.len());
        for term in terms {
            let Some(posting) = self.posting(term) else {
                return Vec::new();
            };
            lists.push(posting);
        }
        lists.sort_unstable_by_key(Posting::len);
        let mut result = Vec::with_capacity(limit.min(lists[0].len()));
        for position in 0..lists[0].len() {
            let doc = lists[0].get(position).expect("position is in bounds");
            if lists[1..].iter().all(|list| list.binary_search(doc)) {
                if result.len() < limit {
                    result.push(doc);
                }
            }
            if result.len() == limit {
                break;
            }
        }
        result
    }
}

/// Build a trigram index from `(document id, lowercased name)` pairs.
#[derive(Debug, Default)]
pub struct TrigramBuilder {
    postings: HashMap<u32, Vec<u32>>,
}

impl TrigramBuilder {
    /// Create an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one document. Names must already be lowercased consistently with
    /// query folding.
    pub fn add(&mut self, doc: u32, name_lower: &[u8]) {
        for trigram in trigrams(name_lower) {
            let list = self.postings.entry(trigram).or_default();
            if list.last().copied() != Some(doc) {
                list.push(doc);
            }
        }
    }

    /// Number of distinct trigrams.
    #[must_use]
    pub fn len(&self) -> usize {
        self.postings.len()
    }

    /// Whether the builder has no trigrams.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    /// Write an immutable index file and return its size.
    pub fn write(&self, path: &Path) -> Result<u64, IndexError> {
        let mut keys: Vec<u32> = self.postings.keys().copied().collect();
        keys.sort_unstable();
        let data_start = HEADER_LEN
            .checked_add(
                keys.len()
                    .checked_mul(ENTRY_LEN)
                    .ok_or_else(|| IndexError::InvalidData("trigram table overflow".to_owned()))?,
            )
            .ok_or_else(|| IndexError::InvalidData("trigram table overflow".to_owned()))?;
        let posting_bytes = self
            .postings
            .values()
            .try_fold(0usize, |total, list| {
                total.checked_add(list.len().checked_mul(4)?)
            })
            .ok_or_else(|| IndexError::InvalidData("trigram postings overflow".to_owned()))?;
        let total = data_start
            .checked_add(posting_bytes)
            .ok_or_else(|| IndexError::InvalidData("trigram file overflow".to_owned()))?;
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(keys.len() as u32).to_le_bytes());
        let mut offset = data_start;
        for key in &keys {
            let list = &self.postings[key];
            bytes.extend_from_slice(&key.to_le_bytes());
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
            bytes.extend_from_slice(&(list.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            offset += list.len() * 4;
        }
        for key in keys {
            for doc in &self.postings[&key] {
                bytes.extend_from_slice(&doc.to_le_bytes());
            }
        }
        let mut file = File::create(path)
            .map_err(|error| IndexError::InvalidData(format!("create trigram index: {error}")))?;
        file.write_all(&bytes)
            .map_err(|error| IndexError::InvalidData(format!("write trigram index: {error}")))?;
        Ok(bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn build(names: &[&str]) -> (TempDir, TrigramIndex) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("trigram.bin");
        let mut builder = TrigramBuilder::new();
        for (doc, name) in names.iter().enumerate() {
            builder.add(doc as u32, name.as_bytes());
        }
        builder.write(&path).unwrap();
        let index = TrigramIndex::open(&path).unwrap();
        (directory, index)
    }

    #[test]
    fn shard_set_unions_primary_and_delta() {
        let directory = TempDir::new().unwrap();
        let primary = directory.path().join("trigram.bin");
        let delta_path = delta_directory(&primary);
        std::fs::create_dir_all(&delta_path).unwrap();
        let mut full = TrigramBuilder::new();
        full.add(0, b"alpha");
        full.write(&primary).unwrap();
        let mut delta = TrigramBuilder::new();
        delta.add(1, b"alpine");
        delta.write(&delta_path.join("delta-0001.bin")).unwrap();
        let set = TrigramSet::open(&primary).unwrap();
        assert_eq!(set.shard_count(), 2);
        assert_eq!(set.candidates("alp", 10), vec![0, 1]);
        assert!(format!("{set:?}").contains("TrigramSet"));
    }

    #[test]
    fn shard_set_ignores_non_bin_delta_files() {
        let directory = TempDir::new().unwrap();
        let primary = directory.path().join("trigram.bin");
        let delta_path = delta_directory(&primary);
        std::fs::create_dir_all(&delta_path).unwrap();
        std::fs::write(delta_path.join("ignore.txt"), b"x").unwrap();
        let set = TrigramSet::open(&primary).unwrap();
        assert_eq!(set.shard_count(), 0);
    }

    #[test]
    fn shard_set_push_and_limit() {
        let directory = TempDir::new().unwrap();
        let primary = directory.path().join("trigram.bin");
        let delta_path = delta_directory(&primary);
        std::fs::create_dir_all(&delta_path).unwrap();
        let mut full = TrigramBuilder::new();
        full.add(0, b"alpha");
        full.write(&primary).unwrap();
        let mut delta = TrigramBuilder::new();
        delta.add(1, b"alpine");
        delta.write(&delta_path.join("delta-0001.bin")).unwrap();
        let set = TrigramSet::open(&primary).unwrap();
        assert_eq!(set.candidates("alp", 1), vec![0]);
        let shard = delta_path.join("delta-0002.bin");
        let mut extra = TrigramBuilder::new();
        extra.add(2, b"alps");
        extra.write(&shard).unwrap();
        let mut set = set;
        set.push(&shard).unwrap();
        assert_eq!(set.shard_count(), 3);
    }

    #[test]
    fn candidates_skip_nonintersecting_documents() {
        let (_directory, index) = build(&["abc", "zzz"]);
        assert!(index.candidates("abcd", 10).is_empty());
    }

    #[test]
    fn finds_intersecting_documents() {
        let (_directory, index) = build(&["notepad.exe", "notepad-notes.txt", "other.txt"]);
        let candidates = index.candidates("notepad", 10);
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&1));
        assert!(!candidates.contains(&2));
    }

    #[test]
    fn short_or_missing_queries_are_empty() {
        let (_directory, index) = build(&["alpha"]);
        assert!(index.candidates("al", 10).is_empty());
        assert!(index.candidates("zzz", 10).is_empty());
    }

    #[test]
    fn candidate_limit_is_applied() {
        let (_directory, index) = build(&["alpha", "alpine", "alps"]);
        assert_eq!(index.candidates("alp", 2).len(), 2);
        assert_eq!(index.candidates("alp", 0).len(), 0);
    }

    #[test]
    fn duplicate_trigrams_do_not_duplicate_documents() {
        let (_directory, index) = build(&["aaaa"]);
        assert_eq!(index.candidates("aaa", 10), vec![0]);
    }

    #[test]
    fn builder_reports_len_and_empty() {
        let mut builder = TrigramBuilder::new();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
        builder.add(0, b"alpha");
        assert!(!builder.is_empty());
        assert!(!builder.is_empty());
    }

    #[test]
    fn posting_binary_search_covers_all_orders() {
        let mut bytes = Vec::new();
        for value in [1u32, 3, 5] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let posting = Posting { bytes: &bytes };
        assert_eq!(posting.len(), 3);
        assert!(posting.binary_search(1));
        assert!(posting.binary_search(3));
        assert!(posting.binary_search(5));
        assert!(!posting.binary_search(0));
        assert!(!posting.binary_search(4));
        assert!(!posting.binary_search(9));
        assert!(posting.get(3).is_none());
    }

    #[test]
    fn debug_reports_index_shape() {
        let (_directory, index) = build(&["alpha"]);
        let text = format!("{index:?}");
        assert!(text.contains("TrigramIndex"));
        assert!(text.contains("keys"));
    }

    #[test]
    fn rejects_table_larger_than_file() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("bad-table.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&100u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(TrigramIndex::open(&path).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("bad.bin");
        std::fs::write(&path, b"not-an-index").unwrap();
        assert!(TrigramIndex::open(&path).is_err());
    }

    #[test]
    fn rejects_truncated_posting() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("truncated.bin");
        let mut builder = TrigramBuilder::new();
        builder.add(0, b"alpha");
        builder.write(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 1);
        std::fs::write(&path, bytes).unwrap();
        assert!(TrigramIndex::open(&path).is_err());
    }
}
