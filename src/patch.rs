use crate::asset_scanner::AssetScanner;
use crate::format::{CDC_AVG, CDC_MAX, CDC_MIN, LARGE_FILE_THRESHOLD};
use fastcdc::v2020::{FastCDC, StreamCDC};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

// ── RAII temp-file guard ───────────────────────────────────────────────────

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// ── On-disk format ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// Version-2 patch entry: per-file chunk list produced by CDC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcPatchEntry {
    pub relative_path: PathBuf,
    pub kind: ChangeKind,
    /// XXH3 of the fully assembled new file (None for Removed).
    pub new_hash: Option<u64>,
    /// Ordered list of chunks that compose the new file.
    pub chunks: Vec<CdcChunkRef>,
    /// CDC parameters used — must match when applying.
    pub cdc_min: u32,
    pub cdc_avg: u32,
    pub cdc_max: u32,
}

/// One content-defined chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcChunkRef {
    /// XXH3 of the raw (uncompressed) chunk bytes.
    pub hash: u64,
    /// Byte offset of this chunk's compressed data in the patch body.
    /// `u64::MAX` means the chunk is unchanged — read it from the old file.
    pub body_offset: u64,
    /// Compressed byte length in the patch body.  0 if unchanged.
    pub body_length: u64,
    /// Original (uncompressed) chunk length.
    pub uncompressed_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchManifest {
    pub format_version: u16,
    pub source_path: String,
    pub target_path: String,
    pub entries: Vec<CdcPatchEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PatchHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub manifest_length: u64,
    pub body_offset: u64,
}

impl PatchHeader {
    fn new() -> Self {
        Self { magic: *b"NPPK", version: 2, manifest_length: 0, body_offset: 0 }
    }
}

// ── Builder ────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct PatchBuilder;

