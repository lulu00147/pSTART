//! Synthetic performance probe for MemoryIndex.
//!
//! Usage: cargo run --release -p pstart-index --example perf_probe

use std::env;
use std::time::Instant;

use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex};

const DEFAULT_RECORDS: u64 = 1_000_000;
const DEPTH: u64 = 200;

fn record(frn: u64, parent: u64, name: String, is_dir: bool) -> FileRecord {
    FileRecord {
        frn,
        parent_frn: parent,
        name,
        attributes: 0,
        is_dir,
        last_usn: frn,
    }
}

fn p50(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let records: u64 = env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_RECORDS);
    let volume = VolumeId::new("C:").unwrap();
    let mut index = MemoryIndex::new(volume, "C:\\", Frn::ROOT, 256).unwrap();

    let start = Instant::now();
    for value in 0..records {
        let name = format!("file{value:07}.dat");
        index
            .upsert(record(value + 10, Frn::ROOT.get(), name, false))
            .expect("upsert");
    }
    println!("build_ms={}", start.elapsed().as_millis());

    let mut samples = Vec::with_capacity(101);
    for _ in 0..101 {
        let start = Instant::now();
        let hits = index.search("file", 10).hits.len();
        samples.push(start.elapsed().as_micros().max(1));
        assert!(hits > 0);
    }
    println!("prefix_us={}", p50(samples));

    let mut samples = Vec::with_capacity(101);
    for _ in 0..101 {
        let start = Instant::now();
        let hits = index.search("9999", 10).hits.len();
        samples.push(start.elapsed().as_micros().max(1));
        assert!(hits > 0);
    }
    println!("substring_us={}", p50(samples));

    let mut samples = Vec::with_capacity(101);
    let leaf = records + 9;
    for _ in 0..101 {
        let start = Instant::now();
        let path = index.resolve_record_path(Frn::new(leaf)).unwrap();
        samples.push(start.elapsed().as_micros().max(1));
        assert!(path.starts_with("C:\\"));
    }
    println!("leaf_path_us={}", p50(samples));

    let deep_root = records + 1000;
    for value in 0..DEPTH {
        let parent = if value == 0 {
            Frn::ROOT.get()
        } else {
            deep_root + value - 1
        };
        index
            .upsert(record(
                deep_root + value,
                parent,
                format!("d{value:03}"),
                true,
            ))
            .expect("upsert deep");
    }
    let deep_leaf = deep_root + DEPTH - 1;
    let mut samples = Vec::with_capacity(101);
    for _ in 0..101 {
        let start = Instant::now();
        let path = index.resolve_record_path(Frn::new(deep_leaf)).unwrap();
        samples.push(start.elapsed().as_micros().max(1));
        assert!(path.len() > DEPTH as usize);
    }
    println!("deep_path_us={}", p50(samples));
}
