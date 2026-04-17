use crate::asset_scanner::{AssetMetadata, AssetScanner, AssetType};
use crate::decompression::PackageReader;
use crate::dictionary::Dictionary;
use crate::duplicate::ExactDuplicateCluster;
use crate::format::{
    AssetChunkRef, AssetIndexEntry, PackageDictionary, PackageDictionaryV3, PackageHeader,
    PackageManifest, PreEncoding, LARGE_FILE_THRESHOLD,
};
use crate::game_optimizations::MeshCompressor;
use crate::incremental::{
    asset_unchanged, copy_body_bytes, make_manifest_entry, BuildManifest, IncrementalStats,
};
use crate::progress::ProgressToken;
use bincode::serialize;
use fastcdc::v2020::FastCDC;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use rayon::prelude::*;
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;
use zstd::stream::write::Encoder;

// ── Constants ──────────────────────────────────────────────────────────────

/// CDC chunk parameters for cross-file deduplication.
/// Larger chunks give zstd more context per chunk and eliminate the zstd-frame
/// overhead (~18 B/chunk) that dominated at the old 4 KB minimum.
/// Files smaller than CDC_MIN produce exactly one chunk (the whole file),
/// giving them the same compression quality as a single-blob compress.
const CDC_MIN: u32 =  16 * 1024; //  16 KB
const CDC_AVG: u32 =  64 * 1024; //  64 KB
const CDC_MAX: u32 = 256 * 1024; // 256 KB

/// Per-type zstd dictionary training parameters.
const DICT_SAMPLE_SIZE: usize = 8 * 1024;   // 8 KB per sample
const DICT_MAX_SAMPLES: usize = 100;          // samples per type
const DICT_MIN_SAMPLES: usize = 5;            // minimum to train
const DICT_SIZE: usize = 112_640;             // 110 KB trained dict

// ── RAII temp-file guard ───────────────────────────────────────────────────

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ── Pipeline ───────────────────────────────────────────────────────────────

pub struct Pipeline {
    pub dictionary_window: usize,
    pub min_pattern_frequency: usize,
    pub enable_mesh_delta: bool,
    pub bypass_audio_dictionary: bool,
    pub default_zstd_level: i32,
    pub texture_zstd_level: i32,
    pub audio_zstd_level: i32,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            dictionary_window: 0,
            min_pattern_frequency: 0,
            enable_mesh_delta: true,
            bypass_audio_dictionary: true,
            default_zstd_level: 3,
            texture_zstd_level: 9,
            audio_zstd_level: 1,
        }
    }
}

impl Pipeline {
    pub fn from_config(cfg: Option<&crate::config::CompressionConfig>) -> Self {
        let d = Pipeline::default();
        match cfg {
            None => d,
            Some(c) => Self {
                dictionary_window: c.dictionary_window.unwrap_or(d.dictionary_window),
                min_pattern_frequency: c
                    .min_pattern_frequency
                    .unwrap_or(d.min_pattern_frequency),
                enable_mesh_delta: c.enable_mesh_delta.unwrap_or(d.enable_mesh_delta),
                bypass_audio_dictionary: c
                    .bypass_audio_dictionary
                    .unwrap_or(d.bypass_audio_dictionary),
                default_zstd_level: c.default_zstd_level.unwrap_or(d.default_zstd_level),
                texture_zstd_level: c.texture_zstd_level.unwrap_or(d.texture_zstd_level),
                audio_zstd_level: c.audio_zstd_level.unwrap_or(d.audio_zstd_level),
            },
        }
    }

    pub fn compress_folder<P: AsRef<Path>>(&self, root: P, output: P) -> anyhow::Result<()> {
        self.compress_folder_with_progress(root.as_ref(), output.as_ref(), None)
    }

