use crate::asset_scanner::AssetType;
use crate::dictionary::{Dictionary, Segment};
use crate::format::{
    AssetIndexEntry, AssetIndexEntryV1, PackageDictionary, PackageDictionaryV3, PackageHeader,
    PreEncoding, LARGE_FILE_THRESHOLD,
};
use crate::game_optimizations::MeshCompressor;
use rayon::prelude::*;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::{xxh3_64, Xxh3};
use zstd::stream::read::Decoder;

const DECOMP_BATCH_BUDGET: u64 = 512 * 1024 * 1024;
const MAX_UNCOMPRESSED: u64 = 8 * 1024 * 1024 * 1024;

// ── helpers ────────────────────────────────────────────────────────────────

/// Return the right per-type zstd dictionary slice for `asset_type`.
fn pick_dict<'a>(
    asset_type: &AssetType,
    texture_dict: &'a [u8],
    mesh_dict: &'a [u8],
    other_dict: &'a [u8],
) -> &'a [u8] {
    match asset_type {
        AssetType::Texture => texture_dict,
        AssetType::Mesh    => mesh_dict,
        _                  => other_dict,
    }
}

fn safe_join(root: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    for component in rel.components() {
        match component {
            std::path::Component::Normal(_) => {}
            other => anyhow::bail!("unsafe path component {:?} in {:?}", other, rel),
        }
    }
    // Walk each component that already exists and reject symlinks.
    // This prevents a crafted archive from writing through a pre-existing
    // symlink to escape the extraction root.
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "extraction path contains symlink at {:?} — refusing to write through it",
                    current
                );
            }
        }
    }
    Ok(root.join(rel))
}

// ── PackageReader ──────────────────────────────────────────────────────────

pub struct PackageReader {
    pub header:     PackageHeader,
    pub dictionary: Dictionary,
    /// Trained zstd dictionary for Texture chunks (v3 packages only; empty otherwise).
    pub texture_zstd_dict: Vec<u8>,
    /// Trained zstd dictionary for Mesh chunks (v3 packages only; empty otherwise).
    pub mesh_zstd_dict:    Vec<u8>,
    /// Trained zstd dictionary for other asset chunks (v3 packages only; empty otherwise).
    pub other_zstd_dict:   Vec<u8>,
    pub index:      Vec<AssetIndexEntry>,
    pub file:       File,
    pub path:       PathBuf,
}

impl PackageReader {
    pub fn open<P: AsRef<Path>>(package_path: P) -> anyhow::Result<Self> {
        let path = package_path.as_ref().to_path_buf();
        let file_size = path.metadata()?.len();
        let mut file = File::open(&path)?;

        let header: PackageHeader = bincode::deserialize_from(&mut file)?;
        if &header.magic != b"NPCK" {
            anyhow::bail!("not a NeuroPack file (bad magic)");
        }

        let header_size = bincode::serialized_size(&header)?;
        let expected_body = header_size + header.metadata_length + header.dictionary_length;
        if header.body_offset != expected_body {
            anyhow::bail!("corrupt header: body_offset mismatch (expected {}, got {})", expected_body, header.body_offset);
        }
        if header.index_offset < header.body_offset {
            anyhow::bail!("corrupt header: index before body");
        }
        if header.index_offset.saturating_add(header.index_length) > file_size {
            anyhow::bail!("corrupt header: index extends past EOF");
        }

        let mut metadata_bytes = vec![0u8; header.metadata_length as usize];
        file.read_exact(&mut metadata_bytes)?;

        let mut dictionary_bytes = vec![0u8; header.dictionary_length as usize];
        file.read_exact(&mut dictionary_bytes)?;

        // ── Version dispatch for dictionary section ───────────────────────
        let (dictionary, texture_zstd_dict, mesh_zstd_dict, other_zstd_dict) =
            if header.version >= 3 {
                let mut sec: PackageDictionaryV3 = bincode::deserialize(&dictionary_bytes)?;
                sec.dictionary.prepare();
                (sec.dictionary, sec.texture_zstd_dict, sec.mesh_zstd_dict, sec.other_zstd_dict)
            } else {
                let mut sec: PackageDictionary = bincode::deserialize(&dictionary_bytes)?;
                sec.dictionary.prepare();
                (sec.dictionary, Vec::new(), Vec::new(), Vec::new())
            };

        file.seek(SeekFrom::Start(header.index_offset))?;
        let mut index_bytes = vec![0u8; header.index_length as usize];
        file.read_exact(&mut index_bytes)?;

        // v1 packages used AssetIndexEntryV1; convert on the fly.
        let index: Vec<AssetIndexEntry> = if header.version <= 1 {
            let old: Vec<AssetIndexEntryV1> = bincode::deserialize(&index_bytes)?;
            old.into_iter().map(AssetIndexEntry::from_v1).collect()
        } else {
            bincode::deserialize(&index_bytes)?
        };

        Ok(Self {
            header,
            dictionary,
            texture_zstd_dict,
            mesh_zstd_dict,
            other_zstd_dict,
            index,
            file,
            path,
        })
    }

