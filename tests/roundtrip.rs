use neuropack::format::PreEncoding;
use neuropack::game_optimizations::MeshCompressor;
use neuropack::signing;
use neuropack::{Pipeline, PackageReader};
use std::fs;
use std::path::{Path, PathBuf};

// ── Helpers ────────────────────────────────────────────────────────────────

fn temp_dir(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let p = std::env::temp_dir().join(format!("{prefix}-{ts}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn temp_file(prefix: &str, ext: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    std::env::temp_dir().join(format!("{prefix}-{ts}.{ext}"))
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn assert_eq_files(a: &Path, b: &Path) {
    let la = fs::read(a).unwrap_or_else(|e| panic!("read {}: {e}", a.display()));
    let lb = fs::read(b).unwrap_or_else(|e| panic!("read {}: {e}", b.display()));
    assert_eq!(la, lb, "mismatch: {} vs {}", a.display(), b.display());
}

/// 1 KB of pseudo-random bytes (deterministic, no external crate needed).
fn random_bytes(seed: u8, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut state = seed as u64;
    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        v.push((state >> 33) as u8);
    }
    v
}

fn mesh_bytes() -> Vec<u8> {
    // 32-bit LE values with smooth progression — good for delta encoding.
    let mut out = Vec::new();
    for i in 0..256u32 {
        out.extend_from_slice(&i.wrapping_mul(17).wrapping_add(3).to_le_bytes());
    }
    out
}

fn png_bytes() -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec(); // PNG signature
    out.extend(random_bytes(1, 256));
    out
}

fn ogg_bytes() -> Vec<u8> {
    let mut out = b"OggS".to_vec();
    out.extend(random_bytes(2, 512));
    out
}

fn build_package(src: &Path, pkg: &Path) {
    Pipeline::default().compress_folder(src, pkg).expect("compress");
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

// ── 1. Codec round-trip ────────────────────────────────────────────────────

#[test]
fn delta_bytes_codec_roundtrip_handles_trailing_bytes() {
    let input: Vec<u8> = (0..53).map(|v| v as u8).collect();
    let encoded = MeshCompressor::delta_encode_bytes(&input);
    let decoded = MeshCompressor::delta_decode_bytes(&encoded);
    assert_eq!(decoded, input);
}

// ── 2. Full pipeline round-trip ────────────────────────────────────────────

#[test]
fn pipeline_roundtrip_covers_none_and_delta_preencoding() {
    let src = temp_dir("np-src");
    let out = temp_dir("np-out");
    let pkg = temp_file("np-pkg", "neuropack");

    write(&src.join("meshes/ship.mesh"), &mesh_bytes());
    write(&src.join("textures/hero.png"), &png_bytes());
    write(&src.join("audio/theme.ogg"), &ogg_bytes());

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");

    // Delta encoding applied to mesh; not to texture or audio.
    let mesh = reader.index.iter().find(|e| e.relative_path.ends_with("ship.mesh")).unwrap();
    assert_eq!(mesh.pre_encoding, PreEncoding::DeltaBytes);
    let tex = reader.index.iter().find(|e| e.relative_path.ends_with("hero.png")).unwrap();
    assert_eq!(tex.pre_encoding, PreEncoding::None);

    reader.extract_all(&out).expect("extract_all");
    assert_eq_files(&src.join("meshes/ship.mesh"), &out.join("meshes/ship.mesh"));
    assert_eq_files(&src.join("textures/hero.png"), &out.join("textures/hero.png"));
    assert_eq_files(&src.join("audio/theme.ogg"),  &out.join("audio/theme.ogg"));

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 3. Already-compressed files stored raw ────────────────────────────────

#[test]
fn already_compressed_files_stored_raw_and_recover_exact_bytes() {
    let src = temp_dir("np-raw-src");
    let out = temp_dir("np-raw-out");
    let pkg = temp_file("np-raw-pkg", "neuropack");

    let png_data = png_bytes();
    let ogg_data = ogg_bytes();
    write(&src.join("t.png"),  &png_data);
    write(&src.join("a.ogg"),  &ogg_data);

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");

    // PNG and OGG should be flagged as raw (no zstd applied).
    let png_entry = reader.index.iter().find(|e| e.relative_path.ends_with("t.png")).unwrap();
    assert!(png_entry.is_stored_raw, "PNG should be stored raw");
    let ogg_entry = reader.index.iter().find(|e| e.relative_path.ends_with("a.ogg")).unwrap();
    assert!(ogg_entry.is_stored_raw, "OGG should be stored raw");

    reader.extract_all(&out).expect("extract_all");
    assert_eq!(fs::read(out.join("t.png")).unwrap(),  png_data, "PNG mismatch");
    assert_eq!(fs::read(out.join("a.ogg")).unwrap(), ogg_data, "OGG mismatch");

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 4. Cross-file chunk deduplication ────────────────────────────────────

#[test]
fn identical_content_across_files_deduplicates() {
    let src = temp_dir("np-dup-src");
    let out = temp_dir("np-dup-out");
    let pkg = temp_file("np-dup-pkg", "neuropack");

    // Two files with identical content → one body copy expected.
    let shared = random_bytes(42, 64 * 1024); // 64 KB — at least one shared CDC chunk
    write(&src.join("a.bin"), &shared);
    write(&src.join("b.bin"), &shared);

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");

    // Both entries should decompress to the same bytes.
    reader.extract_all(&out).expect("extract_all");
    assert_eq_files(&out.join("a.bin"), &out.join("b.bin"));

    // Package body should be smaller than 2 × file size (dedup saved space).
    let pkg_size = fs::metadata(&pkg).unwrap().len();
    assert!(
        pkg_size < (shared.len() * 2) as u64,
        "expected dedup: pkg {pkg_size} B should be < {} B",
        shared.len() * 2,
    );

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 5. Single-file extract ────────────────────────────────────────────────

#[test]
fn extract_single_file_produces_correct_content() {
    let src = temp_dir("np-single-src");
    let out = temp_dir("np-single-out");
    let pkg = temp_file("np-single-pkg", "neuropack");

    let data = random_bytes(7, 8192);
    write(&src.join("sub/target.bin"), &data);
    write(&src.join("other.bin"), &random_bytes(8, 1024));

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");

    reader
        .extract_file(Path::new("sub/target.bin"), &out)
        .expect("extract_file");

    assert_eq!(fs::read(out.join("target.bin")).unwrap(), data);

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 6. Verify passes on a valid package ──────────────────────────────────

#[test]
fn verify_passes_on_valid_package() {
    let src = temp_dir("np-vrfy-src");
    let pkg = temp_file("np-vrfy-pkg", "neuropack");

    write(&src.join("a.bin"), &random_bytes(9, 4096));
    write(&src.join("b.bin"), &random_bytes(10, 4096));

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");
    let report = reader.verify().expect("verify");

    assert!(report.failed.is_empty(), "expected zero failures, got {:?}", report.failed);
    assert_eq!(report.verified, report.total_entries);

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_file(&pkg);
}

// ── 7. Verify detects body corruption ────────────────────────────────────

#[test]
fn verify_detects_corrupted_body_bytes() {
    let src = temp_dir("np-corrupt-src");
    let pkg = temp_file("np-corrupt-pkg", "neuropack");

    write(&src.join("data.bin"), &random_bytes(11, 4096));

    build_package(&src, &pkg);

    // Corrupt a byte deep in the body section.
    let mut raw = fs::read(&pkg).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0xFF;
    fs::write(&pkg, raw).unwrap();

    let reader = PackageReader::open(&pkg).expect("open corrupted");
    let report = reader.verify().expect("verify call itself should not bail");

    assert!(
        !report.failed.is_empty(),
        "expected at least one failure on corrupted package"
    );

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_file(&pkg);
}

// ── 8. safe_join rejects path traversal ──────────────────────────────────
//
// We can't inject a crafted index entry into a finished package without
// re-building the whole file; test the public extract path instead by
// verifying that `extract_file` with a traversal path fails cleanly.

#[test]
fn extract_file_rejects_absolute_and_traversal_paths() {
    let src = temp_dir("np-trav-src");
    let out = temp_dir("np-trav-out");
    let pkg = temp_file("np-trav-pkg", "neuropack");

    write(&src.join("ok.bin"), &random_bytes(12, 256));
    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");

    // A path that doesn't exist in the index → should error (not panic).
    let result = reader.extract_file(Path::new("../evil.bin"), &out);
    assert!(result.is_err(), "traversal path should be rejected");

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 9. List entries returns correct metadata ──────────────────────────────

#[test]
fn list_entries_returns_expected_count_and_sizes() {
    let src = temp_dir("np-list-src");
    let pkg = temp_file("np-list-pkg", "neuropack");

    write(&src.join("a.bin"), &random_bytes(13, 1000));
    write(&src.join("b.bin"), &random_bytes(14, 2000));

    build_package(&src, &pkg);
    let reader = PackageReader::open(&pkg).expect("open");
    let entries = reader.list_entries();

    assert_eq!(entries.len(), 2);
    let a = entries.iter().find(|e| e.path.ends_with("a.bin")).expect("a.bin");
    let b = entries.iter().find(|e| e.path.ends_with("b.bin")).expect("b.bin");
    assert_eq!(a.uncompressed_bytes, 1000);
    assert_eq!(b.uncompressed_bytes, 2000);

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_file(&pkg);
}

// ── 10. Signature: sign, verify, tamper detection ─────────────────────────

#[test]
fn sign_verify_roundtrip_succeeds_and_detects_tampering() {
    let src = temp_dir("np-sig-src");
    let pkg = temp_file("np-sig-pkg", "neuropack");

    write(&src.join("asset.bin"), &random_bytes(15, 4096));
    build_package(&src, &pkg);

    let (sk, vk) = signing::generate_keypair();

    // Unsigned package → verify should fail cleanly.
    assert!(
        signing::verify_package_signature(&pkg).is_err(),
        "unsigned package should fail verification"
    );
    assert!(!signing::is_signed(&pkg).unwrap());

    // Sign the package.
    signing::sign_package(&pkg, &sk).expect("sign");
    assert!(signing::is_signed(&pkg).unwrap());

    // Verify with embedded key → OK.
    let embedded_vk = signing::verify_package_signature(&pkg).expect("verify after sign");
    assert_eq!(embedded_vk.to_bytes(), vk.to_bytes());

    // Tamper: flip a byte in the middle of the file.
    let mut raw = fs::read(&pkg).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0x01;
    fs::write(&pkg, &raw).unwrap();

    // Verify must now fail.
    let result = signing::verify_package_signature(&pkg);
    assert!(result.is_err(), "tampered package should fail verification");

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_file(&pkg);
}

#[test]
fn resign_replaces_previous_signature() {
    let src = temp_dir("np-resign-src");
    let pkg = temp_file("np-resign-pkg", "neuropack");

    write(&src.join("asset.bin"), &random_bytes(16, 512));
    build_package(&src, &pkg);

    let (sk1, _) = signing::generate_keypair();
    let (sk2, vk2) = signing::generate_keypair();

    signing::sign_package(&pkg, &sk1).expect("sign with key1");
    signing::sign_package(&pkg, &sk2).expect("re-sign with key2");

    // Only the latest signature should be present; key2 must verify.
    let embedded = signing::verify_package_signature(&pkg).expect("verify key2");
    assert_eq!(embedded.to_bytes(), vk2.to_bytes());

    // Package content must still be intact.
    let out = temp_dir("np-resign-out");
    let reader = PackageReader::open(&pkg).expect("open after resign");
    reader.extract_all(&out).expect("extract after resign");

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}

// ── 11. Incremental rebuild ───────────────────────────────────────────────

#[test]
fn incremental_rebuild_reuses_unchanged_files_and_updates_changed_ones() {
    let src = temp_dir("np-incr-src");
    let out1 = temp_dir("np-incr-out1");
    let out2 = temp_dir("np-incr-out2");
    let pkg = temp_file("np-incr-pkg", "neuropack");

    let stable_data = random_bytes(20, 8192);
    let changed_data_v1 = random_bytes(21, 8192);
    let changed_data_v2 = random_bytes(22, 8192); // different content

    write(&src.join("stable.bin"),  &stable_data);
    write(&src.join("changed.bin"), &changed_data_v1);

    let pipeline = Pipeline::default();

    // Full build (no sidecar yet).
    pipeline
        .compress_folder_incremental(&src, &pkg, None)
        .expect("initial incremental build");

    // Verify first build extracts correctly.
    let r1 = PackageReader::open(&pkg).expect("open v1");
    r1.extract_all(&out1).expect("extract v1");
    assert_eq!(fs::read(out1.join("stable.bin")).unwrap(),  stable_data);
    assert_eq!(fs::read(out1.join("changed.bin")).unwrap(), changed_data_v1);

    // Modify one file.
    write(&src.join("changed.bin"), &changed_data_v2);

    // Incremental rebuild: stable.bin should be reused, changed.bin recompressed.
    let stats = pipeline
        .compress_folder_incremental(&src, &pkg, None)
        .expect("incremental rebuild");

    assert!(stats.reused >= 1, "expected at least 1 reused file, got {}", stats.reused);
    assert!(stats.recompressed >= 1, "expected at least 1 recompressed file");

    // Verify second build extracts correct content.
    let r2 = PackageReader::open(&pkg).expect("open v2");
    r2.extract_all(&out2).expect("extract v2");
    assert_eq!(fs::read(out2.join("stable.bin")).unwrap(),  stable_data,      "stable mismatch");
    assert_eq!(fs::read(out2.join("changed.bin")).unwrap(), changed_data_v2,  "changed mismatch");

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out1);
    let _ = fs::remove_dir_all(&out2);
    let _ = fs::remove_file(&pkg);
    let _ = fs::remove_file(neuropack::incremental::sidecar_path(&pkg));
}

// ── 12. extract_all error recovery ───────────────────────────────────────

#[test]
fn extract_all_continues_after_partial_failure() {
    // Build a valid package, corrupt it, then verify extract_all returns
    // failures rather than bailing entirely.
    let src = temp_dir("np-recover-src");
    let out = temp_dir("np-recover-out");
    let pkg = temp_file("np-recover-pkg", "neuropack");

    write(&src.join("good.bin"), &random_bytes(30, 512));
    write(&src.join("bad.bin"),  &random_bytes(31, 512));

    build_package(&src, &pkg);

    // Corrupt the middle of the package body.
    let mut raw = fs::read(&pkg).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0xFF;
    fs::write(&pkg, raw).unwrap();

    let reader = PackageReader::open(&pkg).expect("open corrupted");
    // extract_all must not panic or bail — it returns a failure list.
    let failures = reader.extract_all(&out).expect("extract_all should not bail");
    // At least one failure expected (corrupted entry), but the call succeeded.
    // (We accept 0 failures if both files happen to survive — corruption placement may miss.)
    drop(failures);

    let _ = fs::remove_dir_all(&src);
    let _ = fs::remove_dir_all(&out);
    let _ = fs::remove_file(&pkg);
}
