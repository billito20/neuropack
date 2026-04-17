use crate::asset_scanner::AssetMetadata;
use aho_corasick::{AhoCorasick, MatchKind};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

const DEFAULT_MAX_PATTERNS: usize = 4096;

/// Maximum bytes read from each file for dictionary scanning.
/// Scanning 64 KB per file is enough to discover repeated byte patterns;
/// reading the full file for large assets wastes RAM with no benefit.
const MAX_SCAN_BYTES: usize = 64 * 1024; // 64 KB

/// Maximum number of files sampled for dictionary building.
/// For large datasets (Kenshi 12 GB, HoI4 9.5 GB) scanning every file
/// produces O(total_bytes / stride) unique hashes — many GB of HashMaps.
/// A stratified 2 000-file sample gives representative coverage of all asset
/// types while keeping peak RAM well below 512 MB.
const MAX_SAMPLE_FILES: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryPattern {
    pub id: u32,
    pub hash: u64,
    pub bytes: Vec<u8>,
    pub frequency: usize,
    pub benefit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dictionary {
    pub patterns: Vec<DictionaryPattern>,
    /// First-byte dispatch table (fallback if AC unavailable).
    #[serde(skip)]
    dispatch: HashMap<u8, Vec<usize>>,
    /// Aho-Corasick automaton for O(n) multi-pattern matching.
    /// Not serialized — rebuilt by `prepare()`.
    #[serde(skip)]
    ac: Option<AhoCorasick>,
}

impl Default for Dictionary {
    fn default() -> Self {
        Self { patterns: Vec::new(), dispatch: HashMap::new(), ac: None }
    }
}

impl Dictionary {
    /// Build a global dictionary from a representative sample of assets.
    ///
    /// Memory-safe design for large datasets (12 GB+):
    /// 1. **File cap**: at most MAX_SAMPLE_FILES files are scanned.
    ///    Files are chosen with stride `assets.len() / MAX_SAMPLE_FILES` to
    ///    sample evenly across the full asset list rather than only the first N.
    /// 2. **Byte cap**: each file is read up to MAX_SCAN_BYTES (64 KB).
    ///    64 KB / 16-byte stride = 4 096 windows — enough to detect repeated
    ///    patterns without pulling multi-MB assets fully into RAM.
    /// 3. **fold + reduce**: per-file maps are merged into per-thread accumulators
    ///    as they're produced, so only O(num_threads) maps live in RAM at once
    ///    instead of O(num_files).
    pub fn build(
        assets: &[AssetMetadata],
        min_frequency: usize,
        window_size: usize,
    ) -> anyhow::Result<Self> {
        let window_size = window_size.max(32);
        // Stride of window_size/4 samples every quarter-window.
        let stride = (window_size / 4).max(1);

        // Stratified sample: pick evenly spaced indices so all asset types are
        // represented even when the list is sorted by directory / extension.
        let sample: Vec<&AssetMetadata> = if assets.len() <= MAX_SAMPLE_FILES {
            assets.iter().collect()
        } else {
            let step = assets.len() / MAX_SAMPLE_FILES;
            assets.iter().step_by(step).take(MAX_SAMPLE_FILES).collect()
        };

        // Scan in parallel; merge with fold+reduce so only O(num_threads) maps
        // are live at once rather than one map per file.
        type Accum = (HashMap<u64, usize>, HashMap<u64, Vec<u8>>);

        let merge = |mut acc: Accum, (file_counts, file_payloads): Accum| -> Accum {
            for (hash, count) in file_counts {
                *acc.0.entry(hash).or_insert(0) += count;
            }
            for (hash, bytes) in file_payloads {
                acc.1.entry(hash).or_insert(bytes);
            }
            acc
        };

        let (pattern_counts, pattern_payloads): Accum = sample
            .par_iter()
            .filter_map(|asset| {
                scan_file_strided(&asset.path, window_size, stride, min_frequency).ok()
            })
            .fold(|| (HashMap::new(), HashMap::new()), merge)
            .reduce(|| (HashMap::new(), HashMap::new()), merge);

        let mut patterns: Vec<DictionaryPattern> = pattern_payloads
            .into_iter()
            .filter_map(|(hash, bytes)| {
                let frequency = *pattern_counts.get(&hash).unwrap_or(&0);
                let benefit = frequency
                    .saturating_mul(bytes.len())
                    .saturating_sub(8 + bytes.len());
                if frequency >= min_frequency && benefit > 0 {
                    Some(DictionaryPattern { id: 0, hash, bytes, frequency, benefit })
                } else {
                    None
                }
            })
            .collect();

        patterns.sort_unstable_by_key(|p| usize::MAX - p.benefit);
        patterns.truncate(DEFAULT_MAX_PATTERNS);
        for (idx, p) in patterns.iter_mut().enumerate() {
            p.id = idx as u32;
        }

        let mut dict = Dictionary { patterns, dispatch: HashMap::new(), ac: None };
        dict.prepare();
        Ok(dict)
    }

    /// Build the dispatch table and Aho-Corasick automaton.
    /// Must be called after deserialization.
    pub fn prepare(&mut self) {
        // Dispatch table (fallback path).
        self.dispatch.clear();
        for (idx, pattern) in self.patterns.iter().enumerate() {
            if let Some(&first) = pattern.bytes.first() {
                self.dispatch.entry(first).or_default().push(idx);
            }
        }

        // Aho-Corasick automaton — leftmost-longest gives the best substitution
        // coverage: at each position the longest matching pattern wins.
        if !self.patterns.is_empty() {
            let pats: Vec<&[u8]> = self.patterns.iter().map(|p| p.bytes.as_slice()).collect();
            self.ac = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&pats)
                .ok();
        }
    }

    /// Replace known patterns with back-references using Aho-Corasick.
    /// Falls back to the first-byte dispatch table if the automaton is unavailable.
    pub fn apply_patterns(&self, input: &[u8]) -> Vec<Segment> {
        if let Some(ac) = &self.ac {
            return apply_with_ac(ac, input);
        }
        apply_with_dispatch(&self.dispatch, &self.patterns, input)
    }
}

