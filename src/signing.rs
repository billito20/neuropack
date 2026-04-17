//! Ed25519 package signing and verification.
//!
//! # Wire format
//!
//! A signed `.neuropack` file has a 104-byte block appended after the index:
//!
//! ```text
//! bytes  0..7   "NPSIG1\0\0"  (8-byte magic)
//! bytes  8..39  verifying_key  (32-byte Ed25519 public key, compressed)
//! bytes 40..103 signature      (64-byte Ed25519 signature)
//! ```
//!
//! The signature covers the **SHA-256 digest** of every byte in the file
//! before this block.  Streaming hashing means signing a 100 GB package
//! never requires more than a small read buffer.
//!
//! Unsigned packages open and extract normally; the block is only checked
//! when `verify_package_signature` is explicitly called.
//!
//! # Key files
//!
//! * **`*.npkey`** – 32-byte raw Ed25519 seed (keep private; anyone with
//!   this file can sign packages).
//! * **`*.nppub`** – 32-byte raw Ed25519 public key (distribute freely;
//!   embed in the game binary for verification).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

// ── Constants ──────────────────────────────────────────────────────────────

const SIG_MAGIC: &[u8; 8] = b"NPSIG1\x00\x00";
/// Total byte length of the trailing signature block.
pub const BLOCK_LEN: usize = 8 + 32 + 64; // magic + pubkey + sig = 104

// ── Key management ─────────────────────────────────────────────────────────

/// Generate a fresh Ed25519 key pair using the OS CSPRNG.
pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    (sk, vk)
}

/// Write a signing key (32-byte raw seed) to `path`.
pub fn save_signing_key(path: &Path, key: &SigningKey) -> anyhow::Result<()> {
    std::fs::write(path, key.to_bytes())?;
    Ok(())
}

/// Write a verifying key (32-byte compressed public key) to `path`.
pub fn save_verifying_key(path: &Path, key: &VerifyingKey) -> anyhow::Result<()> {
    std::fs::write(path, key.to_bytes())?;
    Ok(())
}

/// Load a signing key from a 32-byte seed file.
pub fn load_signing_key(path: &Path) -> anyhow::Result<SigningKey> {
    let bytes = std::fs::read(path)?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be exactly 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Load a verifying key from a 32-byte public-key file.
pub fn load_verifying_key(path: &Path) -> anyhow::Result<VerifyingKey> {
    let bytes = std::fs::read(path)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("verifying key must be exactly 32 bytes"))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow::anyhow!("invalid verifying key: {e}"))
}

// ── Sign / verify ──────────────────────────────────────────────────────────

/// Append a 104-byte NPSIG block to `path`, signing with `signing_key`.
///
/// If the file already has a signature block it is replaced.
/// The file is modified in-place; on error the file may have been truncated
/// but will never contain a partial/invalid signature block.
pub fn sign_package(path: &Path, signing_key: &SigningKey) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;

    let file_len = file.metadata()?.len();

    // Strip any existing signature so we hash exactly the package bytes.
    let content_len = unsigned_content_len(&mut file, file_len)?;
    if content_len < file_len {
        file.set_len(content_len)?;
    }

    // Stream-hash the package bytes.
    let digest = sha256_prefix(&mut file, content_len)?;
    let sig: Signature = signing_key.sign(&digest);
    let vk = signing_key.verifying_key();

    // Append block.
    file.seek(SeekFrom::End(0))?;
    file.write_all(SIG_MAGIC)?;
    file.write_all(&vk.to_bytes())?;
    file.write_all(&sig.to_bytes())?;
    file.flush()?;
    Ok(())
}

/// Verify the signature on `path`.
///
/// Returns the embedded verifying key on success.
/// Returns `Err` if no block is present, if the block is malformed, or if
/// the signature does not match the package content.
pub fn verify_package_signature(path: &Path) -> anyhow::Result<VerifyingKey> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();

    if (file_len as usize) < BLOCK_LEN {
        anyhow::bail!("package is too small to carry a signature block");
    }

    // Read the last BLOCK_LEN bytes.
    let mut block = [0u8; BLOCK_LEN];
    file.seek(SeekFrom::End(-(BLOCK_LEN as i64)))?;
    file.read_exact(&mut block)?;

    if &block[..8] != SIG_MAGIC {
        anyhow::bail!("package is not signed (missing NPSIG1 magic)");
    }

    let pubkey_bytes: [u8; 32] = block[8..40].try_into().unwrap();
    let sig_bytes: [u8; 64] = block[40..104].try_into().unwrap();

    let vk = VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|e| anyhow::anyhow!("embedded public key is invalid: {e}"))?;
    let sig = Signature::from_bytes(&sig_bytes);

    let content_len = file_len - BLOCK_LEN as u64;
    let digest = sha256_prefix(&mut file, content_len)?;

    vk.verify(&digest, &sig).map_err(|_| {
        anyhow::anyhow!(
            "signature verification FAILED — package may be tampered or key is wrong"
        )
    })?;

    Ok(vk)
}

/// Return `true` if `path` ends with a NPSIG1 magic block (does not verify).
pub fn is_signed(path: &Path) -> anyhow::Result<bool> {
    let file_len = std::fs::metadata(path)?.len();
    if (file_len as usize) < BLOCK_LEN {
        return Ok(false);
    }
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0u8; 8];
    file.seek(SeekFrom::End(-(BLOCK_LEN as i64)))?;
    file.read_exact(&mut magic)?;
    Ok(&magic == SIG_MAGIC)
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// Compute SHA-256 of the first `len` bytes of `file` (seeks to 0 first).
fn sha256_prefix(file: &mut std::fs::File, len: u64) -> anyhow::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            anyhow::bail!("unexpected EOF while hashing package ({remaining} bytes remaining)");
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(hasher.finalize().into())
}

/// Return the byte length of the package content, stripping any trailing
/// NPSIG block.  Does not modify the file.
fn unsigned_content_len(file: &mut std::fs::File, file_len: u64) -> anyhow::Result<u64> {
    if (file_len as usize) < BLOCK_LEN {
        return Ok(file_len);
    }
    let mut magic = [0u8; 8];
    file.seek(SeekFrom::End(-(BLOCK_LEN as i64)))?;
    file.read_exact(&mut magic)?;
    if &magic == SIG_MAGIC {
        Ok(file_len - BLOCK_LEN as u64)
    } else {
        Ok(file_len)
    }
}
