# NeuroPack — Corrupted Chunk Crash Scenario

This document describes the exact runtime behaviour of NeuroPack v1.0.0 when a
package file is corrupted or tampered with on a player's machine.  It is written
for engine-integration engineers, QA, and support teams who need to understand
what the player sees, what the logs say, and what recovery options exist.

---

## Overview

NeuroPack applies three independent layers of corruption detection:

| Layer | What it checks | When it runs |
|-------|---------------|--------------|
| Header sanity | Magic bytes, field offsets, file size bounds | `PackageReader::open()` — before any extraction |
| Chunk content hash | XXH3-64 of every decompressed chunk matches the stored hash | Every `decompress_chunks()` call — at extraction or verify time |
| Ed25519 signature | SHA-256 of the entire package body matches the trailing signature | `verify-sig` subcommand, or runtime API call |

These layers are independent: a corrupt body byte detected at layer 2 does **not**
require that a signature was present at layer 3.

---

## Scenario A — Corrupted Header Bytes

**Examples:** network truncation mid-download, a write that stopped after the
first 512 bytes, filesystem corruption zeroing the first sector.

### Code path

```
PackageReader::open()                     // src/decompression.rs:62
  bincode::deserialize_from(&mut file)?   // ← fails if header is truncated or garbled
  if &header.magic != b"NPCK"
      → bail!("not a NeuroPack file (bad magic)")
  if header.body_offset != expected_body
      → bail!("corrupt header: body_offset mismatch")
  if header.index_offset < header.body_offset
      → bail!("corrupt header: index before body")
  if header.index_offset + header.index_length > file_size
      → bail!("corrupt header: index extends past EOF")
```

### Runtime behaviour

`open()` returns `Err(...)`.  The error propagates through `main()` unchanged.
`clap` / `anyhow` prints to **stderr**:

```
Error: corrupt header: body_offset mismatch
```

**Process exit code: 1.**  No output directory is created, no partial files are
written.

### Player / engine experience

- **CLI (`decompress`):** process exits 1 immediately; output folder is empty.
- **Unreal plugin:** `UNeuropackLibrary::OpenPackage()` returns an invalid handle
  (handle value 0); subsequent `ReadAsset()` calls with that handle return
  `false` and leave the output `TArray` empty.
- **Unity plugin:** `new NeuropackReader(path)` throws `IOException` wrapping the
  anyhow message; callers that don't catch it will see an unhandled exception.

### Recovery

Re-download or re-copy the package.  The error message identifies the exact
mismatch so support can tell users whether the download was truncated or the
file was partially overwritten.

---

## Scenario B — Corrupted Chunk Body

**Examples:** one or more bytes in the compressed body were flipped by a failing
SSD, a network byte-flip in a streaming read, or deliberate tampering with the
body while leaving the header intact.

### Code path — chunk decompression

```
decompress_chunks()                       // src/decompression.rs:530
  for chunk_ref in &entry.chunks {
    file.seek(body_offset + chunk_ref.body_offset)?
    file.read_exact(&mut raw)?             // raw = compressed chunk bytes

    zstd::bulk::decompress(raw, uncompressed_len)?
    // ↑ zstd detects its own internal checksum mismatch and returns:
    //   Err(zstd error code -20002: "Data is corrupted" or similar)

    // OR: zstd decompresses successfully but the bytes changed:
    xxh3_64(&assembled_data) != entry.content_hash
    → bail!("chunk integrity fail textures/hero.dds: 0xabcd1234 != 0xdeadbeef")
  }
```

Both failure modes produce an `anyhow::Error` that is handled identically
upstream.

### Behaviour by CLI subcommand

#### `decompress` (extract all)

```
extract_all()                             // src/decompression.rs:128
  → extract_small_entry(...)              // src/decompression.rs:490
      → decompress_chunks(...)            // returns Err
    .err().map(|e| fail(entry, e))        // ExtractFailure { path, reason }
    failures.push(...)
    // ← loop CONTINUES — other files still extracted
```

**stderr:**
```
WARN  textures/hero.dds  — chunk integrity fail textures/hero.dds: 0xabcd1234 != 0xdeadbeef
1 file(s) failed to extract — see warnings above
```

**Exit code: 0.**  Every _other_ file is extracted correctly.  The corrupted
file is simply missing from the output directory.

#### `verify`

```
verify()                                  // src/decompression.rs:313
  verify_entry_standalone(...)            // returns Err for the bad entry
  → VerifyFailure { path, reason }
  report.failed.push(...)

// After collecting all results:
if !report.failed.is_empty()
  → anyhow::bail!("{n} integrity failure(s)")
```

**stderr:**
```
FAIL  textures/hero.dds  — chunk integrity fail textures/hero.dds: 0xabcd1234 != 0xdeadbeef
4/5 entries verified — FAILURES above
Error: 1 integrity failure(s)
```

**Exit code: 1.**

#### `extract-file` (single file)

```
extract_file()                            // src/decompression.rs:222
  extract_asset()
    decompress_chunks()  → Err(...)       // propagated directly — not caught
```