/// O(n) pattern substitution via Aho-Corasick.
/// Single linear scan over the input; no per-byte HashMap lookup.
fn apply_with_ac(ac: &AhoCorasick, input: &[u8]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut last_end = 0usize;

    for mat in ac.find_iter(input) {
        if mat.start() > last_end {
            segments.push(Segment::Literal(input[last_end..mat.start()].to_vec()));
        }
        segments.push(Segment::Reference(mat.pattern().as_u32()));
        last_end = mat.end();
    }
    if last_end < input.len() {
        segments.push(Segment::Literal(input[last_end..].to_vec()));
    }
    if segments.is_empty() {
        segments.push(Segment::Literal(input.to_vec()));
    }
    segments
}

/// Fallback dispatch-table path (used when AC is not built).
fn apply_with_dispatch(
    dispatch: &HashMap<u8, Vec<usize>>,
    patterns: &[DictionaryPattern],
    input: &[u8],
) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut cursor = 0usize;
    let mut literal_start = 0usize;

    while cursor < input.len() {
        let mut matched = false;
        if let Some(indices) = dispatch.get(&input[cursor]) {
            for &idx in indices {
                let patt = &patterns[idx].bytes;
                let plen = patt.len();
                if cursor + plen <= input.len() && input[cursor..cursor + plen] == *patt {
                    if cursor > literal_start {
                        segments.push(Segment::Literal(input[literal_start..cursor].to_vec()));
                    }
                    segments.push(Segment::Reference(patterns[idx].id));
                    cursor += plen;
                    literal_start = cursor;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            cursor += 1;
        }
    }
    if literal_start < input.len() {
        segments.push(Segment::Literal(input[literal_start..].to_vec()));
    }
    if segments.is_empty() {
        segments.push(Segment::Literal(input.to_vec()));
    }
    segments
}

/// Scan one file with a strided window, reading at most MAX_SCAN_BYTES.
/// Returns (hash → count, hash → first-seen bytes).
fn scan_file_strided(
    path: &Path,
    window_size: usize,
    stride: usize,
    min_frequency: usize,
) -> anyhow::Result<(HashMap<u64, usize>, HashMap<u64, Vec<u8>>)> {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    let mut payloads: HashMap<u64, Vec<u8>> = HashMap::new();

    let file = File::open(path)?;
    let reader = BufReader::with_capacity(MAX_SCAN_BYTES, file);
    let mut data = Vec::with_capacity(MAX_SCAN_BYTES);
    reader.take(MAX_SCAN_BYTES as u64).read_to_end(&mut data)?;

    if data.len() < window_size {
        return Ok((counts, payloads));
    }

    let mut start = 0usize;
    while start + window_size <= data.len() {
        let window = &data[start..start + window_size];
        let hash = xxh3_64(window);
        let count = counts.entry(hash).or_insert(0);
        *count += 1;
        if *count == min_frequency {
            payloads.insert(hash, window.to_vec());
        }
        start += stride;
    }

    Ok((counts, payloads))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Segment {
    Literal(Vec<u8>),
    Reference(u32),
}
