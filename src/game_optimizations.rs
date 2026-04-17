#![allow(dead_code)]
use crate::classifier::TextureRole;
use std::path::Path;

pub struct MeshCompressor;

impl MeshCompressor {
    /// Classify texture role by filename convention.
    pub fn classify_texture_role(path: &Path) -> TextureRole {
        crate::classifier::AssetClassifier::texture_role(path)
    }

    /// Delta-encode a typed vertex buffer (f32 slice).
    /// For use when the caller has already parsed vertex positions out of a mesh.
    pub fn delta_encode_vertices(vertices: &[f32]) -> Vec<i32> {
        let mut deltas = Vec::with_capacity(vertices.len());
        let mut last = 0i32;
        for &value in vertices {
            let encoded = (value * 1024.0) as i32;
            deltas.push(encoded - last);
            last = encoded;
        }
        deltas
    }

    /// Reconstruct original vertex buffer from a delta-encoded f32 slice.
    pub fn delta_decode_vertices(deltas: &[i32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(deltas.len());
        let mut acc = 0i32;
        for &d in deltas {
            acc += d;
            out.push(acc as f32 / 1024.0);
        }
        out
    }

    /// Delta-encode raw bytes by treating them as a stream of little-endian u32 words.
    /// Applied to whole binary mesh files before compression — improves zstd ratio
    /// on vertex-heavy formats by reducing the entropy of adjacent similar values.
    pub fn delta_encode_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut last: u32 = 0;
        let mut i = 0;
        while i + 4 <= data.len() {
            let val = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let delta = val.wrapping_sub(last);
            out.extend_from_slice(&delta.to_le_bytes());
            last = val;
            i += 4;
        }
        // Remainder bytes that don't form a full u32 — copy verbatim.
        out.extend_from_slice(&data[i..]);
        out
    }

    /// Reverse of `delta_encode_bytes`.
    pub fn delta_decode_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut acc: u32 = 0;
        let mut i = 0;
        while i + 4 <= data.len() {
            let delta = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            acc = acc.wrapping_add(delta);
            out.extend_from_slice(&acc.to_le_bytes());
            i += 4;
        }
        out.extend_from_slice(&data[i..]);
        out
    }
}
