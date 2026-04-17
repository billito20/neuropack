use rayon::prelude::*;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use xxhash_rust::xxh3::Xxh3;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AssetType {
    Texture,
    Mesh,
    Audio,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetMetadata {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub asset_type: AssetType,
    pub size: u64,
    pub hash: u64,
}

#[derive(Default)]
pub struct AssetScanner;

impl AssetScanner {
    pub fn scan<P: AsRef<Path>>(&self, root: P) -> anyhow::Result<Vec<AssetMetadata>> {
        let root = root.as_ref();
        let entries = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .collect::<Vec<_>>();

        let assets: Vec<AssetMetadata> = entries
            .into_par_iter()
            .filter_map(|entry| {
                let path = entry.path().to_path_buf();
                match Self::scan_file(root, &path) {
                    Ok(metadata) => Some(metadata),
                    Err(err) => {
                        eprintln!("Warning: failed to scan {}: {}", path.display(), err);
                        None
                    }
                }
            })
            .collect();

        Ok(assets)
    }

    fn scan_file(root: &Path, path: &Path) -> anyhow::Result<AssetMetadata> {
        let metadata = path.metadata()?;
        let size = metadata.len();
        let relative_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let asset_type = classify_asset_type(path);
        let hash = compute_xxh3(path)?;

        Ok(AssetMetadata {
            path: path.to_path_buf(),
            relative_path,
            asset_type,
            size,
            hash,
        })
    }
}

fn compute_xxh3(path: &Path) -> anyhow::Result<u64> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    let mut hasher = Xxh3::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hasher.digest())
}

fn classify_asset_type(path: &Path) -> AssetType {
    // Try magic bytes first — more reliable than extensions.
    if let Ok(magic) = read_magic(path) {
        if let Some(t) = detect_by_magic(&magic) {
            return t;
        }
    }
    // Fall back to extension.
    let ext = path.extension().and_then(OsStr::to_str);
    let ext = ext.map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") | Some("jpg") | Some("jpeg") | Some("tga") | Some("dds")
        | Some("ktx") | Some("ktx2") | Some("hdr") | Some("exr") => AssetType::Texture,
        Some("fbx") | Some("obj") | Some("gltf") | Some("glb") | Some("mesh")
        | Some("dae") | Some("blend") => AssetType::Mesh,
        Some("wav") | Some("ogg") | Some("mp3") | Some("flac") | Some("aac")
        | Some("m4a") | Some("opus") => AssetType::Audio,
        _ => AssetType::Unknown,
    }
}

fn read_magic(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut buf = [0u8; 16];
    let mut f = File::open(path)?;
    let n = f.read(&mut buf)?;
    // Zero-pad if file is shorter than 16 bytes.
    buf[n..].fill(0);
    Ok(buf)
}

fn detect_by_magic(magic: &[u8; 16]) -> Option<AssetType> {
    match magic {
        // Textures
        m if m.starts_with(b"\x89PNG\r\n\x1a\n") => Some(AssetType::Texture),
        m if m.starts_with(b"\xff\xd8\xff") => Some(AssetType::Texture),       // JPEG
        m if m.starts_with(b"DDS ") => Some(AssetType::Texture),               // DDS
        m if m.starts_with(b"\xabKTX 20\xbb\r\n\x1a\n") => Some(AssetType::Texture), // KTX2
        m if m.starts_with(b"\xabKTX 11\xbb\r\n\x1a\n") => Some(AssetType::Texture), // KTX1
        // Meshes
        m if m.starts_with(b"glTF") => Some(AssetType::Mesh),                  // GLB
        // Audio
        m if m.starts_with(b"OggS") => Some(AssetType::Audio),                 // OGG/Opus
        m if m.starts_with(b"fLaC") => Some(AssetType::Audio),                 // FLAC
        m if m.starts_with(b"RIFF") && &m[8..12] == b"WAVE" => Some(AssetType::Audio), // WAV
        m if m.starts_with(b"ID3") || (m[0] == 0xff && m[1] & 0xe0 == 0xe0) => {
            Some(AssetType::Audio) // MP3
        }
        _ => None,
    }
}
