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
    #[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
    fn scan_substring(
        &self,
        query_lower: &str,
        candidates: &mut Vec<(i32, usize)>,
        truncated: &mut bool,
    ) {
        let query_signature = trigram_signature(query_lower);
        let query_quad_signature = quadgram_signature_bytes(query_lower.as_bytes());
        // Prefix buckets are disjoint, so only the fallback needs deduplication.
        let mut seen: Vec<usize> = candidates.iter().map(|(_, slot)| *slot).collect();
        seen.sort_unstable();
        let mut scanned = 0usize;
        for slot in 0..self.slots.len() {
            if self.slot_flags.get(slot).copied().unwrap_or(0) & pstart_core::walker::SLOT_PRESENT
                == 0
            {
                continue;
            }
            let Some(name) = self.slot_name(slot) else {
                continue;
            };
            if seen.binary_search(&slot).is_ok() {
                continue;
            }
            if scanned >= MAX_SUBSTRING_SCAN {
                *truncated = true;
                break;
            }
            scanned += 1;
            if query_signature != 0 {
                let bits = self.slot_signatures.get(slot).copied().unwrap_or(0) & query_signature;
                if bits != query_signature {
                    continue;
                }
            }
            if query_quad_signature != 0 {
                let bits = self.slot_quad_signatures.get(slot).copied().unwrap_or(0)
                    & query_quad_signature;
                if bits != query_quad_signature {
                    continue;
                }
            }
            if !contains_lowercase_needle(name, query_lower) {
                continue;
            }
            let lowered = needs_lowercase(name).then(|| name.to_lowercase());
            let name_lower = lowered.as_deref().unwrap_or(name);
            if let Some(score) = crate::score::score_name_lower(name, name_lower, query_lower) {
                candidates.push((score, slot));
            }
        }
    }

    #[allow(clippy::collapsible_if)] // sequential conditions keep MC/DC satisfiable
    pub fn search(&self, query: &str, limit: u32) -> SearchOutcome {
        let query = query.trim();
        let limit = limit.min(MAX_RESULTS) as usize;
        if query.is_empty() {
            return SearchOutcome::default();
        }
        if limit == 0 {
            return SearchOutcome::default();
        }

        let query_lower = query.to_lowercase();
        let mut candidates: Vec<(i32, usize)> = Vec::new();
        let mut truncated = false;

        let start = self.names.sorted.partition_point(|reference| {
            self.ref_name(*reference)
                .is_some_and(|name| Self::lower_cmp(name, &query_lower) == std::cmp::Ordering::Less)
        });
        let mut push = |reference: u64| -> bool {
            let Some(name) = self.ref_name(reference) else {
                return false;
            };
            let (slot, _) = name_index::unpack(reference);
            let slot = slot as usize;
            if self.slot_flags.get(slot).copied().unwrap_or(0) & pstart_core::walker::SLOT_PRESENT
                == 0
            {
                return false;
            }
            if candidates.len() >= MAX_PREFIX_CANDIDATES {
                return true;
            }
            let base = if Self::lower_cmp(name, &query_lower) == std::cmp::Ordering::Equal {
                SCORE_EXACT
            } else {
                SCORE_PREFIX
            };
            let chars = crate::score::char_count(name);
            candidates.push((apply_penalty(base, chars), slot));
            false
        };
        for reference in &self.names.sorted[start..] {
            let Some(name) = self.ref_name(*reference) else {
                continue;
            };
            if !Self::lower_starts_with(name, &query_lower) {
                break;
            }
            if push(*reference) {
                truncated = true;
                break;
            }
        }
        if !truncated {
            for reference in &self.names.dirty {
                let Some(name) = self.ref_name(*reference) else {
                    continue;
                };
                if !Self::lower_starts_with(name, &query_lower) {
                    continue;
                }
                if push(*reference) {
                    truncated = true;
                    break;
                }
            }
        }

        if candidates.len() < limit {
            if self.might_contain_substring(&query_lower) {
                self.scan_substring(&query_lower, &mut candidates, &mut truncated);
            }
        }

        let mut compare = |(left_score, left_slot): &(i32, usize),
                           (right_score, right_slot): &(i32, usize)| {
            right_score
                .cmp(left_score)
                .then_with(|| {
                    self.slot_name(*left_slot)
                        .unwrap_or("")
                        .cmp(self.slot_name(*right_slot).unwrap_or(""))
                })
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
            if position == sorted_end {
                if sorted_end < candidates.len() {
                    candidates[sorted_end..].sort_by(&mut compare);
                    sorted_end = candidates.len();
                }
            }
            let (score, slot) = candidates[position];
            position += 1;
            let record = self
                .record_from_slot(slot)
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
                    name: record.name.as_ref().into(),
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