    // ── extract_all ────────────────────────────────────────────────────────

    pub fn extract_all<P: AsRef<Path>>(
        &self,
        output_root: P,
    ) -> anyhow::Result<Vec<ExtractFailure>> {
        let output_root = output_root.as_ref();
        let mut failures: Vec<ExtractFailure> = Vec::new();

        let originals: Vec<&AssetIndexEntry> =
            self.index.iter().filter(|e| e.duplicate_of.is_none()).collect();

        let (large, small): (Vec<&AssetIndexEntry>, Vec<&AssetIndexEntry>) = originals
            .into_iter()
            .partition(|e| !e.is_chunk_based() && e.compressed_length >= LARGE_FILE_THRESHOLD);

        // Phase 1: large files, sequential streaming.
        for entry in &large {
            let out_path = match safe_join(output_root, &entry.relative_path) {
                Ok(p) => p,
                Err(e) => { failures.push(fail(entry, e)); continue; }
            };
            ensure_parent(&out_path).unwrap_or_else(|e| failures.push(ExtractFailure {
                path: entry.relative_path.clone(), reason: e.to_string(),
            }));
            if let Err(e) = self.extract_large_to_file(entry, &out_path) {
                failures.push(fail(entry, e));
            }
        }

        // Phase 2: small files (both v1 segment-based and v2 chunk-based), parallel.
        let mut batch_start = 0usize;
        while batch_start < small.len() {
            let mut batch_end = batch_start;
            let mut batch_bytes = 0u64;
            while batch_end < small.len() {
                batch_bytes += small[batch_end].uncompressed_length;
                batch_end += 1;
                if batch_bytes >= DECOMP_BATCH_BUDGET { break; }
            }

            let batch = &small[batch_start..batch_end];
            let package_path = &self.path;
            let header = &self.header;
            let dictionary = &self.dictionary;
            let texture_dict = &self.texture_zstd_dict;
            let mesh_dict    = &self.mesh_zstd_dict;
            let other_dict   = &self.other_zstd_dict;

            let batch_failures: Vec<ExtractFailure> = batch
                .par_iter()
                .filter_map(|entry| -> Option<ExtractFailure> {
                    let out_path = match safe_join(output_root, &entry.relative_path) {
                        Ok(p) => p,
                        Err(e) => return Some(fail(entry, e)),
                    };
                    if let Err(e) = ensure_parent(&out_path) {
                        return Some(fail(entry, e));
                    }
                    extract_small_entry(
                        package_path, header, dictionary,
                        texture_dict, mesh_dict, other_dict,
                        entry, &out_path,
                    )
                    .err()
                    .map(|e| fail(entry, e))
                })
                .collect();

            failures.extend(batch_failures);
            batch_start = batch_end;
        }

        // Phase 3: duplicates.
        for entry in &self.index {
            if let Some(original) = &entry.duplicate_of {
                let source = match safe_join(output_root, original) {
                    Ok(p) => p,
                    Err(e) => { failures.push(ExtractFailure { path: entry.relative_path.clone(), reason: format!("bad original: {e}") }); continue; }
                };
                let target = match safe_join(output_root, &entry.relative_path) {
                    Ok(p) => p,
                    Err(e) => { failures.push(ExtractFailure { path: entry.relative_path.clone(), reason: e.to_string() }); continue; }
                };
                if let Err(e) = ensure_parent(&target) { failures.push(ExtractFailure { path: entry.relative_path.clone(), reason: e.to_string() }); continue; }
                if let Err(e) = std::fs::copy(&source, &target) {
                    failures.push(ExtractFailure { path: entry.relative_path.clone(), reason: e.to_string() });
                }
            }
        }

        Ok(failures)
    }

