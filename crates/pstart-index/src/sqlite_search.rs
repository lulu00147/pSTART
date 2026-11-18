//! SQLite-backed search: NOCASE prefix ranges, trigram substring candidates,
//! recursive parent-chain paths and the existing explainable score.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use pstart_core::{FileId, FileItem, Frn, VolumeId};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OpenFlags};

use crate::error::IndexError;
use crate::memory::{SearchHit, SearchOutcome};
use crate::score::{score_name, SCORE_DEPTH_PENALTY_MAX, SCORE_DIRECTORY_PENALTY};
use crate::trigram::TrigramSet;

const CANDIDATE_CAP: usize = 20_000;

#[derive(Debug)]
struct Row {
    rowid: i64,
    frn: u64,
    name: String,
    attributes: u32,
    is_dir: bool,
}

/// Read-only SQLite + trigram search backend.
pub struct SqliteSearch {
    connection: Connection,
    trigram: TrigramSet,
    volume: VolumeId,
}

impl std::fmt::Debug for SqliteSearch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteSearch")
            .field("volume", &self.volume)
            .finish_non_exhaustive()
    }
}

impl SqliteSearch {
    /// Open an existing database and immutable trigram index.
    pub fn open(database: &Path, trigram: &Path, volume: VolumeId) -> Result<Self, IndexError> {
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "query_only", true)?;
        connection.prepare("SELECT rowid FROM file_metadata LIMIT 0")?;
        Ok(Self {
            connection,
            trigram: TrigramSet::open(trigram)?,
            volume,
        })
    }

    /// Append one delta shard to this backend.
    pub fn add_shard(&mut self, path: &Path) -> Result<(), IndexError> {
        self.trigram.push(path)
    }

    /// Search names, preserving prefix-first semantics and bounded substring
    /// fallback.
    pub fn search(&self, query: &str, limit: u32) -> Result<SearchOutcome, IndexError> {
        let query = query.trim();
        let limit = limit as usize;
        if query.is_empty() || limit == 0 {
            return Ok(SearchOutcome::default());
        }
        let query_lower = query.to_lowercase();
        let mut rows = self.prefix_rows(&query_lower, limit)?;
        let mut truncated = rows.len() >= CANDIDATE_CAP;
        truncated |= self.append_fallback(&mut rows, &query_lower, limit)?;

        let mut preliminary: Vec<(i32, Row)> = rows
            .into_iter()
            .filter_map(|row| {
                let name_score = score_name(&row.name, &query_lower)?;
                Some((name_score, row))
            })
            .collect();
        preliminary.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.name.cmp(&right.1.name))
                .then_with(|| left.1.frn.cmp(&right.1.frn))
        });
        preliminary.truncate(limit);

        let mut hits = Vec::with_capacity(limit);
        let frns: Vec<u64> = preliminary.iter().map(|(_, row)| row.frn).collect();
        let paths = self.paths(&frns)?;
        for (name_score, row) in preliminary {
            let (path, depth) = paths.get(&row.frn).cloned().unwrap_or_default();
            let directory = if row.is_dir {
                SCORE_DIRECTORY_PENALTY
            } else {
                0
            };
            let score =
                (name_score - depth.min(SCORE_DEPTH_PENALTY_MAX as usize) as i32 - directory)
                    .max(1);
            hits.push(SearchHit {
                item: FileItem {
                    id: FileId::new(self.volume.clone(), Frn::new(row.frn)),
                    name: row.name,
                    path: path.clone(),
                    attributes: row.attributes,
                    is_dir: row.is_dir,
                },
                score,
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.item.name.cmp(&right.item.name))
                .then_with(|| left.item.path.cmp(&right.item.path))
        });
        hits.truncate(limit);
        Ok(SearchOutcome { hits, truncated })
    }

    fn append_fallback(
        &self,
        rows: &mut Vec<Row>,
        query_lower: &str,
        limit: usize,
    ) -> Result<bool, IndexError> {
        if rows.len() >= limit {
            return Ok(false);
        }
        let (extra, capped) = if query_lower.chars().count() >= 3 {
            self.trigram_rows(query_lower, limit)?
        } else {
            self.like_rows(query_lower, limit)?
        };
        let mut seen: HashSet<i64> = rows.iter().map(|row| row.rowid).collect();
        for row in extra {
            if seen.insert(row.rowid) {
                rows.push(row);
            }
        }
        Ok(capped)
    }

    fn prefix_rows(&self, query_lower: &str, limit: usize) -> Result<Vec<Row>, IndexError> {
        let (lower, upper) = prefix_bounds(query_lower);
        let cap = (limit.saturating_mul(4)).min(CANDIDATE_CAP) as i64;
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid, frn, name, parent_frn, attributes, is_dir
             FROM file_metadata
             WHERE volume_id = ?1
               AND name >= ?2 COLLATE NOCASE
               AND name < ?3 COLLATE NOCASE
             LIMIT ?4",
            )
            .expect("schema validated at open");
        let rows = statement
            .query_map(
                params![self.volume.as_str(), lower, upper, cap],
                row_from_sql,
            )
            .expect("schema validated at open");
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(IndexError::from)
    }

    fn trigram_rows(
        &self,
        query_lower: &str,
        limit: usize,
    ) -> Result<(Vec<Row>, bool), IndexError> {
        let candidates = self
            .trigram
            .candidates(query_lower, (limit.saturating_mul(4)).min(CANDIDATE_CAP));
        let capped = candidates.len() >= CANDIDATE_CAP;
        let mut rows = Vec::new();
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid, frn, name, parent_frn, attributes, is_dir
             FROM file_metadata WHERE volume_id = ?1 AND rowid = ?2",
            )
            .expect("schema validated at open");
        for rowid in candidates {
            let row = statement.query_row(params![self.volume.as_str(), rowid], row_from_sql);
            if let Ok(row) = row {
                if row.name.to_lowercase().contains(query_lower) {
                    rows.push(row);
                }
            }
        }
        Ok((rows, capped))
    }

    fn like_rows(&self, query_lower: &str, limit: usize) -> Result<(Vec<Row>, bool), IndexError> {
        let pattern = format!("%{}%", escape_like(query_lower));
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid, frn, name, parent_frn, attributes, is_dir
             FROM file_metadata
             WHERE volume_id = ?1 AND name LIKE ?2 ESCAPE '\\' COLLATE NOCASE
             LIMIT ?3",
            )
            .expect("schema validated at open");
        let rows = statement
            .query_map(
                params![
                    self.volume.as_str(),
                    pattern,
                    (limit.saturating_mul(4)).min(CANDIDATE_CAP) as i64
                ],
                row_from_sql,
            )
            .expect("schema validated at open");
        let rows: Vec<Row> = rows.collect::<Result<Vec<_>, _>>()?;
        Ok((rows, false))
    }

    fn paths(&self, frns: &[u64]) -> Result<HashMap<u64, (String, usize)>, IndexError> {
        if frns.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = (0..frns.len())
            .map(|position| format!("?{}", position + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "WITH RECURSIVE chain(root, frn, parent_frn, name, depth) AS (
                 SELECT frn, frn, parent_frn, name, 0
                 FROM file_metadata
                 WHERE volume_id = ?1 AND frn IN ({placeholders})
                 UNION ALL
                 SELECT chain.root, parent.frn, parent.parent_frn, parent.name, chain.depth + 1
                 FROM file_metadata parent
                 JOIN chain ON parent.frn = chain.parent_frn
                 WHERE parent.volume_id = ?1
                   AND chain.parent_frn IS NOT NULL
                   AND chain.parent_frn != 0
             )
             SELECT root, name, depth FROM chain ORDER BY root, depth DESC"
        );
        let mut values = Vec::with_capacity(frns.len() + 1);
        values.push(Value::Text(self.volume.as_str().to_owned()));
        values.extend(frns.iter().map(|frn| Value::Integer(*frn as i64)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            let root: i64 = row.get(0)?;
            let depth: i64 = row.get(2)?;
            Ok((
                u64::try_from(root)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, root))?,
                row.get::<_, String>(1)?,
                usize::try_from(depth)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, depth))?,
            ))
        })?;
        let mut chains: HashMap<u64, Vec<(String, usize)>> = HashMap::new();
        for row in rows {
            let (root, name, depth) = row?;
            chains.entry(root).or_default().push((name, depth));
        }
        let mut paths = HashMap::with_capacity(chains.len());
        for (root, names) in chains {
            let mut path = format!("{}\\", self.volume.as_str());
            for (name, _) in &names {
                if name == "." {
                    continue;
                }
                if !path.ends_with('\\') {
                    path.push('\\');
                }
                path.push_str(name);
            }
            let depth = names.last().map_or(0, |(_, depth)| *depth);
            paths.insert(root, (path, depth));
        }
        Ok(paths)
    }
}

