//! Standalone compact-index memory probe.
//!
//! Uses synthetic records only; it does not touch `MemoryIndex` or production
//! state. Run with `cargo run --release -p pstart-index --example compact_probe`.

use std::hint::black_box;
use std::time::Instant;

#[cfg(windows)]
#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> isize;
}

#[cfg(windows)]
#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(process: isize, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
}

#[cfg(windows)]
fn working_set_bytes() -> usize {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    // SAFETY: the handle is the current process and the counters pointer is
    // valid for the declared size.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        0
    } else {
        counters.working_set_size
    }
}

#[cfg(not(windows))]
fn working_set_bytes() -> usize {
    0
}

struct CompactIndex {
    names: Vec<u8>,
    name_offset: Vec<u32>,
    name_len: Vec<u16>,
    frn: Vec<u64>,
    parent: Vec<u32>,
    attributes: Vec<u16>,
    flags: Vec<u8>,
    last_usn: Vec<u32>,
    by_name: Vec<u32>,
}

impl CompactIndex {
    fn build(count: usize) -> Self {
        let mut names = Vec::with_capacity(count * 16);
        let mut name_offset = Vec::with_capacity(count);
        let mut name_len = Vec::with_capacity(count);
        let mut frn = Vec::with_capacity(count);
        let mut parent = Vec::with_capacity(count);
        let mut attributes = Vec::with_capacity(count);
        let mut flags = Vec::with_capacity(count);
        let mut last_usn = Vec::with_capacity(count);

        for position in 0..count {
            let name = if position % 1000 == 0 {
                format!("notepad-{position:07}.txt")
            } else if position % 3 == 0 {
                format!("a{position:07}.dat")
            } else {
                format!("file_{position:07}.txt")
            };
            name_offset.push(names.len() as u32);
            name_len.push(name.len() as u16);
            names.extend_from_slice(name.as_bytes());
            frn.push((position as u64) + 100);
            parent.push(4);
            attributes.push(0x20);
            flags.push(u8::from(position % 11 == 0));
            last_usn.push(position as u32);
        }

        let mut by_name: Vec<u32> = (0..count as u32).collect();
        by_name.sort_unstable_by(|left, right| {
            Self::name_of_raw(&names, &name_offset, &name_len, *left).cmp(Self::name_of_raw(
                &names,
                &name_offset,
                &name_len,
                *right,
            ))
        });

        Self {
            names,
            name_offset,
            name_len,
            frn,
            parent,
            attributes,
            flags,
            last_usn,
            by_name,
        }
    }

    fn name_of_raw<'a>(names: &'a [u8], offsets: &[u32], lengths: &[u16], slot: u32) -> &'a [u8] {
        let start = offsets[slot as usize] as usize;
        let end = start + lengths[slot as usize] as usize;
        &names[start..end]
    }

    fn name(&self, slot: u32) -> &[u8] {
        Self::name_of_raw(&self.names, &self.name_offset, &self.name_len, slot)
    }

    fn capacity_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_offset.capacity() * 4
            + self.name_len.capacity() * 2
            + self.frn.capacity() * 8
            + self.parent.capacity() * 4
            + self.attributes.capacity() * 2
            + self.flags.capacity()
            + self.last_usn.capacity() * 4
            + self.by_name.capacity() * 4
    }

    fn prefix_range(&self, needle: &[u8]) -> (usize, usize) {
        let start = self
            .by_name
            .partition_point(|slot| self.name(*slot) < needle);
        let mut end = start;
        while end < self.by_name.len() && self.name(self.by_name[end]).starts_with(needle) {
            end += 1;
        }
        (start, end)
    }

    fn substring_count(&self, needle: &[u8]) -> usize {
        let mut count = 0;
        for slot in &self.by_name {
            let name = self.name(*slot);
            if name.windows(needle.len()).any(|window| window == needle) {
                count += 1;
            }
        }
        count
    }
}

fn percentile(mut values: Vec<u128>, percentile: usize) -> u128 {
    values.sort_unstable();
    values[values.len() * percentile / 100]
}

const MAX_INDEX_MEMORY_BYTES: usize = 100 * 1024 * 1024;

fn main() {
    let baseline = working_set_bytes();
    let build_start = Instant::now();
    let index = CompactIndex::build(1_200_000);
    let build = build_start.elapsed();
    let after_build = working_set_bytes();
    let capacity_bytes = index.capacity_bytes();
    let delta = after_build.saturating_sub(baseline);

    let query = |needle: &[u8]| {
        let mut samples = Vec::with_capacity(200);
        let mut matches = 0;
        for _ in 0..200 {
            let start = Instant::now();
            let (begin, end) = index.prefix_range(needle);
            matches = end - begin;
            black_box(&index.by_name[begin..end]);
            samples.push(start.elapsed().as_micros().max(1));
        }
        (
            percentile(samples.clone(), 50),
            percentile(samples, 95),
            matches,
        )
    };

    let (notepad_p50, notepad_p95, notepad_matches) = query(b"notepad");
    let (a_p50, a_p95, a_matches) = query(b"a");
    let (none_p50, none_p95, none_matches) = query(b"zzzz-no-match");

    let substring = |needle: &[u8]| {
        let mut samples = Vec::with_capacity(50);
        let mut matches = 0;
        for _ in 0..50 {
            let start = Instant::now();
            matches = index.substring_count(needle);
            samples.push(start.elapsed().as_micros().max(1));
        }
        (
            percentile(samples.clone(), 50),
            percentile(samples, 95),
            matches,
        )
    };
    let (short_p50, short_p95, short_matches) = substring(b"no");
    let (mid_p50, mid_p95, mid_matches) = substring(b"pad-000");

    println!("records={}", index.by_name.len());
    println!("build_ms={}", build.as_millis());
    println!("retained_capacity_bytes={capacity_bytes}");
    println!("working_set_baseline={baseline}");
    println!("working_set_after_build={after_build}");
    println!("working_set_delta={delta}");
    println!("notepad_us={notepad_p50}/{notepad_p95} matches={notepad_matches}");
    println!("a_us={a_p50}/{a_p95} matches={a_matches}");
    println!("none_us={none_p50}/{none_p95} matches={none_matches}");
    println!("substring2_us={short_p50}/{short_p95} matches={short_matches}");
    println!("substring6_us={mid_p50}/{mid_p95} matches={mid_matches}");
    println!("memory_limit_bytes={MAX_INDEX_MEMORY_BYTES}");
    assert!(
        delta <= MAX_INDEX_MEMORY_BYTES,
        "compact index memory {delta} exceeds {MAX_INDEX_MEMORY_BYTES}"
    );
}