    // ── extract_file (single file) ─────────────────────────────────────────

    pub fn extract_file(&self, relative_path: &Path, output_dir: &Path) -> anyhow::Result<()> {
        let entry = self.index.iter().find(|e| e.relative_path == relative_path)
            .ok_or_else(|| anyhow::anyhow!("not found in package: {:?}", relative_path))?;

        let entry = if let Some(orig) = &entry.duplicate_of {
            self.index.iter().find(|e| &e.relative_path == orig)
                .ok_or_else(|| anyhow::anyhow!("original {:?} not found", orig))?
        } else { entry };

        let file_name = entry.relative_path.file_name()
            .ok_or_else(|| anyhow::anyhow!("entry has no filename"))?;
        std::fs::create_dir_all(output_dir)?;
        let dest = output_dir.join(file_name);

        if !entry.is_chunk_based() && entry.compressed_length >= LARGE_FILE_THRESHOLD {
            self.extract_large_to_file(entry, &dest)
        } else {
            let data = self.extract_asset(entry)?;
            let mut f = BufWriter::new(File::create(&dest)?);
            f.write_all(&data)?;
            Ok(())
        }
    }

    // ── extract_asset (in-memory, small files) ─────────────────────────────

    pub fn extract_asset(&self, entry: &AssetIndexEntry) -> anyhow::Result<Vec<u8>> {
        if entry.uncompressed_length > MAX_UNCOMPRESSED {
            anyhow::bail!("decompression bomb: {} claims {} bytes", entry.relative_path.display(), entry.uncompressed_length);
        }

        if entry.is_chunk_based() {
            let zstd_dict = pick_dict(
                &entry.asset_type,
                &self.texture_zstd_dict,
                &self.mesh_zstd_dict,
                &self.other_zstd_dict,
            );
            return decompress_chunks(&self.file, &self.header, entry, zstd_dict);
        }

        // v1 path: single compressed blob with segment deserialization.
        let abs_offset = self.header.body_offset + entry.compressed_offset;
        let mut file = &self.file;
        file.seek(SeekFrom::Start(abs_offset))?;
        let mut slice = vec![0u8; entry.compressed_length as usize];
        file.read_exact(&mut slice)?;
        decode_v1_entry(slice, entry, &self.dictionary)
    }

    // ── open_asset_stream — streaming reader API ───────────────────────────

    /// Return a `Read` + `Seek`-able handle to a single asset's decoded bytes
    /// without writing anything to disk.  Suitable for streaming directly into
    /// a GPU upload buffer or audio decoder at game runtime.
    ///
    /// For large files the stream is a live `zstd::Decoder` over a cloned file
    /// handle.  For chunk-based small files the chunks are decompressed eagerly
    /// into a `Cursor<Vec<u8>>` (combined size ≤ MAX_UNCOMPRESSED guard).
    pub fn open_asset_stream(
        &self,
        relative_path: &Path,
    ) -> anyhow::Result<Box<dyn Read + Send + 'static>> {
        let entry = self.index.iter().find(|e| e.relative_path == relative_path)
            .ok_or_else(|| anyhow::anyhow!("not found: {:?}", relative_path))?;

        let entry = if let Some(orig) = &entry.duplicate_of {
            self.index.iter().find(|e| &e.relative_path == orig)
                .ok_or_else(|| anyhow::anyhow!("original not found"))?
        } else { entry };

        if !entry.is_chunk_based() && entry.compressed_length >= LARGE_FILE_THRESHOLD {
            let abs_offset = self.header.body_offset + entry.compressed_offset;
            let mut src = self.file.try_clone()?;
            src.seek(SeekFrom::Start(abs_offset))?;
            let bounded = src.take(entry.compressed_length);
            let decoder = Decoder::new(bounded)?;
            return Ok(Box::new(decoder));
        }