fn prefix_bounds(value: &str) -> (String, String) {
    let mut characters: Vec<char> = value.chars().collect();
    while let Some(last) = characters.pop() {
        if let Some(next) = char::from_u32(u32::from(last) + 1) {
            characters.push(next);
            return (value.to_owned(), characters.into_iter().collect());
        }
    }
    let mut upper = value.to_owned();
    upper.push(char::MAX);
    (value.to_owned(), upper)
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let frn: i64 = row.get(1)?;
    Ok(Row {
        rowid: row.get(0)?,
        frn: u64::try_from(frn).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, frn))?,
        name: row.get(2)?,
        attributes: row.get(4)?,
        is_dir: row.get::<_, i64>(5)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileRecord;
    use crate::state::Checkpoint;
    use crate::store::{IndexStore, PersistBatch, VolumeMeta};
    use crate::trigram::TrigramBuilder;
    use tempfile::TempDir;

    fn record(frn: u64, parent: u64, name: &str, is_dir: bool) -> FileRecord {
        FileRecord {
            frn,
            parent_frn: parent,
            name: name.to_owned(),
            attributes: 0x20,
            is_dir,
            last_usn: 0,
        }
    }

    fn fixture() -> (TempDir, SqliteSearch) {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("index.db");
        let trigram = directory.path().join("trigram.bin");
        let volume = VolumeId::new("C:").unwrap();
        let meta = VolumeMeta::new(volume.clone(), "System", "NTFS").unwrap();
        let mut store = IndexStore::open(&database, meta).unwrap();
        store
            .commit_batch(&PersistBatch {
                upserts: vec![
                    record(5, 0, ".", true),
                    record(10, 5, "docs", true),
                    record(11, 10, "alpha.txt", false),
                    record(12, 10, "alphabet.txt", false),
                ],
                deletes: Vec::new(),
                checkpoint: Checkpoint {
                    journal_id: 7,
                    next_usn: 4,
                    index_generation: 1,
                    committed_at: 0,
                },
                sync_state: None,
            })
            .unwrap();
        store.build_trigram_index(&volume, &trigram).unwrap();
        drop(store);
        let search = SqliteSearch::open(&database, &trigram, volume).unwrap();
        (directory, search)
    }

    #[test]
    fn empty_and_zero_limit_queries_are_empty() {
        let (_directory, search) = fixture();
        assert!(search.search("", 10).unwrap().hits.is_empty());
        assert!(search.search("alpha", 0).unwrap().hits.is_empty());
    }

    #[test]
    fn prefix_limit_skips_substring_fallback() {
        let (_directory, search) = fixture();
        let outcome = search.search("a", 1).unwrap();
        assert_eq!(outcome.hits.len(), 1);
    }

    #[test]
    fn prefix_bounds_handle_unicode_max() {
        let (lower, upper) = prefix_bounds("\u{10FFFF}");
        assert_eq!(lower, "\u{10FFFF}");
        assert!(upper.ends_with(char::MAX));
        let (lower, upper) = prefix_bounds("alpha");
        assert_eq!(lower, "alpha");
        assert_eq!(upper, "alphb");
    }

    #[test]
    fn debug_reports_volume() {
        let (_directory, search) = fixture();
        assert!(format!("{search:?}").contains("C:"));
    }

    #[test]
    fn open_missing_database_is_reported() {
        let directory = TempDir::new().unwrap();
        let trigram = directory.path().join("trigram.bin");
        assert!(SqliteSearch::open(
            &directory.path().join("missing.db"),
            &trigram,
            VolumeId::new("C:").unwrap()
        )
        .is_err());
    }

    #[test]
    fn open_missing_schema_is_reported() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("empty.db");
        Connection::open(&database).unwrap();
        let trigram = directory.path().join("trigram.bin");
        assert!(SqliteSearch::open(&database, &trigram, VolumeId::new("C:").unwrap()).is_err());
    }

    #[test]
    fn ghost_trigram_candidate_is_skipped() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("index.db");
        let trigram = directory.path().join("trigram.bin");
        let volume = VolumeId::new("C:").unwrap();
        let meta = VolumeMeta::new(volume.clone(), "System", "NTFS").unwrap();
        let mut store = IndexStore::open(&database, meta).unwrap();
        store
            .commit_batch(&PersistBatch {
                upserts: vec![record(10, 5, "alpha.txt", false)],
                deletes: Vec::new(),
                checkpoint: Checkpoint {
                    journal_id: 7,
                    next_usn: 1,
                    index_generation: 1,
                    committed_at: 0,
                },
                sync_state: None,
            })
            .unwrap();
        drop(store);
        let mut builder = TrigramBuilder::new();
        builder.add(999, b"ghost");
        builder.write(&trigram).unwrap();
        let search = SqliteSearch::open(&database, &trigram, volume).unwrap();
        assert!(search.search("ghost", 10).unwrap().hits.is_empty());
    }

    #[test]
    fn directory_match_gets_penalty() {
        let (_directory, search) = fixture();
        let outcome = search.search("docs", 10).unwrap();
        assert_eq!(outcome.hits.len(), 1);
        assert!(outcome.hits[0].item.is_dir);
    }

    #[test]
    fn prefix_and_exact_hits(/*covered*/) {
        let (_directory, search) = fixture();
        let outcome = search.search("alpha", 10).unwrap();
        assert_eq!(outcome.hits[0].item.name, "alpha.txt");
        assert!(outcome
            .hits
            .iter()
            .any(|hit| hit.item.name == "alphabet.txt"));
        assert_eq!(outcome.hits[0].item.path, "C:\\docs\\alpha.txt");
    }

    #[test]
    fn substring_uses_trigram_candidates(/*covered*/) {
        let (_directory, search) = fixture();
        let outcome = search.search("lpha", 10).unwrap();
        assert_eq!(outcome.hits.len(), 2);
        assert!(outcome
            .hits
            .iter()
            .all(|hit| hit.item.name.contains("lpha")));
    }

    #[test]
    fn no_prefix_falls_back_to_short_like(/*covered*/) {
        let (_directory, search) = fixture();
        let outcome = search.search("ph", 10).unwrap();
        assert_eq!(outcome.hits.len(), 2);
    }

    #[test]
    fn like_escape_blocks_wildcards(/*covered*/) {
        let (_directory, search) = fixture();
        assert!(search.search("%", 10).unwrap().hits.is_empty());
        assert!(search.search("_", 10).unwrap().hits.is_empty());
    }
}
