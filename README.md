# NeuroPack

NeuroPack is a game asset compression engine built in Rust. It packages a folder of game assets into a single `.neuropack` file using content-defined chunking (CDC) with cross-file chunk deduplication, per-type zstd dictionary training, and transparent bypass for already-compressed formats (JPEG, DDS, OGG, MP3, MP4, …). Packages are optionally signed with Ed25519 for tamper-proof distribution, and incremental rebuilds skip files that haven't changed.

Designed for use in game build pipelines and playable at runtime via Unreal Engine 5 and Unity UPM plugins.

---

## Install

### Prerequisites

- [Rust](https://rustup.rs) 1.75 or later — install with `rustup update stable`
- Windows, Linux, or macOS (x86-64 or ARM64)

### Build from source

```bash
git clone https://github.com/billito20/neuropack
cd neuropack
cargo build --release
```

The binary is at `target/release/neuropack.exe` (Windows) or `target/release/neuropack` (Linux/Mac).

Optionally add it to your PATH:

```bash
# Linux / Mac
cp target/release/neuropack ~/.local/bin/

# Windows (PowerShell)
Copy-Item target\release\neuropack.exe $env:USERPROFILE\bin\
```

### Verify the install

```bash
neuropack --version   # neuropack 1.0.0
neuropack --help      # full command list
```

---

## Usage

### Compress a folder

```bash
neuropack compress assets/ game.neuropack
```

Produces a single `.neuropack` file. All asset types are handled automatically: already-compressed formats (JPEG, MP3, OGG, MP4, DDS, …) are stored raw; everything else is CDC-chunked, cross-file deduplicated, and zstd-compressed with per-type trained dictionaries.

### Extract

```bash
# All files
neuropack decompress game.neuropack output/

# Single file
neuropack extract-file game.neuropack textures/hero.dds --output /tmp/
```

### Inspect a package

```bash
neuropack list game.neuropack                  # human-readable table
neuropack list game.neuropack --json           # JSON output for scripts
neuropack list game.neuropack --sort-by-size   # largest files first
```

### Verify integrity

```bash
neuropack verify game.neuropack                # exit 0 = all ok, exit 1 = failures
neuropack verify game.neuropack --verbose      # print every verified entry
```

### Sign and verify signatures

```bash
# Generate a key pair
neuropack keygen --signing-key signing.npkey --verifying-key signing.nppub

# Sign a package
neuropack sign game.neuropack --key signing.npkey

# Verify signature
neuropack verify-sig game.neuropack --key signing.nppub
```

### Incremental rebuild

```bash
neuropack compress-incremental assets/ game.neuropack
```

Unchanged files are reused from the previous package without re-compression. Only modified or new files are processed. Essential for game build pipelines where most assets don't change between builds.

### Benchmark

```bash
neuropack benchmark assets/ --report benchmark.json

# Recommended flags for large datasets
neuropack benchmark assets/ --report benchmark.json --no-brotli           # skip brotli (> 5 GB)
neuropack benchmark assets/ --report benchmark.json --no-brotli --skip-zstd19  # skip zstd-19 too (> 20 GB)
```

Run any subcommand with `--help` for full option descriptions.

---

## Performance

Measured on the Blender 5.0 install (5,374 files, 333 MB), 20-core machine:

| Compressor | Ratio | Compress | Decompress |
| --- | --- | --- | --- |
| zstd-3 | 2.03× | 859 ms | 137 ms |
| zstd-9 | 2.10× | 5,451 ms | 573 ms |
| zstd-19 | 2.20× | 69,935 ms | 201 ms |
| brotli-5 | 2.12× | 8,107 ms | 710 ms |
| brotli-11 | 2.25× | 299,068 ms | 716 ms |
| NeuroPack (baseline) | 2.00× | 12,969 ms | 1,122 ms |
| **NeuroPack (optimised)** | **2.00×** | **13,086 ms** | **1,048 ms** |

See [`benchmark.json`](benchmark.json) for the full report.

### Large dataset stress test results

| Dataset | Files | Size | NeuroPack ratio | vs zstd-3 |
| --- | --- | --- | --- | --- |
| Blender 5.0 (general files) | 5,374 | 333 MB | 2.00× | −2% |
| church/ (Photoshop PSDs) | 194 | 36.9 GB | 1.57× | **+5%** |
| Photoshop/ (mixed PSD/JPEG/PNG) | 607 | 10.3 GB | 1.73× | **+4%** |

NeuroPack's advantage comes from cross-file CDC chunk deduplication: the church corpus saved 1,312 MB (8,492 chunk hits) across 192 similar design assets. Zero crashes or OOM failures across all 47+ GB of test data. Already-compressed assets (JPEG, MP4) are stored raw — no wasted CPU expanding incompressible data.

---

## Reliability

NeuroPack detects corruption at three independent layers: magic-byte and offset checks on open, per-chunk XXH3-64 hash verification on every read, and Ed25519 signature verification for signed packages. A single corrupted chunk in `decompress` is a non-fatal warning (other files still extract); `verify` and `extract-file` treat it as a hard error (exit 1).

See [`docs/corrupted-chunk-scenario.md`](docs/corrupted-chunk-scenario.md) for a full walkthrough — header checks, chunk hash mismatch handling, signature tamper detection, exit codes, and Unreal/Unity integration guidance.

---

## Format overview

NeuroPack packages use the magic bytes `NPCK` and a version field:

| Version | Description |
| --- | --- |
| 1 | Legacy: single zstd blob + segment dictionary (readable by v2/v3 code) |
| 2 | CDC chunk dedup: cross-file FastCDC chunking, already-compressed bypass |
| 3 | Per-type trained zstd dictionaries (texture / mesh / other) added on top of v2 |

All versions are backward-compatible: v3 code reads v1 and v2 packages correctly.

Package layout:

```text
[Header (bincode)] [Metadata (JSON)] [Dictionary blob (bincode)] [Body] [Index (bincode)]
```

- **Body**: compressed chunk blobs, each stored once even when referenced by multiple files
- **Index**: per-file list of chunk references, duplicate pointers, hash, asset type
- **Dictionary**: pattern dict (v1 fallback) + per-type trained zstd dicts (v3)

Signing appends a 104-byte `NPSIG1` block after the last byte: 8-byte magic, 32-byte Ed25519 public key, 64-byte signature over `SHA-256(all preceding bytes)`.

---

## Rust API

```rust
use neuropack::compression::Pipeline;
use neuropack::decompression::PackageReader;

// Compress
Pipeline::default().compress_folder("assets/", "game.neuropack")?;

// Extract all
let reader = PackageReader::open("game.neuropack")?;
reader.extract_all("output/")?;

// Read one asset to memory
let data: Vec<u8> = reader.extract_asset(reader.index.iter()
    .find(|e| e.relative_path.to_str() == Some("textures/hero.dds"))
    .unwrap())?;

// Verify integrity
let report = reader.verify()?;
println!("{}/{} entries verified", report.verified, report.total_entries);

// Incremental rebuild
let stats = Pipeline::default()
    .compress_folder_incremental("assets/", "game.neuropack", None)?;
println!("{} reused, {} recompressed", stats.reused, stats.recompressed);
```

---

## Engine plugins

### Unreal Engine 5 (Win64 / Linux / Mac)

1. Copy `unreal_plugin/NeuroPack/` into your project's `Plugins/` folder.
2. Build NeuroPack as a shared library for your target platform (`cargo build --release`).
3. Place the `.dll` / `.so` / `.dylib` next to the `.uplugin` as referenced in `Build.cs`.
4. Regenerate project files and build in the editor.

C++ API:

```cpp
int32 Handle = UNeuropackLibrary::OpenPackage(FString Path);  // 0 = failed to open
bool  Ok     = UNeuropackLibrary::ReadAsset(Handle, FString RelPath, TArray<uint8>& Out);
void         UNeuropackLibrary::ClosePackage(Handle);
```

Always check `Handle != 0` before calling `ReadAsset`. A corrupted or missing package returns 0 and logs the error via `UE_LOG`.

### Unity (2022.3 LTS+)

1. In the Package Manager, choose **Add package from disk** and point to `unity_plugin/com.neuropack.runtime/package.json`.
2. Build NeuroPack as a shared library and place it in `Assets/Plugins/`.
3. Use the C# API:

```csharp
using NeuroPack;

try
{
    using var reader = new NeuropackReader("StreamingAssets/game.neuropack");
    byte[] data = reader.ReadAsset("textures/hero.dds");

    // Or load directly into a Texture2D
    Texture2D tex = reader.LoadTexture("textures/hero.dds");
}
catch (IOException ex)
{
    Debug.LogError($"NeuroPack open failed: {ex.Message}");
}
```

---

## License

Apache-2.0 — see [`Cargo.toml`](Cargo.toml).
