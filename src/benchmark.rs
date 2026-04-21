use crate::asset_scanner::AssetScanner;
use crate::compression::Pipeline;
use crate::decompression::PackageReader;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorResult {
    pub compressed_bytes: u64,
    pub compress_ms: u128,
    pub decompress_ms: u128,
    /// Compression ratio: original_bytes / compressed_bytes. Higher is better.
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub file_count: usize,
    pub total_bytes: u64,
    /// Zstandard at level 3 (fast, default for many tools).
    pub zstd_3: CompressorResult,
    /// Zstandard at level 9 (balanced).
    pub zstd_9: CompressorResult,
    /// Zstandard at level 19 (maximum; slow but best ratio).  `null` when `--skip-zstd19` was passed.
    pub zstd_19: Option<CompressorResult>,
    /// Brotli at quality 5 (fast).  `null` when `--no-brotli` was passed.
    pub brotli_5: Option<CompressorResult>,
    /// Brotli at quality 11 (maximum; very slow).  `null` when `--no-brotli` was passed.
    pub brotli_11: Option<CompressorResult>,
    /// NeuroPack baseline: no asset-type strategies (all zstd-3, no delta, no dict).  `null` when `--skip-neuropack` was passed.
    pub neuropack_baseline: Option<CompressorResult>,
    /// NeuroPack optimised: current default (type-aware levels, mesh delta, dict).  `null` when `--skip-neuropack` was passed.
    pub neuropack: Option<CompressorResult>,
}

#[derive(Default)]
pub struct BenchmarkRunner;

impl BenchmarkRunner {
    pub fn run<P: AsRef<Path>>(&self, root: P, output: P, no_brotli: bool, no_zstd19: bool, no_neuropack: bool) -> anyhow::Result<()> {
        let root = root.as_ref();
        let output = output.as_ref();
        let output_dir = output.parent().unwrap_or_else(|| std::path::Path::new("."));
        let scanner = AssetScanner::default();
        let assets = scanner.scan(root)?;

        let total_bytes: u64 = assets.iter().map(|a| a.size).sum();
        let gb = total_bytes as f64 / 1_073_741_824.0;
        let mut flags = Vec::new();
        if no_brotli    { flags.push("brotli skipped"); }
        if no_zstd19    { flags.push("zstd-19 skipped"); }
        if no_neuropack { flags.push("neuropack skipped"); }
        let flag_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
        eprintln!("Benchmarking {} files ({:.2} GB){} ...", assets.len(), gb, flag_str);

        // ── Zstandard (levels 3, 9, 19) ───────────────────────────────────
        eprintln!("  zstd-3 ...");
        let zstd_3  = bench_zstd(&assets,  3, total_bytes)?;
        eprintln!("  zstd-9 ...");
        let zstd_9  = bench_zstd(&assets,  9, total_bytes)?;
        let zstd_19 = if no_zstd19 {
            eprintln!("  zstd-19 skipped.");
            None
        } else {
            eprintln!("  zstd-19 ...");
            Some(bench_zstd(&assets, 19, total_bytes)?)
        };

        // ── Brotli (qualities 5, 11) — skip on large datasets ─────────────
        let (brotli_5, brotli_11) = if no_brotli {
            eprintln!("  brotli skipped.");
            (None, None)
        } else {
            eprintln!("  brotli-5 ...");
            let b5 = bench_brotli(&assets, 5, total_bytes)?;
            eprintln!("  brotli-11 ...");
            let b11 = bench_brotli(&assets, 11, total_bytes)?;
            (Some(b5), Some(b11))
        };

        // ── NeuroPack baseline + optimised ───────────────────────────────
        let (neuropack_baseline, neuropack) = if no_neuropack {
            eprintln!("  NeuroPack skipped.");
            (None, None)
        } else {
            eprintln!("  NeuroPack baseline ...");
            let baseline = self.run_neuropack_with_pipeline(
                root,
                Pipeline {
                    enable_mesh_delta: false,
                    bypass_audio_dictionary: false,
                    texture_zstd_level: 3,
                    audio_zstd_level: 3,
                    default_zstd_level: 3,
                    ..Pipeline::default()
                },
                total_bytes,
                output_dir,
            )?;
            eprintln!("  NeuroPack optimised ...");
            let optimised = self.run_neuropack_with_pipeline(
                root, Pipeline::default(), total_bytes, output_dir,
            )?;
            (Some(baseline), Some(optimised))
        };

        let report = BenchmarkReport {
            file_count: assets.len(),
            total_bytes,
            zstd_3,
            zstd_9,
            zstd_19,
            brotli_5,
            brotli_11,
            neuropack_baseline,
            neuropack,
        };

        let serialized = serde_json::to_vec_pretty(&report)?;
        File::create(output)?.write_all(&serialized)?;
        Ok(())
    }

    fn run_neuropack_with_pipeline(
        &self,
        root: &Path,
        pipeline: Pipeline,
        total_bytes: u64,
        output_dir: &Path,
    ) -> anyhow::Result<CompressorResult> {
        let tmp_pack = temp_path_in(output_dir, "neuropack-bench", "neuropack")?;

        let t = Instant::now();
        pipeline.compress_folder(root, &tmp_pack)?;
        let compress_ms = t.elapsed().as_millis();
        let compressed_bytes = fs::metadata(&tmp_pack)?.len();

        // Time decompression in-memory (verify reads + decompresses every chunk
        // without writing to disk, making it directly comparable to the in-memory
        // zstd/brotli decompression timings above).
        let t = Instant::now();
        let reader = PackageReader::open(&tmp_pack)?;
        reader.verify()?;
        let decompress_ms = t.elapsed().as_millis();

        let _ = fs::remove_file(&tmp_pack);

        Ok(CompressorResult {
            compressed_bytes,
            compress_ms,
            decompress_ms,
            ratio: ratio(total_bytes, compressed_bytes),
        })
    }
}

