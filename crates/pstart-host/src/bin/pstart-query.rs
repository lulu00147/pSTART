//! Headless AF_UNIX query client.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;

    use pstart_host::Client;

    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("pstart-query {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let mut socket = None;
    let mut query = None;
    let mut limit = 10_u32;
    let mut burst = None;
    let mut bench = None;
    let mut status = false;
    let mut hello_version = None;
    let mut database = None;
    let mut ids = None;
    let mut ids_detail = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" => socket = arguments.next().map(PathBuf::from),
            "--query" => query = arguments.next(),
            "--limit" => {
                limit = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(10);
            }
            "--burst" => {
                burst = arguments.next().and_then(|value| value.parse().ok());
            }
            "--bench" => {
                bench = arguments.next().and_then(|value| value.parse().ok());
            }
            "--status" => status = true,
            "--hello-version" => {
                hello_version = arguments.next().and_then(|value| value.parse().ok());
            }
            "--db" => database = arguments.next().map(PathBuf::from),
            "--ids" => ids = arguments.next().map(PathBuf::from),
            "--ids-detail" => ids_detail = arguments.next().map(PathBuf::from),
            _ => {
                eprintln!("unknown argument: {argument}");
                std::process::exit(2);
            }
        }
    }
    if let Some(path) = database {
        use rusqlite::{Connection, OpenFlags};
        let connection = match Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("open failed: {error}");
                std::process::exit(1);
            }
        };
        let files: i64 = connection
            .query_row("SELECT COUNT(*) FROM file_metadata", [], |row| row.get(0))
            .unwrap_or(-1);
        let dirs: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(is_dir), 0) FROM file_metadata",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let regular: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(1 - is_dir), 0) FROM file_metadata",
                [],
                |row| row.get(0),
            )
            .unwrap_or(-1);
        let volumes: i64 = connection
            .query_row("SELECT COUNT(*) FROM volume_state", [], |row| row.get(0))
            .unwrap_or(-1);
        let checkpoint: i64 = connection
            .query_row("SELECT next_usn FROM index_checkpoint", [], |row| {
                row.get(0)
            })
            .unwrap_or(-1);
        let journal: i64 = connection
            .query_row("SELECT journal_id FROM index_checkpoint", [], |row| {
                row.get(0)
            })
            .unwrap_or(-1);
        let generation: i64 = connection
            .query_row("SELECT index_generation FROM index_checkpoint", [], |row| {
                row.get(0)
            })
            .unwrap_or(-1);
        if files < 0 || dirs < 0 || regular < 0 || volumes < 0 {
            eprintln!("database does not contain the pSTART index schema");
            std::process::exit(1);
        }
        println!(
            "files={files} dirs={dirs} regular={regular} volumes={volumes} \
             checkpoint={checkpoint} journal={journal} generation={generation}"
        );
        if let Some(path) = ids {
            use std::io::Write as _;
            let mut output = match std::fs::File::create(&path) {
                Ok(file) => std::io::BufWriter::new(file),
                Err(error) => {
                    eprintln!("ids output failed: {error}");
                    std::process::exit(1);
                }
            };
            let mut statement =
                match connection.prepare("SELECT frn FROM file_metadata WHERE is_dir = 0") {
                    Ok(statement) => statement,
                    Err(error) => {
                        eprintln!("ids query failed: {error}");
                        std::process::exit(1);
                    }
                };
            let rows = match statement.query_map([], |row| row.get::<_, i64>(0)) {
                Ok(rows) => rows,
                Err(error) => {
                    eprintln!("ids query failed: {error}");
                    std::process::exit(1);
                }
            };
            let mut count = 0_u64;
            for row in rows.flatten() {
                let _ = writeln!(output, "{row}");
                count += 1;
            }
            println!("ids={count} path={}", path.display());
        }
        if let Some(path) = ids_detail {
            use std::io::Write as _;
            let mut output = match std::fs::File::create(&path) {
                Ok(file) => std::io::BufWriter::new(file),
                Err(error) => {
                    eprintln!("detail output failed: {error}");
                    std::process::exit(1);
                }
            };
            let mut statement = match connection.prepare(
                "SELECT frn, COALESCE(parent_frn, 0), is_dir, attributes, \
                 COALESCE(last_usn, 0), name FROM file_metadata",
            ) {
                Ok(statement) => statement,
                Err(error) => {
                    eprintln!("detail query failed: {error}");
                    std::process::exit(1);
                }
            };
            let rows = match statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            }) {
                Ok(rows) => rows,
                Err(error) => {
                    eprintln!("detail query failed: {error}");
                    std::process::exit(1);
                }
            };
            let mut count = 0_u64;
            for row in rows.flatten() {
                let _ = writeln!(
                    output,
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    row.0, row.1, row.2, row.3, row.4, row.5
                );
                count += 1;
            }
            println!("detail={count} path={}", path.display());
        }
        return;
    }
    let Some(socket) = socket else {
        eprintln!(
            "usage: pstart-query --socket <path> (--query <text> | --burst <n>) [--limit <n>]"
        );
        std::process::exit(2);
    };
    let connection = match hello_version {
        Some(version) => Client::connect_with_protocol_version(&socket, version),
        None => Client::connect(&socket),
    };
    let Ok(mut client) = connection else {
        eprintln!("connect failed");
        std::process::exit(1);
    };
    if status {
        match client.status() {
            Ok(status) => println!(
                "volume={} files={} syncing={}",
                status.volume_id, status.indexed_files, status.is_syncing
            ),
            Err(error) => {
                eprintln!("status failed: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(total) = bench {
        use std::time::Instant;
        let text = query.as_deref().unwrap_or("notepad");
        let mut samples = Vec::with_capacity(total as usize);
        let mut errors = 0_u32;
        for _ in 0..total {
            let start = Instant::now();
            match client.query(text, 10) {
                Ok(_) => samples.push(start.elapsed().as_micros()),
                Err(_) => errors += 1,
            }
        }
        samples.sort_unstable();
        println!(
            "bench samples={} errors={} p50_us={} p95_us={} max_us={}",
            samples.len(),
            errors,
            percentile(&samples, 50),
            percentile(&samples, 95),
            samples.last().copied().unwrap_or(0)
        );
        if errors > 0 || samples.is_empty() {
            std::process::exit(1);
        }
        return;
    }
    if let Some(total) = burst {
        let mut ok = 0_u32;
        let mut errors = 0_u32;
        let mut first_error = None;
        for _ in 0..total {
            match client.query("notepad", 1) {
                Ok(_) => ok += 1,
                Err(error) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(error.to_string());
                    }
                }
            }
        }
        println!(
            "burst={total} ok={ok} errors={errors} first_error={}",
            first_error.unwrap_or_else(|| "none".to_owned())
        );
        if errors != 0 {
            std::process::exit(3);
        }
        return;
    }
    let Some(query) = query else {
        eprintln!("missing --query");
        std::process::exit(2);
    };
    match client.query(&query, limit) {
        Ok(response) => {
            for item in &response.items {
                println!("{}\t{}", item.name, item.path);
            }
            println!(
                "count={} truncated={} session={}",
                response.items.len(),
                response.truncated,
                client.session_id()
            );
        }
        Err(error) => {
            eprintln!("query failed: {error}");
            std::process::exit(1);
        }
    }
}

fn percentile(samples: &[u128], percent: usize) -> u128 {
    let Some(last) = samples.len().checked_sub(1) else {
        return 0;
    };
    samples.get(last * percent / 100).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_handles_empty_and_boundaries() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 3);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("pstart-query is only available on Windows");
    std::process::exit(2);
}
