//! Same-fixture A/B: MemoryIndex vs SQLite + trigram backend.
//!
//! Usage:
//!   cargo run --release -p pstart-index --example search_ab_probe -- <index.db>

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex, SqliteSearch, TrigramBuilder};
use rusqlite::Connection;

fn percentile(samples: &mut [u128], percentile: usize) -> u128 {
    samples.sort_unstable();
    samples[samples.len() * percentile / 100]
}

fn main() {
    let database = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("pstart-fts-full0.db"));
    let trigram_path = env::temp_dir().join("pstart-ab-trigram.bin");
    let connection = Connection::open(&database).expect("fixture");
    let mut statement = connection
        .prepare("SELECT rowid, frn, parent_frn, name, attributes, is_dir FROM file_metadata")
        .expect("prepare");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                FileRecord {
                    frn: u64::try_from(row.get::<_, i64>(1)?).unwrap(),
                    parent_frn: row
                        .get::<_, Option<i64>>(2)?
                        .and_then(|value| u64::try_from(value).ok())
                        .unwrap_or(0),
                    name: row.get(3)?,
                    attributes: u32::try_from(row.get::<_, i64>(4)?).unwrap(),
                    is_dir: row.get::<_, i64>(5)? != 0,
                    last_usn: 0,
                },
            ))
        })
        .expect("rows");

    let volume = VolumeId::new("C:").unwrap();
    let mut memory = MemoryIndex::new(volume.clone(), "C:\\", Frn::new(5), 256).unwrap();
    let mut builder = TrigramBuilder::new();
    let start = Instant::now();
    for row in rows {
        let (rowid, record) = row.expect("record");
        builder.add(rowid as u32, record.name.to_lowercase().as_bytes());
        memory.upsert(record).expect("memory upsert");
    }
    println!("memory_build_ms={}", start.elapsed().as_millis());
    builder.write(&trigram_path).expect("trigram write");
    drop(statement);
    drop(connection);
    let sqlite = SqliteSearch::open(&database, &trigram_path, volume).expect("sqlite search");

    let measure = |search: &mut dyn FnMut(&str) -> usize, term: &str| {
        let mut samples = Vec::with_capacity(100);
        let mut hits = 0;
        for _ in 0..100 {
            let start = Instant::now();
            hits = search(term);
            samples.push(start.elapsed().as_micros().max(1));
        }
        (
            percentile(&mut samples, 50),
            percentile(&mut samples, 95),
            hits,
        )
    };

    let mut memory_search = |term: &str| memory.search(term, 10).hits.len();
    let mut sqlite_search = |term: &str| sqlite.search(term, 10).expect("sqlite search").hits.len();

    for term in ["notepad", "a", "pad-000", "no", "zzzz-no-match"] {
        let (memory_p50, memory_p95, memory_hits) = measure(&mut memory_search, term);
        let (sqlite_p50, sqlite_p95, sqlite_hits) = measure(&mut sqlite_search, term);
        println!(
            "term={term:?} memory={memory_p50}/{memory_p95}us hits={memory_hits} \
             sqlite={sqlite_p50}/{sqlite_p95}us hits={sqlite_hits}"
        );
    }
}