        // Eagerly decompress into a Vec<u8> Cursor (safe: MAX_UNCOMPRESSED guard).
        let data = self.extract_asset(entry)?;
        Ok(Box::new(Cursor::new(data)))
    }

    // ── verify ─────────────────────────────────────────────────────────────

    /// Validate every non-duplicate entry's content hash in parallel.
    ///
    /// Each parallel worker opens its **own** file handle so that concurrent
    /// seeks never interfere with each other.
    pub fn verify(&self) -> anyhow::Result<VerifyReport> {
        let total_entries = self.index.iter().filter(|e| e.duplicate_of.is_none()).count();
        let pkg_path = &self.path;
        let header   = &self.header;
        let dict     = &self.dictionary;

        let texture_dict = &self.texture_zstd_dict;
        let mesh_dict    = &self.mesh_zstd_dict;
        let other_dict   = &self.other_zstd_dict;

        let results: Vec<(PathBuf, bool, Option<String>)> = self
            .index
            .par_iter()
            .filter(|e| e.duplicate_of.is_none())
            .map(|entry| {
                let result = verify_entry_standalone(
                    pkg_path, header, dict,
                    texture_dict, mesh_dict, other_dict,
                    entry,
                );
                let path = entry.relative_path.clone();
                match result {
                    Ok(()) => (path, true, None),
                    Err(e) => (path, false, Some(e.to_string())),
                }
            })
            .collect();

        let mut report = VerifyReport { total_entries, verified: 0, failed: Vec::new() };
        for (path, ok, reason) in results {
            if ok {
                report.verified += 1;
            } else {
                report.failed.push(VerifyFailure {
                    path,
                    reason: reason.unwrap_or_default(),
                });
            }
        }
        Ok(report)
    }

    fn verify_large(&self, entry: &AssetIndexEntry) -> anyhow::Result<()> {
        let abs_offset = self.header.body_offset + entry.compressed_offset;
        let mut src = self.file.try_clone()?;
        src.seek(SeekFrom::Start(abs_offset))?;
        let bounded = src.take(entry.compressed_length);
        let mut decoder = Decoder::new(bounded)?;
        let mut hasher = Xxh3Hasher::default();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        let actual = hasher.finish();
        if actual != entry.content_hash {
            anyhow::bail!("hash mismatch for {}: {:#x} != {:#x}", entry.relative_path.display(), entry.content_hash, actual);
        }
        Ok(())
    }

    // ── extract_large_to_file ──────────────────────────────────────────────

    fn extract_large_to_file(&self, entry: &AssetIndexEntry, dest: &Path) -> anyhow::Result<()> {
        let abs_offset = self.header.body_offset + entry.compressed_offset;
        let mut src = self.file.try_clone()?;
        src.seek(SeekFrom::Start(abs_offset))?;
        let bounded = src.take(entry.compressed_length);
        let mut decoder = Decoder::new(bounded)?;
        let out_file = File::create(dest)?;
        let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, out_file);
        let mut hasher = Xxh3Hasher::default();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n])?;
        }
        writer.flush()?;
        let actual = hasher.finish();
        if actual != entry.content_hash {
            anyhow::bail!("integrity fail {}: {:#x} != {:#x}", entry.relative_path.display(), entry.content_hash, actual);
        }
        Ok(())
    }

    // ── list ───────────────────────────────────────────────────────────────

    pub fn list_entries(&self) -> Vec<ListEntry> {
        self.index.iter().map(|e| {
            let is_dup = e.duplicate_of.is_some();
            let compressed_bytes = if is_dup { 0 } else {
                if e.is_chunk_based() {
                    e.chunks.iter().map(|c| c.body_length).sum()
                } else {
                    e.compressed_length
                }
            };
            let ratio = if is_dup || compressed_bytes == 0 { 0.0 }
                        else { e.uncompressed_length as f64 / compressed_bytes as f64 };
            ListEntry {
                path: e.relative_path.display().to_string(),
                asset_type: format!("{:?}", e.asset_type),
                uncompressed_bytes: e.uncompressed_length,
                compressed_bytes,
                ratio,
                is_duplicate: is_dup,
                is_stored_raw: e.is_stored_raw,
                duplicate_of: e.duplicate_of.as_ref().map(|p| p.display().to_string()),
            }
        }).collect()
    }
}

// ── free functions ─────────────────────────────────────────────────────────

