//! Bounded search and ranking.

use super::*;

/// One ranked search hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// Wire-ready file item.
    pub item: FileItem,
    /// Explainable score.
    pub score: i32,
}

/// Search result plus whether the bounded scan stopped early.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOutcome {
    /// Ranked hits.
    pub hits: Vec<SearchHit>,
    /// True when a candidate or scan budget was reached.
    pub truncated: bool,
}

impl MemoryIndex {
    pub fn search(&self, query: &str, limit: u32) -> SearchOutcome {
        let query = query.trim();
        let limit = limit.min(MAX_RESULTS) as usize;
        if query.is_empty() || limit == 0 {
            return SearchOutcome::default();
        }

        let query_lower = query.to_lowercase();
        let mut candidates: Vec<(i32, usize)> = Vec::new();
        let mut truncated = false;

        for (name, slots) in self.by_name.range(query_lower.clone()..) {
            if !name.starts_with(&query_lower) {
                break;
            }
            for slot in slots {
                if candidates.len() >= MAX_PREFIX_CANDIDATES {
                    truncated = true;
                    break;
                }
                if let Some(record) = self.slots.get(*slot).and_then(Option::as_ref) {
                    let base = if name == &query_lower {
                        SCORE_EXACT
                    } else {
                        SCORE_PREFIX
                    };
                    let score = apply_penalty(base, record.name.chars().count());
                    candidates.push((score, *slot));
                }
            }
            if truncated {
                break;
            }
        }

        if candidates.len() < limit && self.might_contain_substring(&query_lower) {
            let query_signature = trigram_signature(&query_lower);
            let query_quad_signature = quadgram_signature_bytes(query_lower.as_bytes());
            // Prefix buckets are disjoint, so only the fallback needs deduplication.
            let mut seen: Vec<usize> = candidates.iter().map(|(_, slot)| *slot).collect();
            seen.sort_unstable();
            let mut scanned = 0usize;
            for (slot, record) in self.slots.iter().enumerate() {
                let Some(record) = record.as_ref() else {
                    continue;
                };
                if seen.binary_search(&slot).is_ok() {
                    continue;
                }
                if scanned >= MAX_SUBSTRING_SCAN {
                    truncated = true;
                    break;
                }
                scanned += 1;
                if query_signature != 0
                    && self.slot_signatures.get(slot).copied().unwrap_or(0) & query_signature
                        != query_signature
                {
                    continue;
                }
                if query_quad_signature != 0
                    && self.slot_quad_signatures.get(slot).copied().unwrap_or(0)
                        & query_quad_signature
                        != query_quad_signature
                {
                    continue;
                }
                if !contains_lowercase_needle(&record.name, &query_lower) {
                    continue;
                }
                if let Some(score) = score_name(&record.name, query_lower.as_str()) {
                    candidates.push((score, slot));
                }
            }
        }

        let mut compare = |(left_score, left_slot): &(i32, usize),
                           (right_score, right_slot): &(i32, usize)| {
            right_score
                .cmp(left_score)
                .then_with(|| self.slot_name(*left_slot).cmp(self.slot_name(*right_slot)))
                .then_with(|| left_slot.cmp(right_slot))
        };
        let mut sorted_end = candidates.len();
        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, &mut compare);
            candidates[..limit].sort_by(&mut compare);
            sorted_end = limit;
        } else {
            candidates.sort_by(&mut compare);
        }

        let mut hits = Vec::with_capacity(limit.min(candidates.len()));
        let mut position = 0usize;
        while position < candidates.len() {
            if hits.len() >= limit {
                break;
            }
            if position == sorted_end && sorted_end < candidates.len() {
                candidates[sorted_end..].sort_by(&mut compare);
                sorted_end = candidates.len();
            }
            let (score, slot) = candidates[position];
            position += 1;
            let record = self
                .slots
                .get(slot)
                .and_then(Option::as_ref)
                .expect("search candidate slot is live");
            let path = match self.resolve_record_path(Frn::new(record.frn)) {
                Ok(path) => path,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            let depth = path.matches('\\').count() as i32;
            let directory = if record.is_dir {
                SCORE_DIRECTORY_PENALTY
            } else {
                0
            };
            hits.push(SearchHit {
                item: FileItem {
                    id: FileId::new(self.volume.clone(), Frn::new(record.frn)),
                    name: record.name.clone(),
                    path,
                    attributes: record.attributes,
                    is_dir: record.is_dir,
                },
                score: (score - depth.min(SCORE_DEPTH_PENALTY_MAX) - directory).max(1),
            });
        }
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.item.name.cmp(&right.item.name))
                .then_with(|| left.item.path.cmp(&right.item.path))
        });

        SearchOutcome { hits, truncated }
    }
}