impl PatchBuilder {
    /// Build a CDC binary-diff patch from `old_root` → `new_root`.
    ///
    /// Only chunks absent from the old version are stored in the patch body.
    /// Files ≥ LARGE_FILE_THRESHOLD are processed with `StreamCDC` so their
    /// content never accumulates fully in RAM.
    pub fn build<P: AsRef<Path>>(
        &self,
        old_root: P,
        new_root: P,
        output: P,
    ) -> anyhow::Result<()> {
        let old_root = old_root.as_ref();
        let new_root = new_root.as_ref();
        let output = output.as_ref();

        let scanner = AssetScanner::default();
        let old_assets = scanner.scan(old_root)?;
        let new_assets = scanner.scan(new_root)?;

        let old_map: HashMap<PathBuf, &crate::asset_scanner::AssetMetadata> =
            old_assets.iter().map(|a| (a.relative_path.clone(), a)).collect();
        let new_map: HashMap<PathBuf, &crate::asset_scanner::AssetMetadata> =
            new_assets.iter().map(|a| (a.relative_path.clone(), a)).collect();

        // Phase 1: Collect all chunk hashes present in the old version.
        eprintln!("Scanning old version for existing chunks ...");
        let old_chunk_hashes = collect_chunk_hashes(&old_assets)?;
        eprintln!("  {} unique chunks in old version", old_chunk_hashes.len());

        // Phase 2: Build patch entries.
        let tmp_body_path = temp_path("neuropack-patch-body")?;
        let _body_guard = TempFile(tmp_body_path.clone()); // cleaned up on any exit

        let mut body_writer = BufWriter::with_capacity(4 * 1024 * 1024, File::create(&tmp_body_path)?);
        let mut current_offset = 0u64;
        let mut entries: Vec<CdcPatchEntry> = Vec::new();

        // Added and modified files.
        for asset in &new_assets {
            let is_unchanged = old_map
                .get(&asset.relative_path)
                .map(|old| old.hash == asset.hash)
                .unwrap_or(false);
            if is_unchanged {
                continue;
            }
            let kind = if old_map.contains_key(&asset.relative_path) {
                ChangeKind::Modified
            } else {
                ChangeKind::Added
            };

            let chunks = if asset.size >= LARGE_FILE_THRESHOLD {
                build_chunk_refs_streaming(
                    &asset.path,
                    &old_chunk_hashes,
                    &mut body_writer,
                    &mut current_offset,
                )?
            } else {
                let raw = read_file_bytes(&asset.path)?;
                build_chunk_refs(&raw, &old_chunk_hashes, &mut body_writer, &mut current_offset)?
            };

            entries.push(CdcPatchEntry {
                relative_path: asset.relative_path.clone(),
                kind,
                new_hash: Some(asset.hash),
                chunks,
                cdc_min: CDC_MIN,
                cdc_avg: CDC_AVG,
                cdc_max: CDC_MAX,
            });
        }

        // Removed files.
        for asset in &old_assets {
            if !new_map.contains_key(&asset.relative_path) {
                entries.push(CdcPatchEntry {
                    relative_path: asset.relative_path.clone(),
                    kind: ChangeKind::Removed,
                    new_hash: None,
                    chunks: Vec::new(),
                    cdc_min: CDC_MIN,
                    cdc_avg: CDC_AVG,
                    cdc_max: CDC_MAX,
                });
            }
        }

        body_writer.flush()?;
        drop(body_writer);

        // Phase 3: Write patch file (atomic: write to .tmp, rename on success).
        let tmp_out_path = {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_micros();
            let stem = output
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("patch");
            let dir = output.parent().unwrap_or(Path::new("."));
            dir.join(format!("{}-{}.nptmp", stem, ts))
        };
        let _out_guard = TempFile(tmp_out_path.clone());

        let manifest = PatchManifest {
            format_version: 2,
            source_path: old_root.display().to_string(),
            target_path: new_root.display().to_string(),
            entries,
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

        let mut header = PatchHeader::new();
        header.manifest_length = manifest_bytes.len() as u64;
        let header_size = bincode::serialized_size(&header)?;
        header.body_offset = header_size + header.manifest_length;

        {
            let out = File::create(&tmp_out_path)?;
            let mut w = BufWriter::new(out);
            w.write_all(&bincode::serialize(&header)?)?;
            w.write_all(&manifest_bytes)?;

            let mut body_file = File::open(&tmp_body_path)?;
            std::io::copy(&mut body_file, &mut w)?;
            drop(body_file);
            w.flush()?;
        }

        fs::rename(&tmp_out_path, output)?;

        let patch_size = fs::metadata(output)?.len();
        eprintln!(
            "Patch written to {} ({:.1} MB, {} entries)",
            output.display(),
            patch_size as f64 / 1_048_576.0,
            manifest.entries.len(),
        );
        Ok(())
    }
}

// ── Applier ────────────────────────────────────────────────────────────────

pub struct PatchApplier;

impl PatchApplier {
    /// Apply a NeuroPack patch to `target_root`.
    pub fn apply<P: AsRef<Path>>(patch_path: P, target_root: P) -> anyhow::Result<()> {
        let patch_path = patch_path.as_ref();
        let target_root = target_root.as_ref();
        let mut file = File::open(patch_path)?;

        let header: PatchHeader = bincode::deserialize_from(&mut file)?;
        if &header.magic != b"NPPK" {
            anyhow::bail!("not a NeuroPack patch file");
        }

        let mut manifest_bytes = vec![0u8; header.manifest_length as usize];
        file.read_exact(&mut manifest_bytes)?;
        let manifest: PatchManifest = serde_json::from_slice(&manifest_bytes)?;

        for entry in &manifest.entries {
            let dest = target_root.join(&entry.relative_path);
            match entry.kind {
                ChangeKind::Removed => {
                    if dest.exists() {
                        fs::remove_file(&dest)?;
                    }
                }
                ChangeKind::Added | ChangeKind::Modified => {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let old_path = target_root.join(&entry.relative_path);
                    let old_chunk_map =
                        if entry.kind == ChangeKind::Modified && old_path.exists() {
                            Some(build_old_chunk_map(
                                &old_path,
                                entry.cdc_min,
                                entry.cdc_avg,
                                entry.cdc_max,
                            )?)
                        } else {
                            None
                        };

                    // Write to a sibling .nptmp, rename on success.
                    let tmp_dest = {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_micros();
                        let stem = dest
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file");
                        let dir = dest.parent().unwrap_or(Path::new("."));
                        dir.join(format!("{}-{}.nptmp", stem, ts))
                    };
                    let _dest_guard = TempFile(tmp_dest.clone());

                    {
                        let out_file = File::create(&tmp_dest)?;
                        let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, out_file);

                        for chunk_ref in &entry.chunks {
                            if chunk_ref.body_offset == u64::MAX {
                                if let Some(map) = &old_chunk_map {
                                    if let Some((off, len)) = map.get(&chunk_ref.hash) {
                                        let old_raw = read_file_range(&old_path, *off, *len)?;
                                        writer.write_all(&old_raw)?;
                                        continue;
                                    }
                                }
                                anyhow::bail!(
                                    "cannot locate unchanged chunk {:x} for {}",
                                    chunk_ref.hash,
                                    entry.relative_path.display()
                                );
                            }

                            file.seek(SeekFrom::Start(
                                header.body_offset + chunk_ref.body_offset,
                            ))?;
                            let mut compressed = vec![0u8; chunk_ref.body_length as usize];
                            file.read_exact(&mut compressed)?;
                            let raw_chunk = zstd::bulk::decompress(
                                &compressed,
                                chunk_ref.uncompressed_length as usize,
                            )?;
                            writer.write_all(&raw_chunk)?;
                        }
                        writer.flush()?;
                    }

                    // Verify assembled file hash before replacing destination.
                    if let Some(expected_hash) = entry.new_hash {
                        let actual_hash = xxh3_64(&fs::read(&tmp_dest)?);
                        if actual_hash != expected_hash {
                            anyhow::bail!(
                                "patch integrity check failed for {}: expected {:#x}, got {:#x}",
                                entry.relative_path.display(),
                                expected_hash,
                                actual_hash,
                            );
                        }
                    }

                    fs::rename(&tmp_dest, &dest)?;
                }
            }
        }