**stderr:**
```
Error: chunk integrity fail textures/hero.dds: 0xabcd1234 != 0xdeadbeef
```

**Exit code: 1.**  No partial file is written (the output file is only created
after full decompression succeeds).

### Player / engine experience

- **`decompress` / batch extract:** the title loads; assets whose package
  entries were uncorrupted load normally.  The one corrupted asset is absent —
  the engine sees a missing file, not a crash.
- **`extract-file` / `ReadAsset`:** the specific asset read returns an error;
  the engine can display a placeholder or fallback asset.

### Recovery

Run `neuropack verify game.neuropack` to identify all corrupted entries.  Then
either re-download the full package, or (if the game ships a manifest) patch
only the affected file.

---

## Scenario C — Ed25519 Signature Tampered

**Applies when:** the package was distributed signed.  Tampered signatures catch
any change to the body that also forged the trailing `NPSIG1` block, as well as
distributions where someone replaced the body but forgot to re-sign.

### Code path

```
signing::verify_package_signature()       // src/signing.rs
  // Read trailing 104 bytes:
  //   8  bytes  magic = "NPSIG1\0\0"
  //   32 bytes  Ed25519 public key
  //   64 bytes  Ed25519 signature

  if magic != b"NPSIG1\0\0"
      → bail!("package is not signed")

  // Compute SHA-256 over all bytes before the signature block.
  // Compare against the stored signature using the embedded public key:
  ed25519_dalek::VerifyingKey::verify(sha256_digest, &signature)
  // ↑ tampered body → hash changed → signature mismatch:
  //   Err("signature error: Verification equation was not satisfied")
```

**stderr:**
```
Error: signature error: Verification equation was not satisfied
```

**Exit code: 1.**

An optional `--key` flag lets the caller additionally assert _which_ keypair
signed the package:

```
if embedded_vk.to_bytes() != expected_vk.to_bytes()
    → bail!("signature is valid but public key does not match {path}")
```

This catches packages re-signed with an unauthorised key.

### Recovery

Do not load or distribute the package.  This error indicates either corruption
or tampering.  Obtain a fresh copy from a trusted source and verify before
deploying.

---

## Exit Code Reference

| Command | Condition | Exit code |
|---------|-----------|-----------|
| any | Header corrupt / bad magic | 1 |
| `decompress` | One or more chunks corrupt | **0** (partial success) |
| `verify` | Any chunk hash mismatch | 1 |
| `extract-file` | That file's chunk corrupt | 1 |
| `verify-sig` | Signature invalid or missing | 1 |
| `verify-sig --key` | Sig valid but wrong pubkey | 1 |
| all | No errors | 0 |

---

## Integration Guidance

### Unreal Engine 5

`UNeuropackLibrary::OpenPackage()` returns a `int32` handle.  A return value of
`0` (zero) indicates that `PackageReader::open()` failed — the error message is
forwarded to `UE_LOG(LogNeuroPack, Error, ...)`.  Check the handle before calling
`ReadAsset`:

```cpp
int32 handle = UNeuropackLibrary::OpenPackage(PackagePath);
if (handle == 0)
{
    UE_LOG(LogGame, Error, TEXT("NeuroPack: failed to open package"));
    // show error screen / fallback
    return;
}
TArray<uint8> data;
bool ok = UNeuropackLibrary::ReadAsset(handle, AssetPath, data);
if (!ok)
{
    UE_LOG(LogGame, Warning, TEXT("NeuroPack: asset read failed: %s"), *AssetPath);
}
```

### Unity

The C# API throws `IOException` on `open()` failure and `InvalidDataException`
on chunk integrity failure.  Wrap both in a `try/catch` and surface a
user-facing "download corrupt — please reinstall" message:

```csharp
try
{
    using var reader = new NeuropackReader("StreamingAssets/game.neuropack");
    byte[] data = reader.ReadAsset("textures/hero.dds");
    // use data
}
catch (IOException ex)
{
    Debug.LogError($"NeuroPack open failed: {ex.Message}");
    ShowCorruptInstallDialog();
}
catch (InvalidDataException ex)
{
    Debug.LogWarning($"NeuroPack chunk corrupt: {ex.Message}");
    // load placeholder asset
}
```

---

## Recovery Recommendations

1. **Run `neuropack verify` as part of your patcher / launcher** before launching
   the game.  Exit code 0 = all good; exit code 1 = at least one file is corrupt.

2. **Sign packages before distribution** (`neuropack sign game.neuropack --key
   signing.npkey`) and run `neuropack verify-sig` in the launcher.  This detects
   any tampering before any asset is loaded.

3. **Keep a per-file manifest** (the output of `neuropack list --json`) alongside
   the package.  On a verify failure, compare the manifest to identify which
   files were corrupted and download a targeted patch rather than the full package.

4. **Never treat exit code 0 from `decompress` as "all files OK"** — check the
   `WARN` lines on stderr or run `verify` instead if you need a hard guarantee.