/// Verify a single index entry by opening a **fresh** file handle.
/// This is the parallel-safe alternative to calling `PackageReader::extract_asset`
/// (which shares `self.file` and would race when called from rayon threads).
fn verify_entry_standalone(
    pkg_path: &Path,
    header: &PackageHeader,
    dict: &Dictionary,
    texture_dict: &[u8],
    mesh_dict: &[u8],
    other_dict: &[u8],
    entry: &AssetIndexEntry,
) -> anyhow::Result<()> {
    if !entry.is_chunk_based() && entry.compressed_length >= LARGE_FILE_THRESHOLD {
        // Large file: streaming hash via fresh handle.
        let abs_offset = header.body_offset + entry.compressed_offset;
        let mut src = File::open(pkg_path)?;
        src.seek(SeekFrom::Start(abs_offset))?;
        let bounded = src.take(entry.compressed_length);
        let mut decoder = Decoder::new(bounded)?;
        let mut hasher = Xxh3Hasher::default();
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        let actual = hasher.finish();
        if actual != entry.content_hash {
            anyhow::bail!(
                "hash mismatch for {}: expected {:#x}, got {:#x}",
                entry.relative_path.display(), entry.content_hash, actual
            );
        }
        Ok(())
    } else if entry.is_chunk_based() {
        // Chunk-based (v2/v3): open fresh handle, decompress + hash-check.
        let file = File::open(pkg_path)?;
        let zstd_dict = pick_dict(&entry.asset_type, texture_dict, mesh_dict, other_dict);
        decompress_chunks(&file, header, entry, zstd_dict).map(|_| ())
    } else {
        // v1 small file: open fresh handle, decompress, verify hash.
        let mut file = File::open(pkg_path)?;
        let abs_offset = header.body_offset + entry.compressed_offset;
        file.seek(SeekFrom::Start(abs_offset))?;
        let mut slice = vec![0u8; entry.compressed_length as usize];
        file.read_exact(&mut slice)?;
        let decoded = decode_v1_entry(slice, entry, dict)?;
        let actual = xxh3_64(&decoded);
        if actual != entry.content_hash {
            anyhow::bail!(
                "hash mismatch for {}: expected {:#x}, got {:#x}",
                entry.relative_path.display(), entry.content_hash, actual
            );
        }
        Ok(())
    }
}