// ── per-compressor helpers ─────────────────────────────────────────────────

/// Maximum bytes of compressed output held in memory at once.
/// Files are processed in slices whose total INPUT size ≤ this value.
/// Prevents OOM on large datasets (e.g. 37 GB of PSDs on 32 GB RAM).
const BATCH_BUDGET: u64 = 2 * 1024 * 1024 * 1024; // 2 GB input per batch

fn bench_zstd(
    assets: &[crate::asset_scanner::AssetMetadata],
    level: i32,
    total_bytes: u64,
) -> anyhow::Result<CompressorResult> {
    let mut compressed_bytes_total = 0u64;
    let mut compress_ms_total = 0u128;
    let mut decompress_ms_total = 0u128;

    // Process in batches to cap peak memory usage regardless of dataset size.
    let mut batch_start = 0;
    while batch_start < assets.len() {
        let mut batch_end = batch_start;
        let mut budget = 0u64;
        while batch_end < assets.len() {
            budget += assets[batch_end].size;
            batch_end += 1;
            if budget >= BATCH_BUDGET { break; }
        }
        let batch = &assets[batch_start..batch_end];

        let t = Instant::now();
        let pairs: Vec<(Vec<u8>, usize)> = batch
            .par_iter()
            .map(|asset| -> anyhow::Result<(Vec<u8>, usize)> {
                let raw = fs::read(&asset.path)?;
                let orig_len = raw.len();
                let compressed = zstd::bulk::compress(&raw, level)?;
                Ok((compressed, orig_len))
            })
            .collect::<anyhow::Result<_>>()?;
        compress_ms_total += t.elapsed().as_millis();

        compressed_bytes_total += pairs.iter().map(|(c, _)| c.len() as u64).sum::<u64>();

        let t = Instant::now();
        pairs.par_iter().try_for_each(|(compressed, orig_len)| -> anyhow::Result<()> {
            let _ = zstd::bulk::decompress(compressed, *orig_len)?;
            Ok(())
        })?;
        decompress_ms_total += t.elapsed().as_millis();

        batch_start = batch_end;
    }

    Ok(CompressorResult {
        compressed_bytes: compressed_bytes_total,
        compress_ms: compress_ms_total,
        decompress_ms: decompress_ms_total,
        ratio: ratio(total_bytes, compressed_bytes_total),
    })
}

fn bench_brotli(
    assets: &[crate::asset_scanner::AssetMetadata],
    quality: u32,
    total_bytes: u64,
) -> anyhow::Result<CompressorResult> {
    let mut compressed_bytes_total = 0u64;
    let mut compress_ms_total = 0u128;
    let mut decompress_ms_total = 0u128;

    let mut batch_start = 0;
    while batch_start < assets.len() {
        let mut batch_end = batch_start;
        let mut budget = 0u64;
        while batch_end < assets.len() {
            budget += assets[batch_end].size;
            batch_end += 1;
            if budget >= BATCH_BUDGET { break; }
        }
        let batch = &assets[batch_start..batch_end];

        let t = Instant::now();
        let compressed_all: Vec<Vec<u8>> = batch
            .par_iter()
            .map(|asset| -> anyhow::Result<Vec<u8>> {
                let raw = fs::read(&asset.path)?;
                let mut compressed = Vec::new();
                {
                    let mut w = brotli::CompressorWriter::new(&mut compressed, 4096, quality, 22);
                    w.write_all(&raw)?;
                }
                Ok(compressed)
            })
            .collect::<anyhow::Result<_>>()?;
        compress_ms_total += t.elapsed().as_millis();

        compressed_bytes_total += compressed_all.iter().map(|c| c.len() as u64).sum::<u64>();

        let t = Instant::now();
        compressed_all.par_iter().try_for_each(|compressed| -> anyhow::Result<()> {
            let mut out = Vec::new();
            brotli::Decompressor::new(compressed.as_slice(), 4096).read_to_end(&mut out)?;
            Ok(())
        })?;
        decompress_ms_total += t.elapsed().as_millis();

        batch_start = batch_end;
    }

    Ok(CompressorResult {
        compressed_bytes: compressed_bytes_total,
        compress_ms: compress_ms_total,
        decompress_ms: decompress_ms_total,
        ratio: ratio(total_bytes, compressed_bytes_total),
    })
}

fn ratio(total: u64, compressed: u64) -> f64 {
    if compressed == 0 { 0.0 } else { total as f64 / compressed as f64 }
}

/// Create a temp file path in `dir` (not the system temp dir) so large benchmark
/// packages land on the same drive as the output report and don't exhaust C:.
fn temp_path_in(dir: &Path, prefix: &str, ext: &str) -> anyhow::Result<std::path::PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_micros();
    let filename = if ext.is_empty() {
        format!("{}-{}", prefix, ts)
    } else {
        format!("{}-{}.{}", prefix, ts, ext)
    };
    Ok(dir.join(filename))
}
