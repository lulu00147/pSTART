use std::time::{Duration, Instant};

use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex};

fn record(frn: u64, parent: u64, name: &str, usn: u64) -> FileRecord {
    FileRecord {
        frn,
        parent_frn: parent,
        name: name.to_owned(),
        attributes: 0,
        is_dir: false,
        last_usn: usn,
    }
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let index = (samples.len() * percentile / 100).min(samples.len() - 1);
    samples[index]
}

#[test]
#[ignore = "performance benchmark"]
fn fixed_dataset_query_benchmark() {
    const RECORDS: u64 = 200_000;
    const ROUNDS: usize = 50;
    let mut index = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 64).unwrap();

    let insert_start = Instant::now();
    for value in 0..RECORDS {
        assert!(index
            .upsert(record(
                value + 10,
                5,
                &format!("file{value:06}.txt"),
                value + 1,
            ))
            .unwrap());
    }
    let insert = insert_start.elapsed();

    let queries = ["file000", "file123", "file19", "123", "nomatch"];
    let mut samples = Vec::with_capacity(queries.len() * ROUNDS);
    for _ in 0..ROUNDS {
        for query in queries {
            let start = Instant::now();
            let outcome = index.search(query, 10);
            samples.push(start.elapsed());
            assert!(outcome.hits.len() <= 10);
        }
    }
    samples.sort_unstable();

    println!(
        "records={RECORDS} insert_ms={} samples={} p50_us={} p95_us={} max_us={}",
        insert.as_millis(),
        samples.len(),
        percentile(&samples, 50).as_micros(),
        percentile(&samples, 95).as_micros(),
        samples.last().unwrap().as_micros()
    );
}
