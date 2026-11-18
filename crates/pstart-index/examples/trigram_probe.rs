//! Disk trigram + mmap probe for the 1.2M-name SQLite fixture.
//!
//! Usage:
//!   cargo run --release -p pstart-index --example trigram_probe -- <index.db>

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use memmap2::Mmap;
use rusqlite::Connection;

const LIMIT: usize = 10;
const HEADER_SIZE: usize = 4;
const ENTRY_SIZE: usize = 16;

struct Names {
    bytes: Vec<u8>,
    offsets: Vec<u32>,
    lengths: Vec<u16>,
}

impl Names {
    fn get(&self, doc: u32) -> &[u8] {
        let start = self.offsets[doc as usize] as usize;
        let end = start + self.lengths[doc as usize] as usize;
        &self.bytes[start..end]
    }
}

fn trigrams(name: &[u8]) -> Vec<u32> {
    if name.len() < 3 {
        return Vec::new();
    }
    name.windows(3)
        .map(|window| ((window[0] as u32) << 16) | ((window[1] as u32) << 8) | window[2] as u32)
        .collect()
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32"))
}

struct DiskIndex {
    mmap: Mmap,
    keys: usize,
}

impl DiskIndex {
    fn open(path: &PathBuf) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let keys = u32_at(&mmap, 0) as usize;
        Ok(Self { mmap, keys })
    }

    fn entry(&self, position: usize) -> (u32, usize, usize) {
        let offset = HEADER_SIZE + position * ENTRY_SIZE;
        let key = u32_at(&self.mmap, offset);
        let start = u32_at(&self.mmap, offset + 4) as usize;
        let length = u32_at(&self.mmap, offset + 12) as usize;
        (key, start, length)
    }

    fn posting(&self, term: u32) -> Option<&[u32]> {
        let mut low = 0usize;
        let mut high = self.keys;
        while low < high {
            let middle = low + (high - low) / 2;
            let (key, start, length) = self.entry(middle);
            if key < term {
                low = middle + 1;
            } else if key > term {
                high = middle;
            } else {
                let bytes = &self.mmap[start..start + length * 4];
                // SAFETY: entries are 4-byte aligned and little-endian on the
                // supported Windows target; the slice is bounds checked above.
                let pointer = bytes.as_ptr().cast::<u32>();
                return Some(unsafe { std::slice::from_raw_parts(pointer, length) });
            }
        }
        None
    }
}

fn build_index(names: &Names) -> HashMap<u32, Vec<u32>> {
    let mut postings: HashMap<u32, Vec<u32>> = HashMap::new();
    for doc in 0..names.offsets.len() as u32 {
        for trigram in trigrams(names.get(doc)) {
            postings.entry(trigram).or_default().push(doc);
        }
    }
    postings
}

fn write_index(path: &PathBuf, postings: &HashMap<u32, Vec<u32>>) -> std::io::Result<u64> {
    let mut keys: Vec<u32> = postings.keys().copied().collect();
    keys.sort_unstable();
    let data_start = HEADER_SIZE + keys.len() * ENTRY_SIZE;
    let total = data_start + postings.values().map(|list| list.len() * 4).sum::<usize>();
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    let mut offset = data_start;
    for key in &keys {
        let length = postings[key].len();
        bytes.extend_from_slice(&key.to_le_bytes());
        bytes.extend_from_slice(&(offset as u64).to_le_bytes());
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        offset += length * 4;
    }
    for key in keys {
        for doc in &postings[&key] {
            bytes.extend_from_slice(&doc.to_le_bytes());
        }
    }
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    Ok(file.metadata()?.len())
}

fn search(names: &Names, index: &DiskIndex, needle: &[u8]) -> usize {
    let terms = trigrams(needle);
    if terms.is_empty() {
        return 0;
    }
    let mut lists = Vec::with_capacity(terms.len());
    for term in terms {
        let Some(list) = index.posting(term) else {
            return 0;
        };
        lists.push(list);
    }
    lists.sort_unstable_by_key(|list| list.len());
    let mut found = 0;
    'candidates: for doc in lists[0] {
        for list in &lists[1..] {
            if list.binary_search(doc).is_err() {
                continue 'candidates;
            }
        }
        let name = names.get(*doc);
        if name.windows(needle.len()).any(|window| window == needle) {
            found += 1;
            if found == LIMIT {
                break;
            }
        }
    }
    found
}

fn percentile(samples: &mut [u128], percentile: usize) -> u128 {
    samples.sort_unstable();
    samples[samples.len() * percentile / 100]
}

fn main() {
    let database = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("pstart-fts-full0.db"));
    let connection = Connection::open(&database).expect("open fixture");
    let mut statement = connection
        .prepare("SELECT name FROM file_metadata ORDER BY frn")
        .expect("prepare names");
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    let mut lengths = Vec::new();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query names");
    for row in rows {
        let name = row.expect("name row");
        offsets.push(bytes.len() as u32);
        lengths.push(name.len() as u16);
        bytes.extend_from_slice(name.as_bytes());
    }
    let names = Names {
        bytes,
        offsets,
        lengths,
    };
    println!("records={}", names.offsets.len());

    let build_start = Instant::now();
    let postings = build_index(&names);
    println!("build_ms={}", build_start.elapsed().as_millis());
    println!("trigrams={}", postings.len());

    let path = env::temp_dir().join("pstart-trigram-index.bin");
    let write_start = Instant::now();
    let bytes = write_index(&path, &postings).expect("write index");
    println!("write_ms={}", write_start.elapsed().as_millis());
    println!("index_bytes={bytes}");
    drop(postings);

    let open_start = Instant::now();
    let index = DiskIndex::open(&path).expect("open mmap");
    println!("open_ms={}", open_start.elapsed().as_millis());

    let measure = |needle: &[u8]| {
        let mut samples = Vec::with_capacity(100);
        let mut matches = 0;
        for _ in 0..100 {
            let start = Instant::now();
            matches = search(&names, &index, needle);
            samples.push(start.elapsed().as_nanos().max(1));
        }
        (
            percentile(&mut samples, 50),
            percentile(&mut samples, 95),
            matches,
        )
    };

    let (notepad_p50, notepad_p95, notepad_matches) = measure(b"notepad");
    let (mid_p50, mid_p95, mid_matches) = measure(b"pad-000");
    let (none_p50, none_p95, none_matches) = measure(b"zzzz-no-match");
    let (short_p50, short_p95, short_matches) = measure(b"no");

    println!("notepad_ns={notepad_p50}/{notepad_p95} matches={notepad_matches}");
    println!("pad_ns={mid_p50}/{mid_p95} matches={mid_matches}");
    println!("none_ns={none_p50}/{none_p95} matches={none_matches}");
    println!("short_ns={short_p50}/{short_p95} matches={short_matches}");
}
