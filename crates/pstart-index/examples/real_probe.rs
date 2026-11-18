//! Local real-filesystem probe for MemoryIndex.
//!
//! Usage: cargo run --release -p pstart-index --example real_probe -- <root>

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex};

fn p50(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn walk(root: &Path) -> Vec<(PathBuf, PathBuf, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path.clone());
            }
            out.push((path, directory.clone(), file_type.is_dir()));
        }
    }
    out
}

fn main() {
    let root = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let start = Instant::now();
    let entries = walk(&root);
    println!(
        "walk_ms={} files={}",
        start.elapsed().as_millis(),
        entries.len()
    );

    let mut ids = std::collections::HashMap::new();
    ids.insert(root.clone(), Frn::ROOT.get());
    let mut next = 10u64;
    for (path, parent, _) in &entries {
        if !ids.contains_key(path) {
            ids.insert(path.clone(), next);
            next += 1;
        }
        let _ = parent;
    }

    let volume = VolumeId::new("C:").unwrap();
    let mut index = MemoryIndex::new(volume, "C:\\", Frn::ROOT, 256).unwrap();
    let start = Instant::now();
    let mut inserted = 0u64;
    for (path, parent, is_dir) in &entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let frn = *ids.get(path).unwrap();
        let parent_frn = *ids.get(parent).unwrap_or(&Frn::ROOT.get());
        if index
            .upsert(FileRecord {
                frn,
                parent_frn,
                name: name.to_owned(),
                attributes: 0,
                is_dir: *is_dir,
                last_usn: frn,
            })
            .is_ok()
        {
            inserted += 1;
        }
    }
    println!(
        "build_ms={} inserted={}",
        start.elapsed().as_millis(),
        inserted
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

    if let Some((path, _, _)) = entries.iter().find(|(_, _, is_dir)| !*is_dir) {
        let frn = *ids.get(path).unwrap();
        let mut samples = Vec::with_capacity(101);
        for _ in 0..101 {
            let start = Instant::now();
            let _ = index.resolve_record_path(Frn::new(frn));
            samples.push(start.elapsed().as_micros().max(1));
        }
        println!("path_us={}", p50(samples));
    }
}