/// Decompress a small entry from a fresh file handle (rayon-safe).
fn extract_small_entry(
    package_path: &Path,
    header: &PackageHeader,
    dictionary: &Dictionary,
    texture_dict: &[u8],
    mesh_dict: &[u8],
    other_dict: &[u8],
    entry: &AssetIndexEntry,
    dest: &Path,
) -> anyhow::Result<()> {
    if entry.uncompressed_length > MAX_UNCOMPRESSED {
        anyhow::bail!("bomb guard: {} claims {} bytes", entry.relative_path.display(), entry.uncompressed_length);
    }

    let data = if entry.is_chunk_based() {
        let file = File::open(package_path)?;
        let zstd_dict = pick_dict(&entry.asset_type, texture_dict, mesh_dict, other_dict);
        decompress_chunks(&file, header, entry, zstd_dict)?
    } else {
        let mut file = File::open(package_path)?;
        let abs_offset = header.body_offset + entry.compressed_offset;
        file.seek(SeekFrom::Start(abs_offset))?;
        let mut slice = vec![0u8; entry.compressed_length as usize];
        file.read_exact(&mut slice)?;
        decode_v1_entry(slice, entry, dictionary)?
    };

    let out_file = File::create(dest)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

/// Reassemble an asset from its CDC chunk list (v2/v3 format).
///
/// Each chunk is independently zstd-compressed (or stored raw).
/// `zstd_dict` — pass the trained dictionary for this asset type when the
/// package was built with v3 dictionaries; pass an empty slice otherwise.
/// After reassembly the pre-encoding is reversed and the full-file hash checked.
fn decompress_chunks(
    file: &File,
    header: &PackageHeader,
    entry: &AssetIndexEntry,
    zstd_dict: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let mut assembled = Vec::with_capacity(entry.uncompressed_length as usize);
    let mut file_ref = file;
    let mut assembled_so_far = 0u64;

    for chunk_ref in &entry.chunks {
        assembled_so_far = assembled_so_far.saturating_add(chunk_ref.uncompressed_length);
        if assembled_so_far > MAX_UNCOMPRESSED {
            anyhow::bail!(
                "decompression bomb guard: {} chunk data would exceed {} bytes",
                entry.relative_path.display(), MAX_UNCOMPRESSED
            );
        }
        let abs_offset = header.body_offset + chunk_ref.body_offset;
        file_ref.seek(SeekFrom::Start(abs_offset))?;
        let mut raw = vec![0u8; chunk_ref.body_length as usize];
        file_ref.read_exact(&mut raw)?;

        let chunk_bytes = if chunk_ref.compressed {
            if zstd_dict.is_empty() {
                zstd::bulk::decompress(&raw, chunk_ref.uncompressed_length as usize)?
            } else {
                zstd::bulk::Decompressor::with_dictionary(zstd_dict)?
                    .decompress(&raw, chunk_ref.uncompressed_length as usize)?
            }
        } else {
            raw
        };
        assembled.extend_from_slice(&chunk_bytes);
    }

    let data = match entry.pre_encoding {
        PreEncoding::None => assembled,
        PreEncoding::DeltaBytes => MeshCompressor::delta_decode_bytes(&assembled),
    };

    let actual = xxh3_64(&data);
    if actual != entry.content_hash {
        anyhow::bail!(
            "chunk integrity fail {}: {:#x} != {:#x}",
            entry.relative_path.display(), entry.content_hash, actual
        );
    }
    Ok(data)
}

/// Decode a v1 (or large-file fallback) entry: single zstd blob + segments.
fn decode_v1_entry(
    compressed: Vec<u8>,
    entry: &AssetIndexEntry,
    dictionary: &Dictionary,
) -> anyhow::Result<Vec<u8>> {
    let mut decoder = Decoder::new(&compressed[..])?;
    let mut payload = Vec::with_capacity(entry.uncompressed_length as usize);
    decoder.read_to_end(&mut payload)?;

    let segments: Vec<Segment> = bincode::deserialize(&payload)?;
    let reconstructed = reconstruct(&segments, dictionary)?;

    let data = match entry.pre_encoding {
        PreEncoding::None => reconstructed,
        PreEncoding::DeltaBytes => MeshCompressor::delta_decode_bytes(&reconstructed),
    };

    let actual = xxh3_64(&data);
    if actual != entry.content_hash {
        anyhow::bail!("integrity fail {}: {:#x} != {:#x}", entry.relative_path.display(), entry.content_hash, actual);
    }
    Ok(data)
}

fn reconstruct(segments: &[Segment], dictionary: &Dictionary) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    for segment in segments {
        match segment {
            Segment::Literal(bytes) => output.extend_from_slice(bytes),
            Segment::Reference(id) => {
                match dictionary.patterns.get(*id as usize) {
                    Some(pattern) => output.extend_from_slice(&pattern.bytes),
                    None => anyhow::bail!(
                        "corrupt package: segment references unknown pattern id {}", id
                    ),
                }
            }
        }
    }
    Ok(output)
}

fn ensure_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn fail(entry: &AssetIndexEntry, e: anyhow::Error) -> ExtractFailure {
    ExtractFailure { path: entry.relative_path.clone(), reason: e.to_string() }
}

// ── Rolling XXH3 hasher (no buffer accumulation) ───────────────────────────

#[derive(Default)]
struct Xxh3Hasher(Xxh3);
impl Xxh3Hasher {
    fn update(&mut self, data: &[u8]) { self.0.update(data); }
    fn finish(&self) -> u64 { self.0.digest() }
}

// ── Public types ───────────────────────────────────────────────────────────

pub struct VerifyReport {
    pub total_entries: usize,
    pub verified: usize,
    pub failed: Vec<VerifyFailure>,
}

#[derive(Debug)]
pub struct VerifyFailure {
    pub path:   PathBuf,
    pub reason: String,
}

pub struct ExtractFailure {
    pub path:   PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListEntry {
    pub path:               String,
    pub asset_type:         String,
    pub uncompressed_bytes: u64,
    pub compressed_bytes:   u64,
    pub ratio:              f64,
    pub is_duplicate:       bool,
    pub is_stored_raw:      bool,
    pub duplicate_of:       Option<String>,
}
