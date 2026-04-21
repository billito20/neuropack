//! Incremental rebuild support.
//!
//! A `.npmanifest` sidecar is written next to every `.neuropack` output.
//! On the next `compress --incremental` run NeuroPack reads it to decide
//! which source files changed:
//!
//! * **Unchanged** (mtime + size match, or XXH3 hash matches) → compressed
//!   bytes copied verbatim from the old package; zero re-compression work.
//! * **Changed / new** → compressed normally by the full v2 pipeline.
//! * **Deleted** → omitted from the new package automatically.
//!
//! # Sidecar path
//!
//! Given `output.neuropack` → sidecar is `output.npmanifest`.
//!
//! # Stability
//!
//! The manifest format is bincode-serialized.  It is considered an opaque
//! build artifact: delete it to force a full rebuild.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::asset_scanner::AssetMetadata;
use crate::format::AssetIndexEntry;

// ── Manifest types ─────────────────────────────────────────────────────────

/// Per-file record stored in the build manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Schema version for forward-compatibility.  Current value: 1.
    /// If a future release adds fields, old manifests (version < current) are
    /// silently ignored (load_for falls back to empty manifest → full rebuild).
    pub version: u8,
    /// Nanoseconds since UNIX epoch at last modification (0 when unavailable).
    /// Using nanoseconds (NTFS: 100 ns resolution) avoids the 1-second
    /// granularity of FAT32 / `as_secs()` incorrectly treating fast sequential
    /// writes as unchanged.
    pub mtime_nanos: u128,
    /// File size in bytes.
    pub file_size: u64,
    /// XXH3-64 content fingerprint of the raw (uncompressed) file bytes.
    pub content_hash: u64,
    /// The index entry that was written for this file.  Stored so we can
    /// replay it into the new package with remapped body offsets.
    pub index_entry: AssetIndexEntry,
}

/// Sidecar manifest produced alongside every `.neuropack` output.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BuildManifest {
    /// Keyed by `relative_path.to_string_lossy()`.
    pub entries: HashMap<String, ManifestEntry>,
}

impl BuildManifest {
    /// Load the sidecar for `package_path`, or return an empty manifest.
    pub fn load_for(package_path: &Path) -> Self {
        let p = sidecar_path(package_path);
        std::fs::read(&p)
            .ok()
            .and_then(|b| bincode::deserialize::<Self>(&b).ok())
            .unwrap_or_default()
    }

    /// Persist this manifest as the sidecar for `package_path`.
    pub fn save_for(&self, package_path: &Path) -> anyhow::Result<()> {
        let bytes = bincode::serialize(self)?;
        std::fs::write(sidecar_path(package_path), bytes)?;
        Ok(())
    }

    /// Look up the manifest entry for a source file by its relative path.
    pub fn get(&self, relative_path: &Path) -> Option<&ManifestEntry> {
        self.entries.get(&relative_path.to_string_lossy().into_owned())
    }

    /// Record an entry.
    pub fn insert(&mut self, relative_path: &Path, entry: ManifestEntry) {
        self.entries
            .insert(relative_path.to_string_lossy().into_owned(), entry);
    }
}

/// Path of the `.npmanifest` sidecar for `package_path`.
pub fn sidecar_path(package_path: &Path) -> PathBuf {
    package_path.with_extension("npmanifest")
}

// ── Change detection ───────────────────────────────────────────────────────

/// `true` if `asset` on disk is byte-for-byte identical to the recorded entry.
///
/// Primary check: XXH3 content hash (already computed by `AssetScanner` — zero
/// extra I/O). Size is an additional sanity check.  Mtime is NOT used as a
/// fast-path skip because nanosecond mtime may collide on fast sequential
/// writes in tests and CI, producing false "unchanged" results.
pub fn asset_unchanged(asset: &AssetMetadata, entry: &ManifestEntry) -> bool {
    asset.size == entry.file_size && asset.hash == entry.content_hash
}

/// Build a `ManifestEntry` from a freshly scanned asset and its index entry.
pub fn make_manifest_entry(asset: &AssetMetadata, index_entry: AssetIndexEntry) -> ManifestEntry {
    ManifestEntry {
        version: 1,
        mtime_nanos: mtime_nanos(&asset.path).unwrap_or(0),
        file_size: asset.size,
        content_hash: asset.hash,
        index_entry,
    }
}

// ── Body copy helper ───────────────────────────────────────────────────────

/// Copy `length` bytes starting at `src_abs_offset` from `src` into `dst`.
///
/// Used to transplant compressed body bytes from an old package into a new
/// one without decompressing and re-compressing.
pub fn copy_body_bytes(
    src: &mut File,
    src_abs_offset: u64,
    length: u64,
    dst: &mut BufWriter<File>,
) -> anyhow::Result<()> {
    src.seek(SeekFrom::Start(src_abs_offset))?;
    let buf_cap = (length.min(256 * 1024)) as usize;
    let mut buf = vec![0u8; buf_cap];
    let mut remaining = length;
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        src.read_exact(&mut buf[..to_read])?;
        dst.write_all(&buf[..to_read])?;
        remaining -= to_read as u64;
    }
    Ok(())
}

// ── Internal ───────────────────────────────────────────────────────────────

fn mtime_nanos(path: &Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

// ── Incremental stats returned to the caller ───────────────────────────────

/// Summary of an incremental build pass.
pub struct IncrementalStats {
    pub total: usize,
    pub reused: usize,
    pub recompressed: usize,
    pub deleted: usize,
}