    /// Compress `root` into `output`.
    ///
    /// **Format v2 features enabled here:**
    /// - Already-compressed content stored verbatim (no zstd expansion).
    /// - Cross-file CDC chunk deduplication: identical 16 KB regions in
    ///   different files are stored once, referenced many times.
    /// - Stable sort before processing → bit-for-bit identical packages
    ///   for the same input (deterministic builds).
    /// - Atomic rename on success; partial writes never land at destination.
    pub fn compress_folder_with_progress(
        &self,
        root: &Path,
        output: &Path,
        token: Option<Arc<ProgressToken>>,
    ) -> anyhow::Result<()> {
        let scanner = AssetScanner::default();
        let mut assets = scanner.scan(root)?;

        // ── Deterministic ordering: sort by relative path before processing.
        // This guarantees identical packages for identical inputs regardless
        // of filesystem walk order (critical for build caching and CI diffs).
        assets.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

        let duplicates = ExactDuplicateCluster::find(&assets);

        let mut duplicate_map: HashMap<PathBuf, PathBuf> = HashMap::new();
        for cluster in &duplicates {
            if let Some(first) = cluster.paths.first() {
                for path in cluster.paths.iter().skip(1) {
                    duplicate_map.insert(path.clone(), first.clone());
                }
            }
        }

        let to_compress: Vec<(usize, &AssetMetadata)> = assets
            .iter()
            .enumerate()
            .filter(|(_, a)| !duplicate_map.contains_key(&a.relative_path))
            .collect();

        let total_assets = to_compress.len();
        let dup_count = assets.len() - total_assets;
        eprintln!(
            "Compressing {} assets ({} duplicates skipped) ...",
            total_assets, dup_count
        );

        if let Some(t) = &token {
            t.set_total(total_assets);
        }

        // ── Temp body file ─────────────────────────────────────────────────
        let tmp_body_path = temp_sibling_path(output, "nptmp-body")?;
        let _body_guard = TempFile(tmp_body_path.clone());

        let mut body_writer =
            BufWriter::with_capacity(4 * 1024 * 1024, File::create(&tmp_body_path)?);

        // Global chunk pool for cross-file dedup.
        // key = xxh3 of raw (uncompressed) chunk bytes
        // value = (body_offset, body_length, uncompressed_length)
        let mut chunk_pool: HashMap<u64, (u64, u64, u64)> = HashMap::new();

        // per-asset index info:
        //   Some((pre_encoding, compressed_offset, compressed_length, Vec<chunks>, is_stored_raw))
        //   - large files: chunks is empty, compressed_offset/length is set
        //   - small v2 files: chunks is non-empty, compressed_offset=0
        let mut body_info: HashMap<
            usize,
            (PreEncoding, u64, u64, Vec<AssetChunkRef>, bool),
        > = HashMap::new();

        let mut current_offset = 0u64;
        let mut total_compressed = 0u64;
        let mut done_assets = 0usize;
        let mut dedup_hits = 0usize;
        let mut dedup_bytes_saved = 0u64;
        let mut unique_chunk_count = 0usize;

        // ── Phase 1: large files (sequential streaming, unchanged) ────────
        for &(idx, asset) in &to_compress {
            if asset.size < LARGE_FILE_THRESHOLD {
                continue;
            }
            if token.as_ref().map_or(false, |t| t.is_cancelled()) {
                anyhow::bail!("cancelled");
            }

            done_assets += 1;
            eprintln!(
                "[{}/{}] {} ({:.0} MB) streaming ...",
                done_assets, total_assets,
                asset.relative_path.display(),
                asset.size as f64 / 1_048_576.0,
            );

            let level = self.zstd_level(&asset.asset_type);
            let file = File::open(&asset.path)?;
            let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
            let mut cw = CountingWriter { inner: &mut body_writer, count: 0 };
            let mut enc = Encoder::new(&mut cw, level)?;
            std::io::copy(&mut reader, &mut enc)?;
            enc.finish()?;
            let compressed_len = cw.count;

            body_info.insert(idx, (PreEncoding::None, current_offset, compressed_len, Vec::new(), false));
            current_offset += compressed_len;
            total_compressed += compressed_len;

            if let Some(t) = &token {
                t.advance();
            }
        }

        // ── Per-type zstd dictionary training ─────────────────────────────
        // Pass 1 (read-only): collect a small sample from each non-compressed
        // small file, train one zstd dictionary per asset type, then use those
        // dictionaries during the body-write pass below.
        // Each file is only read up to DICT_SAMPLE_SIZE bytes, so overhead is
        // negligible even for large corpora.
        let (texture_zstd_dict, mesh_zstd_dict, other_zstd_dict) = {
            let mut texture_samples: Vec<Vec<u8>> = Vec::with_capacity(DICT_MAX_SAMPLES);
            let mut mesh_samples: Vec<Vec<u8>> = Vec::with_capacity(DICT_MAX_SAMPLES);
            let mut other_samples: Vec<Vec<u8>> = Vec::with_capacity(DICT_MAX_SAMPLES);

            for &(_, asset) in &to_compress {
                if asset.size >= LARGE_FILE_THRESHOLD {
                    continue;
                }
                if is_already_compressed(&asset.asset_type, &asset.path) {
                    continue;
                }
                let need_more = match &asset.asset_type {
                    AssetType::Texture => texture_samples.len() < DICT_MAX_SAMPLES,
                    AssetType::Mesh    => mesh_samples.len() < DICT_MAX_SAMPLES,
                    _                  => other_samples.len() < DICT_MAX_SAMPLES,
                };
                if !need_more {
                    continue;
                }
                // Read only up to DICT_SAMPLE_SIZE bytes.
                let mut f = std::fs::File::open(&asset.path)?;
                let cap = (asset.size as usize).min(DICT_SAMPLE_SIZE);
                let mut buf = vec![0u8; cap];
                use std::io::Read as _;
                f.read_exact(&mut buf)?;
                match &asset.asset_type {
                    AssetType::Texture => texture_samples.push(buf),
                    AssetType::Mesh    => mesh_samples.push(buf),
                    _                  => other_samples.push(buf),
                }
            }

            fn train(samples: &[Vec<u8>]) -> Vec<u8> {
                if samples.len() < DICT_MIN_SAMPLES {
                    return Vec::new();
                }
                zstd::dict::from_samples(samples, DICT_SIZE).unwrap_or_default()
            }

            (train(&texture_samples), train(&mesh_samples), train(&other_samples))
        };

        // ── Phase 2: small files — v2 chunk-dedup ─────────────────────────
        //
        // Two-stage design to exploit all CPU cores while keeping dedup correct:
        //
        // Stage 2a (parallel, CPU-bound): read every small file, pre-encode,
        //   CDC-chunk, and compress each new chunk.  Results are returned as a
        //   Vec of per-asset work items sorted by their original index so that
        //   Stage 2b can commit them in deterministic order.
        //
        // Stage 2b (sequential, I/O-bound): walk the work items in order and
        //   commit chunks to the body writer.  The chunk_pool lookup happens here,
        //   so dedup quality and determinism are identical to the old single-
        //   threaded loop.

        // ── Stage 2a: parallel read + compress ────────────────────────────

        // Each element: (original_index, pre_encoding, already_compressed,
        //                Vec<(hash, uncompressed_len, compressed_bytes)>)
        // `compressed_bytes` is the raw chunk data (already-compressed) or the
        // zstd-compressed output.
        let small_asset_work: Vec<anyhow::Result<(usize, PreEncoding, bool, Vec<(u64, u64, Vec<u8>)>)>> =
            to_compress
                .par_iter()
                .filter(|&&(_, asset)| asset.size < LARGE_FILE_THRESHOLD)
                .map(|&(idx, asset)| -> anyhow::Result<(usize, PreEncoding, bool, Vec<(u64, u64, Vec<u8>)>)> {
                    let raw = read_file_bytes(&asset.path)?;
                    let already_compressed = is_already_compressed(&asset.asset_type, &asset.path);
                    let (pre_encoding, processed) = if already_compressed {
                        (PreEncoding::None, raw)
                    } else {
                        // Note: pre_encode takes ownership; MeshCompressor::delta_encode_bytes
                        // is pure and safe to call from multiple threads.
                        self.pre_encode(&asset.asset_type, raw)
                    };

                    let level = self.zstd_level(&asset.asset_type);
                    let dict: &[u8] = match &asset.asset_type {
                        AssetType::Texture => &texture_zstd_dict,
                        AssetType::Mesh    => &mesh_zstd_dict,
                        _                  => &other_zstd_dict,
                    };

                    let mut chunks: Vec<(u64, u64, Vec<u8>)> = Vec::new();
                    for chunk in FastCDC::new(&processed, CDC_MIN, CDC_AVG, CDC_MAX) {
                        let chunk_data = &processed[chunk.offset..chunk.offset + chunk.length];
                        let hash = xxh3_64(chunk_data);
                        let uncompressed_len = chunk.length as u64;
                        let bytes = if already_compressed {
                            chunk_data.to_vec()
                        } else if dict.is_empty() {
                            zstd::bulk::compress(chunk_data, level)?
                        } else {
                            zstd::bulk::Compressor::with_dictionary(level, dict)?
                                .compress(chunk_data)?
                        };
                        chunks.push((hash, uncompressed_len, bytes));
                    }
                    Ok((idx, pre_encoding, already_compressed, chunks))
                })
                .collect();

        // Propagate any per-asset error from the parallel pass.
        let mut small_asset_work: Vec<(usize, PreEncoding, bool, Vec<(u64, u64, Vec<u8>)>)> =
            small_asset_work.into_iter().collect::<anyhow::Result<_>>()?;

        // Sort by original index so pool commits are deterministic (same order
        // as the old sequential loop, which iterated to_compress in path order).
        small_asset_work.sort_unstable_by_key(|(idx, _, _, _)| *idx);

        // ── Stage 2b: sequential pool commit + body write ─────────────────

        for (idx, pre_encoding, already_compressed, chunks) in small_asset_work {
            if token.as_ref().map_or(false, |t| t.is_cancelled()) {
                anyhow::bail!("cancelled");
            }

            done_assets += 1;
            if done_assets % 500 == 0 {
                eprintln!(
                    "[{}/{}] {} dedup hits so far ...",
                    done_assets, total_assets, dedup_hits
                );
            }

            let mut file_chunks: Vec<AssetChunkRef> = Vec::new();

            for (hash, uncompressed_len, bytes) in chunks {
                if let Some(&(off, body_len, _)) = chunk_pool.get(&hash) {
                    // Chunk already in body — reference it.
                    dedup_hits += 1;
                    dedup_bytes_saved += if already_compressed {
                        uncompressed_len
                    } else {
                        body_len
                    };
                    file_chunks.push(AssetChunkRef {
                        chunk_hash: hash,
                        body_offset: off,
                        body_length: body_len,
                        uncompressed_length: uncompressed_len,
                        compressed: !already_compressed,
                    });
                } else {
                    // New chunk — write to body.
                    unique_chunk_count += 1;
                    let body_offset = current_offset;
                    let body_len = bytes.len() as u64;
                    body_writer.write_all(&bytes)?;
                    chunk_pool.insert(hash, (body_offset, body_len, uncompressed_len));
                    current_offset += body_len;
                    total_compressed += body_len;

                    file_chunks.push(AssetChunkRef {
                        chunk_hash: hash,
                        body_offset,
                        body_length: body_len,
                        uncompressed_length: uncompressed_len,
                        compressed: !already_compressed,
                    });
                }
            }

            body_info.insert(idx, (pre_encoding, 0, 0, file_chunks, already_compressed));

            if let Some(t) = &token {
                t.advance();
            }
        }

        if done_assets > 0 {
            eprintln!(
                "[{}/{}] done. Dedup: {} chunk hits, {:.1} MB saved, {} unique chunks.",
                done_assets, total_assets,
                dedup_hits,
                dedup_bytes_saved as f64 / 1_048_576.0,
                unique_chunk_count,
            );
        }

        body_writer.flush()?;
        drop(body_writer);

        // ── Build index ────────────────────────────────────────────────────
        let mut index_entries: Vec<AssetIndexEntry> = Vec::with_capacity(assets.len());
        for (idx, asset) in assets.iter().enumerate() {
            if let Some(original) = duplicate_map.get(&asset.relative_path) {
                index_entries.push(AssetIndexEntry {
                    relative_path: asset.relative_path.clone(),
                    asset_type: asset.asset_type.clone(),
                    content_hash: asset.hash,
                    compressed_offset: 0,
                    compressed_length: 0,
                    uncompressed_length: asset.size,
                    duplicate_of: Some(original.clone()),
                    pre_encoding: PreEncoding::None,
                    chunks: Vec::new(),
                    is_stored_raw: false,
                });
            } else if let Some((pre_encoding, offset, len, chunks, is_stored_raw)) =
                body_info.get(&idx)
            {
                index_entries.push(AssetIndexEntry {
                    relative_path: asset.relative_path.clone(),
                    asset_type: asset.asset_type.clone(),
                    content_hash: asset.hash,
                    compressed_offset: *offset,
                    compressed_length: *len,
                    uncompressed_length: asset.size,
                    duplicate_of: None,
                    pre_encoding: pre_encoding.clone(),
                    chunks: chunks.clone(),
                    is_stored_raw: *is_stored_raw,
                });
            }
        }

        // ── Assemble final package (atomic write) ──────────────────────────
        let dictionary = Dictionary::build(
            &assets,
            self.min_pattern_frequency.max(2),
            self.dictionary_window.max(64),
        )?;

        let manifest = PackageManifest {
            asset_count: assets.len(),
            total_uncompressed_bytes: assets.iter().map(|a| a.size).sum(),
            total_compressed_bytes: total_compressed,
            created_by: "NeuroPack".to_string(),
            unique_chunk_count,
            dedup_hits,
            dedup_bytes_saved,
        };

        let metadata_bytes = serde_json::to_vec(&manifest)?;
        // v3: store pattern dictionary + trained per-type zstd dictionaries.
        let dictionary_bytes = serialize(&PackageDictionaryV3 {
            dictionary,
            texture_zstd_dict,
            mesh_zstd_dict,
            other_zstd_dict,
        })?;
        let index_bytes = serialize(&index_entries)?;

        let mut header = PackageHeader::default(); // version = 3
        header.metadata_length = metadata_bytes.len() as u64;
        header.dictionary_length = dictionary_bytes.len() as u64;
        header.index_length = index_bytes.len() as u64;
        let header_size = bincode::serialized_size(&header)?;
        header.body_offset = header_size + header.metadata_length + header.dictionary_length;
        header.index_offset = header.body_offset + current_offset;

        // Atomic: write to .nptmp sibling, rename on success.
        let tmp_out_path = temp_sibling_path(output, "nptmp")?;
        let _out_guard = TempFile(tmp_out_path.clone());

        {
            let output_file = File::create(&tmp_out_path)?;
            let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, output_file);
            writer.write_all(&serialize(&header)?)?;
            writer.write_all(&metadata_bytes)?;
            writer.write_all(&dictionary_bytes)?;

            let mut tmp_body = File::open(&tmp_body_path)?;
            std::io::copy(&mut tmp_body, &mut writer)?;
            drop(tmp_body);

            writer.write_all(&index_bytes)?;
            writer.flush()?;
        }

