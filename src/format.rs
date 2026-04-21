use crate::asset_scanner::AssetType;
use crate::dictionary::Dictionary;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Files >= this size use a streaming code path in both compression and
/// decompression: no intermediate Vec<u8>, no dictionary segmentation.
pub const LARGE_FILE_THRESHOLD: u64 = 256 * 1024 * 1024; // 256 MB

/// CDC parameters shared by all subsystems (compression, patch, dedup).
/// Keeping these identical ensures chunk hashes are comparable across builds.
pub const CDC_MIN: u32 =  16 * 1024; //  16 KB
pub const CDC_AVG: u32 =  64 * 1024; //  64 KB
pub const CDC_MAX: u32 = 256 * 1024; // 256 KB

// ── Header ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageHeader {
    pub magic: [u8; 4],
    /// 1 = legacy (segment+dictionary), 2 = CDC chunk-dedup, 3 = per-type zstd dicts.
    pub version: u16,
    pub metadata_length: u64,
    pub dictionary_length: u64,
    /// Absolute byte offset to the start of the compressed-asset body.
    pub body_offset: u64,
    /// Absolute byte offset to the index section (after the body).
    pub index_offset: u64,
    pub index_length: u64,
}

impl Default for PackageHeader {
    fn default() -> Self {
        Self {
            magic: *b"NPCK",
            version: 3,
            metadata_length: 0,
            dictionary_length: 0,
            body_offset: 0,
            index_offset: 0,
            index_length: 0,
        }
    }
}

// ── PreEncoding ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum PreEncoding {
    #[default]
    None,
    /// u32 delta encoding on raw bytes — reduces entropy in mesh vertex data.
    DeltaBytes,
}

// ── v1 AssetIndexEntry (kept for reading legacy packages) ──────────────────

/// The original v1 index entry format.  Only used when opening a v1 package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetIndexEntryV1 {
    pub relative_path: PathBuf,
    pub asset_type: AssetType,
    pub content_hash: u64,
    pub compressed_offset: u64,
    pub compressed_length: u64,
    pub uncompressed_length: u64,
    pub duplicate_of: Option<PathBuf>,
    pub pre_encoding: PreEncoding,
}

// ── v2 AssetIndexEntry ─────────────────────────────────────────────────────

/// A single content-defined chunk stored in the body of a v2 package.
///
/// Multiple index entries may reference the same chunk (cross-file dedup).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetChunkRef {
    /// XXH3 hash of the raw (uncompressed) chunk bytes.
    pub chunk_hash: u64,
    /// Byte offset of this chunk's data in the package body.
    pub body_offset: u64,
    /// Byte length of this chunk's data in the body.
    /// Equals `uncompressed_length` when `compressed` is false.
    pub body_length: u64,
    /// Original (uncompressed) chunk length.
    pub uncompressed_length: u64,
    /// True → chunk bytes in body are zstd-compressed.
    /// False → stored verbatim (used for already-compressed asset formats).
    pub compressed: bool,
}

/// Current (v2) index entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetIndexEntry {
    pub relative_path: PathBuf,
    pub asset_type: AssetType,
    /// XXH3 hash of the fully decoded asset bytes (after decompression + reverse pre-encode).
    pub content_hash: u64,

    // ── Large-file path (v1 + v2) ──────────────────────────────────────
    /// Body offset for large files (≥ LARGE_FILE_THRESHOLD).
    /// 0 when `chunks` is non-empty.
    pub compressed_offset: u64,
    /// Body length for large files.  0 when `chunks` is non-empty.
    pub compressed_length: u64,

    pub uncompressed_length: u64,
    pub duplicate_of: Option<PathBuf>,
    pub pre_encoding: PreEncoding,

    // ── Chunk-dedup path (v2 small files) ──────────────────────────────
    /// Ordered CDC chunk list.  Non-empty for v2 small files.
    /// Empty for large files (use compressed_offset/length instead).
    pub chunks: Vec<AssetChunkRef>,

    /// True when the original asset is already compressed (DDS/BCn, KTX2,
    /// OGG, MP3, …) and applying zstd would expand rather than shrink it.
    /// Chunk data is stored raw (AssetChunkRef::compressed = false).
    pub is_stored_raw: bool,
}

impl AssetIndexEntry {
    /// Convert a v1 entry to the current format, filling new fields with
    /// backward-compatible defaults.
    pub fn from_v1(v1: AssetIndexEntryV1) -> Self {
        Self {
            relative_path:     v1.relative_path,
            asset_type:        v1.asset_type,
            content_hash:      v1.content_hash,
            compressed_offset: v1.compressed_offset,
            compressed_length: v1.compressed_length,
            uncompressed_length: v1.uncompressed_length,
            duplicate_of:      v1.duplicate_of,
            pre_encoding:      v1.pre_encoding,
            chunks:            Vec::new(),
            is_stored_raw:     false,
        }
    }

    /// True when this entry uses the v2 chunk-dedup body layout.
    pub fn is_chunk_based(&self) -> bool {
        !self.chunks.is_empty()
    }
}

// ── Manifest + Dictionary ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    pub asset_count: usize,
    pub total_uncompressed_bytes: u64,
    pub total_compressed_bytes: u64,
    pub created_by: String,
    /// Number of unique chunks in the body (v2 only; 0 for v1).
    pub unique_chunk_count: usize,
    /// Number of chunk references that hit an existing chunk (dedup events).
    pub dedup_hits: usize,
    /// Bytes saved by cross-file chunk deduplication (v2 only).
    pub dedup_bytes_saved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDictionary {
    pub dictionary: Dictionary,
}

/// v3 dictionary section — extends v2 with per-type trained zstd dictionaries.
///
/// Stored in the same header region as `PackageDictionary`.  The version field
/// in `PackageHeader` selects which struct to deserialise.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PackageDictionaryV3 {
    /// Pattern dictionary kept for potential v1-path fallback reads.
    pub dictionary: Dictionary,
    /// Trained zstd dictionary for `AssetType::Texture` chunks.  Empty when
    /// there were too few texture samples to train.
    pub texture_zstd_dict: Vec<u8>,
    /// Trained zstd dictionary for `AssetType::Mesh` chunks.
    pub mesh_zstd_dict: Vec<u8>,
    /// Trained zstd dictionary for all other asset types.
    pub other_zstd_dict: Vec<u8>,
}
