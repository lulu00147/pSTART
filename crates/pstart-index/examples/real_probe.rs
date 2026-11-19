//! Local real-filesystem probe for MemoryIndex.
//!
//! Usage: cargo run --release -p pstart-index --example real_probe -- <root>

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex};

fn p50(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn main() {
    let root = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let volume = VolumeId::new("C:").unwrap();
    let mut index = MemoryIndex::new(volume, "C:\\", Frn::ROOT, 256).unwrap();

    let start = Instant::now();
    let mut index_time = std::time::Duration::ZERO;
    let mut inserted = 0u64;
    let mut sample_frn = None;
    let mut next = 10u64;
    let mut stack = vec![(root, Frn::ROOT.get())];
    while let Some((directory, parent_frn)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let frn = next;
            next += 1;
            let is_dir = file_type.is_dir();
            if is_dir {
                stack.push((entry.path(), frn));
            } else if sample_frn.is_none() {
                sample_frn = Some(frn);
            }
            let upsert_start = Instant::now();
            let changed = index
                .upsert(FileRecord {
                    frn,
                    parent_frn,
                    name: name.into(),
                    attributes: if is_dir { 0x10 } else { 0 },
                    is_dir,
                    last_usn: frn,
                })
                .is_ok();
            index_time += upsert_start.elapsed();
            if changed {
                inserted += 1;
            }
        }
    }
    println!(
        "total_ms={} index_ms={} walk_ms={} inserted={}",
        start.elapsed().as_millis(),
        index_time.as_millis(),
        start.elapsed().saturating_sub(index_time).as_millis(),
        inserted
    );
    index.compact_names();
    println!(
        "index_mb={:.1}",
        index.memory_report().total() as f64 / 1_048_576.0
    );

    for query in ["dll", "config", "system32", "setup"] {
        let mut samples = Vec::with_capacity(101);
        let mut hits = 0;
        for _ in 0..101 {
            let start = Instant::now();
            hits = index.search(query, 10).hits.len();
            samples.push(start.elapsed().as_micros().max(1));
        }
        println!("search_{query}_us={} hits={hits}", p50(samples));
    }

    if let Some(frn) = sample_frn {
        let mut samples = Vec::with_capacity(101);
        for _ in 0..101 {
            let start = Instant::now();
            let _ = index.resolve_record_path(Frn::new(frn));
            samples.push(start.elapsed().as_micros().max(1));
        }
        println!("path_us={}", p50(samples));
    }
}