        std::fs::rename(&tmp_out_path, output)?;

        let ratio = if total_compressed == 0 {
            0.0f64
        } else {
            assets.iter().map(|a| a.size).sum::<u64>() as f64 / total_compressed as f64
        };
        eprintln!(
            "Wrote {} ({:.2}× ratio, {:.1} MB → {:.1} MB | dedup saved {:.1} MB)",
            output.display(),
            ratio,
            assets.iter().map(|a| a.size).sum::<u64>() as f64 / 1_048_576.0,
            total_compressed as f64 / 1_048_576.0,
            dedup_bytes_saved as f64 / 1_048_576.0,
        );

        Ok(())
    }

    // ── Incremental rebuild ───────────────────────────────────────────────────

    /// Like `compress_folder`, but skips files that have not changed since
    /// the last build.  Unchanged files have their compressed bytes copied
    /// verbatim from the previous package — no decompression or
    /// re-compression.
    ///
    /// A `.npmanifest` sidecar is written next to `output` on success.
    /// If no sidecar exists (first run, or deleted to force rebuild) this
    /// falls back to a full build and produces a new sidecar.
    pub fn compress_folder_incremental(
        &self,
        root: &Path,
        output: &Path,
        token: Option<Arc<ProgressToken>>,
    ) -> anyhow::Result<IncrementalStats> {
        // Load old manifest + open old package for byte copying.
        let old_manifest = BuildManifest::load_for(output);
        let old_reader: Option<PackageReader> = if output.exists() {
            PackageReader::open(output).ok()
        } else {
            None
        };

        let scanner = AssetScanner::default();
        let mut assets = scanner.scan(root)?;
        assets.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

        let duplicates = ExactDuplicateCluster::find(&assets);
        let mut duplicate_map: HashMap<PathBuf, PathBuf> = HashMap::new();
        for cluster in &duplicates {
            if let Some(first) = cluster.paths.first() {
                for path in cluster.paths.iter().skip(1) {
                    duplicate_map.insert(path.clone(), first.clone());
                }
            }
        }

        let to_compress: Vec<(usize, &AssetMetadata)> = assets
            .iter()
            .enumerate()
            .filter(|(_, a)| !duplicate_map.contains_key(&a.relative_path))
            .collect();

        let total_assets = to_compress.len();
        if let Some(t) = &token {
            t.set_total(total_assets);
        }

        // Classify each asset.
        let mut reused = 0usize;
        let mut recompressed = 0usize;

        // Open old package file handle for raw byte copying.
        let mut old_file: Option<File> = old_reader
            .as_ref()
            .and_then(|_| File::open(output).ok());

        let tmp_body_path = temp_sibling_path(output, "nptmp-body")?;
        let _body_guard = TempFile(tmp_body_path.clone());
        let mut body_writer =
            BufWriter::with_capacity(4 * 1024 * 1024, File::create(&tmp_body_path)?);

        let mut chunk_pool: HashMap<u64, (u64, u64, u64)> = HashMap::new();
        let mut body_info: HashMap<usize, (PreEncoding, u64, u64, Vec<AssetChunkRef>, bool)> =
            HashMap::new();
        let mut current_offset = 0u64;
        let mut total_compressed = 0u64;
        let mut dedup_hits = 0usize;
        let mut dedup_bytes_saved = 0u64;
        let mut unique_chunk_count = 0usize;
        let mut new_manifest = BuildManifest::default();

        // ── Phase 1: large files ─────────────────────────────────────────────
        for &(idx, asset) in &to_compress {
            if asset.size < LARGE_FILE_THRESHOLD {
                continue;
            }
            if token.as_ref().map_or(false, |t| t.is_cancelled()) {
                anyhow::bail!("cancelled");
            }

            let old_entry = old_manifest.get(&asset.relative_path);
            let unchanged = old_entry
                .map(|e| asset_unchanged(asset, e))
                .unwrap_or(false);

            let (pre_enc, compressed_len) = if unchanged
                && old_entry.is_some()
                && old_reader.is_some()
                && old_file.is_some()
            {
                // Copy raw compressed bytes from the old package body.
                let e = old_entry.expect("checked above");
                let old_reader_ref = old_reader.as_ref().expect("checked above");
                let abs_offset =
                    old_reader_ref.header.body_offset + e.index_entry.compressed_offset;
                let len = e.index_entry.compressed_length;
                copy_body_bytes(
                    old_file.as_mut().expect("checked above"),
                    abs_offset,
                    len,
                    &mut body_writer,
                )?;
                reused += 1;
                (e.index_entry.pre_encoding.clone(), len)
            } else {
                // Re-compress.
                let level = self.zstd_level(&asset.asset_type);
                let file = File::open(&asset.path)?;
                let mut reader = BufReader::with_capacity(4 * 1024 * 1024, file);
                let mut cw = CountingWriter { inner: &mut body_writer, count: 0 };
                let mut enc = Encoder::new(&mut cw, level)?;
                std::io::copy(&mut reader, &mut enc)?;
                enc.finish()?;
                recompressed += 1;
                (PreEncoding::None, cw.count)
            };

            let new_compressed_offset = current_offset;
            body_info.insert(idx, (pre_enc, new_compressed_offset, compressed_len, Vec::new(), false));
            current_offset += compressed_len;
            total_compressed += compressed_len;

            if let Some(t) = &token { t.advance(); }
        }

        // ── Phase 2: small files ─────────────────────────────────────────────
        for &(idx, asset) in &to_compress {
            if asset.size >= LARGE_FILE_THRESHOLD {
                continue;
            }
            if token.as_ref().map_or(false, |t| t.is_cancelled()) {
                anyhow::bail!("cancelled");
            }

            let old_entry = old_manifest.get(&asset.relative_path);
            let unchanged = old_entry
                .map(|e| asset_unchanged(asset, e))
                .unwrap_or(false);

            let (pre_encoding, file_chunks, already_compressed) = if unchanged
                && old_entry.is_some()
                && old_reader.is_some()
                && old_file.is_some()
            {
                let e = old_entry.expect("checked above");
                let old_reader_ref = old_reader.as_ref().expect("checked above");
                let mut new_chunks: Vec<AssetChunkRef> = Vec::new();

                for old_chunk in &e.index_entry.chunks {
                    if let Some(&(new_off, body_len, uncompr_len)) =
                        chunk_pool.get(&old_chunk.chunk_hash)
                    {
                        // Chunk already written to new body (dedup).
                        dedup_hits += 1;
                        dedup_bytes_saved += body_len;
                        new_chunks.push(AssetChunkRef {
                            chunk_hash: old_chunk.chunk_hash,
                            body_offset: new_off,
                            body_length: body_len,
                            uncompressed_length: uncompr_len,
                            compressed: old_chunk.compressed,
                        });
                    } else {
                        // Copy chunk bytes from old package.
                        let abs = old_reader_ref.header.body_offset + old_chunk.body_offset;
                        let new_off = current_offset;
                        copy_body_bytes(
                            old_file.as_mut().expect("checked above"),
                            abs,
                            old_chunk.body_length,
                            &mut body_writer,
                        )?;
                        chunk_pool.insert(
                            old_chunk.chunk_hash,
                            (new_off, old_chunk.body_length, old_chunk.uncompressed_length),
                        );
                        current_offset += old_chunk.body_length;
                        total_compressed += old_chunk.body_length;
                        unique_chunk_count += 1;
                        new_chunks.push(AssetChunkRef {
                            chunk_hash: old_chunk.chunk_hash,
                            body_offset: new_off,
                            body_length: old_chunk.body_length,
                            uncompressed_length: old_chunk.uncompressed_length,
                            compressed: old_chunk.compressed,
                        });
                    }
                }
                reused += 1;
                (e.index_entry.pre_encoding.clone(), new_chunks, e.index_entry.is_stored_raw)
            } else {
                // Re-compress from scratch.
                let raw = read_file_bytes(&asset.path)?;
                let already_compressed = is_already_compressed(&asset.asset_type, &asset.path);
                let (pre_encoding, processed) = if already_compressed {
                    (PreEncoding::None, raw)
                } else {
                    self.pre_encode(&asset.asset_type, raw)
                };

                let level = self.zstd_level(&asset.asset_type);
                let mut file_chunks: Vec<AssetChunkRef> = Vec::new();

                for chunk in FastCDC::new(&processed, CDC_MIN, CDC_AVG, CDC_MAX) {
                    let chunk_data = &processed[chunk.offset..chunk.offset + chunk.length];
                    let hash = xxh3_64(chunk_data);
                    let uncompressed_len = chunk.length as u64;

                    if let Some(&(off, body_len, _)) = chunk_pool.get(&hash) {
                        dedup_hits += 1;
                        dedup_bytes_saved += body_len;
                        file_chunks.push(AssetChunkRef {
                            chunk_hash: hash,
                            body_offset: off,
                            body_length: body_len,
                            uncompressed_length: uncompressed_len,
                            compressed: !already_compressed,
                        });
                    } else {
                        unique_chunk_count += 1;
                        let body_offset = current_offset;
                        let body_len = if already_compressed {
                            body_writer.write_all(chunk_data)?;
                            uncompressed_len
                        } else {
                            let compressed = zstd::bulk::compress(chunk_data, level)?;
                            let len = compressed.len() as u64;
                            body_writer.write_all(&compressed)?;
                            len
                        };
                        chunk_pool.insert(hash, (body_offset, body_len, uncompressed_len));
                        current_offset += body_len;
                        total_compressed += body_len;
                        file_chunks.push(AssetChunkRef {
                            chunk_hash: hash,
                            body_offset,
                            body_length: body_len,
                            uncompressed_length: uncompressed_len,
                            compressed: !already_compressed,
                        });
                    }
                }
                recompressed += 1;
                (pre_encoding, file_chunks, already_compressed)
            };

            body_info.insert(idx, (pre_encoding, 0, 0, file_chunks, already_compressed));
            if let Some(t) = &token { t.advance(); }
        }

        body_writer.flush()?;
        drop(body_writer);
        drop(old_file);

        // ── Build index + manifest ────────────────────────────────────────────
        let mut index_entries: Vec<AssetIndexEntry> = Vec::with_capacity(assets.len());
        for (idx, asset) in assets.iter().enumerate() {
            let entry = if let Some(original) = duplicate_map.get(&asset.relative_path) {
                AssetIndexEntry {
                    relative_path: asset.relative_path.clone(),
                    asset_type: asset.asset_type.clone(),
                    content_hash: asset.hash,
                    compressed_offset: 0,
                    compressed_length: 0,
                    uncompressed_length: asset.size,
                    duplicate_of: Some(original.clone()),
                    pre_encoding: PreEncoding::None,
                    chunks: Vec::new(),
                    is_stored_raw: false,
                }
            } else if let Some((pre_encoding, offset, len, chunks, is_stored_raw)) =
                body_info.get(&idx)
            {
                AssetIndexEntry {
                    relative_path: asset.relative_path.clone(),
                    asset_type: asset.asset_type.clone(),
                    content_hash: asset.hash,
                    compressed_offset: *offset,
                    compressed_length: *len,
                    uncompressed_length: asset.size,
                    duplicate_of: None,
                    pre_encoding: pre_encoding.clone(),
                    chunks: chunks.clone(),
                    is_stored_raw: *is_stored_raw,
                }
            } else {
                continue;
            };

            // Only record non-duplicate originals in the manifest.
            if entry.duplicate_of.is_none() {
                new_manifest.insert(
                    &asset.relative_path,
                    make_manifest_entry(asset, entry.clone()),
                );
            }
            index_entries.push(entry);
        }

        // ── Assemble output ───────────────────────────────────────────────────
        let dictionary = Dictionary::build(
            &assets,
            self.min_pattern_frequency.max(2),
            self.dictionary_window.max(64),
        )?;

        let manifest_meta = PackageManifest {
            asset_count: assets.len(),
            total_uncompressed_bytes: assets.iter().map(|a| a.size).sum(),
            total_compressed_bytes: total_compressed,
            created_by: "NeuroPack".to_string(),
            unique_chunk_count,
            dedup_hits,
            dedup_bytes_saved,
        };

        let metadata_bytes = serde_json::to_vec(&manifest_meta)?;
        let dictionary_bytes = serialize(&PackageDictionary { dictionary })?;
        let index_bytes = serialize(&index_entries)?;

        let mut header = PackageHeader::default();
        header.metadata_length = metadata_bytes.len() as u64;
        header.dictionary_length = dictionary_bytes.len() as u64;
        header.index_length = index_bytes.len() as u64;
        let header_size = bincode::serialized_size(&header)?;
        header.body_offset = header_size + header.metadata_length + header.dictionary_length;
        header.index_offset = header.body_offset + current_offset;

        let tmp_out_path = temp_sibling_path(output, "nptmp")?;
        let _out_guard = TempFile(tmp_out_path.clone());
        {
            let output_file = File::create(&tmp_out_path)?;
            let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, output_file);
            writer.write_all(&serialize(&header)?)?;
            writer.write_all(&metadata_bytes)?;
            writer.write_all(&dictionary_bytes)?;
            let mut tmp_body = File::open(&tmp_body_path)?;
            std::io::copy(&mut tmp_body, &mut writer)?;
            drop(tmp_body);
            writer.write_all(&index_bytes)?;
            writer.flush()?;
        }
        std::fs::rename(&tmp_out_path, output)?;

        // Save manifest after the rename succeeds.
        new_manifest.save_for(output)?;

        let deleted = old_manifest
            .entries
            .len()
            .saturating_sub(reused + recompressed);

        eprintln!(
            "Incremental: {reused} reused, {recompressed} recompressed, {deleted} deleted → {}",
            output.display()
        );

        Ok(IncrementalStats { total: total_assets, reused, recompressed, deleted })
    }

    fn pre_encode(&self, asset_type: &AssetType, raw: Vec<u8>) -> (PreEncoding, Vec<u8>) {
        match asset_type {
            AssetType::Mesh if self.enable_mesh_delta => {
                (PreEncoding::DeltaBytes, MeshCompressor::delta_encode_bytes(&raw))
            }
            _ => (PreEncoding::None, raw),
        }
    }

    fn zstd_level(&self, asset_type: &AssetType) -> i32 {
        match asset_type {
            AssetType::Texture => self.texture_zstd_level,
            AssetType::Audio   => self.audio_zstd_level,
            _                  => self.default_zstd_level,
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Returns true if the asset is stored in an already-compressed format.
///
/// Applying zstd to BCn textures, KTX2, or compressed audio expands them
/// and wastes CPU.  These assets are stored verbatim in the package body.
fn is_already_compressed(asset_type: &AssetType, path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match asset_type {
        // Block-compressed texture formats: DDS (BC1-BC7), KTX/KTX2 (BCn/ASTC),
        // JPEG (DCT), PNG (DEFLATE).  All are already compressed.
        AssetType::Texture => matches!(
            ext.as_deref(),
            Some("dds") | Some("ktx") | Some("ktx2") | Some("jpg") | Some("jpeg") | Some("png")
        ),
        // Compressed audio codecs.  WAV is PCM → NOT already compressed.
        AssetType::Audio => matches!(
            ext.as_deref(),
            Some("ogg") | Some("mp3") | Some("flac") | Some("aac") | Some("m4a") | Some("opus")
        ),
        _ => false,
    }
}

fn read_file_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    Ok(buf)
}

fn temp_sibling_path(base: &Path, tag: &str) -> anyhow::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_micros();
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("neuropack");
    let dir = base.parent().unwrap_or(Path::new("."));
    Ok(dir.join(format!("{}-{}-{}.tmp", stem, tag, ts)))
}

struct CountingWriter<W: Write> {
    inner: W,
    count: u64,
}
impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