        Ok(())
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Collect all CDC chunk hashes from a set of assets.
/// Files ≥ LARGE_FILE_THRESHOLD are chunked via StreamCDC (no full-file alloc).
fn collect_chunk_hashes(
    assets: &[crate::asset_scanner::AssetMetadata],
) -> anyhow::Result<HashSet<u64>> {
    let mut hashes = HashSet::new();
    for asset in assets {
        if asset.size >= LARGE_FILE_THRESHOLD {
            let f = File::open(&asset.path)?;
            let stream = StreamCDC::new(BufReader::new(f), CDC_MIN, CDC_AVG, CDC_MAX);
            for result in stream {
                let chunk = result?;
                hashes.insert(xxh3_64(&chunk.data));
            }
        } else {
            let data = read_file_bytes(&asset.path)?;
            for chunk in FastCDC::new(&data, CDC_MIN, CDC_AVG, CDC_MAX) {
                hashes.insert(xxh3_64(&data[chunk.offset..chunk.offset + chunk.length]));
            }
        }
    }
    Ok(hashes)
}

/// Build `CdcChunkRef` list for in-memory `data`, writing new chunks to body.
fn build_chunk_refs(
    data: &[u8],
    old_hashes: &HashSet<u64>,
    body_writer: &mut impl Write,
    current_offset: &mut u64,
) -> anyhow::Result<Vec<CdcChunkRef>> {
    let mut refs = Vec::new();
    for chunk in FastCDC::new(data, CDC_MIN, CDC_AVG, CDC_MAX) {
        let chunk_data = &data[chunk.offset..chunk.offset + chunk.length];
        let hash = xxh3_64(chunk_data);
        refs.push(emit_chunk_ref(
            hash,
            chunk_data,
            chunk.length as u64,
            old_hashes,
            body_writer,
            current_offset,
        )?);
    }
    Ok(refs)
}

/// Build `CdcChunkRef` list for a large file using streaming CDC.
/// Chunks arrive one at a time; peak RAM is one chunk (≤ CDC_MAX = 64 KB).
fn build_chunk_refs_streaming(
    path: &Path,
    old_hashes: &HashSet<u64>,
    body_writer: &mut impl Write,
    current_offset: &mut u64,
) -> anyhow::Result<Vec<CdcChunkRef>> {
    let f = File::open(path)?;
    let stream = StreamCDC::new(BufReader::new(f), CDC_MIN, CDC_AVG, CDC_MAX);
    let mut refs = Vec::new();
    for result in stream {
        let chunk = result?;
        let hash = xxh3_64(&chunk.data);
        let uncompressed_length = chunk.data.len() as u64;
        refs.push(emit_chunk_ref(
            hash,
            &chunk.data,
            uncompressed_length,
            old_hashes,
            body_writer,
            current_offset,
        )?);
    }
    Ok(refs)
}

/// Common logic: decide whether to store the chunk in the patch body.
fn emit_chunk_ref(
    hash: u64,
    chunk_data: &[u8],
    uncompressed_length: u64,
    old_hashes: &HashSet<u64>,
    body_writer: &mut impl Write,
    current_offset: &mut u64,
) -> anyhow::Result<CdcChunkRef> {
    if old_hashes.contains(&hash) {
        Ok(CdcChunkRef {
            hash,
            body_offset: u64::MAX,
            body_length: 0,
            uncompressed_length,
        })
    } else {
        let compressed = zstd::bulk::compress(chunk_data, 3)?;
        let body_length = compressed.len() as u64;
        body_writer.write_all(&compressed)?;
        let body_offset = *current_offset;
        *current_offset += body_length;
        Ok(CdcChunkRef { hash, body_offset, body_length, uncompressed_length })
    }
}

/// Build a map of chunk_hash → (file_offset, chunk_length) for an existing file.
fn build_old_chunk_map(
    path: &Path,
    cdc_min: u32,
    cdc_avg: u32,
    cdc_max: u32,
) -> anyhow::Result<HashMap<u64, (u64, u64)>> {
    let data = read_file_bytes(path)?;
    let mut map = HashMap::new();
    for chunk in FastCDC::new(&data, cdc_min, cdc_avg, cdc_max) {
        let hash = xxh3_64(&data[chunk.offset..chunk.offset + chunk.length]);
        map.entry(hash).or_insert((chunk.offset as u64, chunk.length as u64));
    }
    Ok(map)
}

fn read_file_range(path: &Path, offset: u64, length: u64) -> anyhow::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; length as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_file_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let f = File::open(path)?;
    let mut r = BufReader::with_capacity(256 * 1024, f);
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    Ok(buf)
}

fn temp_path(prefix: &str) -> anyhow::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_micros();
    let mut p = std::env::temp_dir();
    p.push(format!("{}-{}.tmp", prefix, ts));
    Ok(p)
}
