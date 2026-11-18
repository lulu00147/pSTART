//! SQLite search backend probe over the 1.2M-name fixture.
//!
//! Usage:
//!   cargo run --release -p pstart-index --example sqlite_search_probe -- <index.db>

use std::env;
use std::path::PathBuf;
use std::time::Instant;

use pstart_core::VolumeId;
use pstart_index::trigram::TrigramBuilder;
use pstart_index::SqliteSearch;
use rusqlite::Connection;

fn main() {
    let database = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("pstart-fts-full0.db"));
    let trigram_path = env::temp_dir().join("pstart-prod-trigram.bin");

    let connection = Connection::open(&database).expect("open fixture");
    let mut statement = connection
        .prepare("SELECT rowid, name FROM file_metadata ORDER BY rowid")
        .expect("prepare");
    let mut builder = TrigramBuilder::new();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query");
    for row in rows {
        let (rowid, name) = row.expect("row");
        builder.add(rowid as u32, name.to_lowercase().as_bytes());
    }
    drop(statement);
    drop(connection);

    let write_start = Instant::now();
    let bytes = builder.write(&trigram_path).expect("write trigram");
    println!("trigram_bytes={bytes}");
    println!("trigram_write_ms={}", write_start.elapsed().as_millis());

    let volume = VolumeId::new("C:").unwrap();
    let search = SqliteSearch::open(&database, &trigram_path, volume).expect("search backend");

    let measure = |term: &str| {
        let mut samples = Vec::with_capacity(100);
        let mut hits = 0;
        for _ in 0..100 {
            let start = Instant::now();
            let outcome = search.search(term, 10).expect("search");
            hits = outcome.hits.len();
            samples.push(start.elapsed().as_micros().max(1));
        }
        samples.sort_unstable();
        (
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100],
            hits,
        )
    };

    for term in ["notepad", "pad-000", "a", "zzzz-no-match", "no"] {
        let (p50, p95, hits) = measure(term);
        println!("term={term:?} p50={p50}us p95={p95}us hits={hits}");
    }
}
