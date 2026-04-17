mod asset_scanner;
mod benchmark;
mod classifier;
mod compression;
mod config;
mod decompression;
mod dictionary;
mod duplicate;
mod ffi;
mod format;
mod game_optimizations;
mod gui;
mod incremental;
mod patch;
mod progress;
mod signing;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::asset_scanner::{AssetScanner, AssetType};
use crate::benchmark::BenchmarkRunner;
use crate::compression::Pipeline;
use crate::config::load_config;
use crate::decompression::PackageReader;
use crate::duplicate::{find_similar_files, ExactDuplicateCluster};
use crate::patch::{PatchApplier, PatchBuilder};

#[derive(Parser)]
#[command(name = "neuropack")]
#[command(author = "NeuroPack Team")]
#[command(version = "1.0.0")]
#[command(about = "NeuroPack asset compression engine", long_about = None)]
struct Cli {
    /// Path to a neuropack.toml config file (default: ./neuropack.toml if it exists).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan a folder and list discovered assets.
    Scan {
        path: PathBuf,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Compress a folder into a .neuropack package.
    Compress {
        path: PathBuf,
        #[arg(long, default_value = "output.neuropack")]
        output: PathBuf,
    },
    /// Extract a .neuropack package to a folder.
    Decompress {
        package: PathBuf,
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    /// Extract a single file from a .neuropack package.
    ExtractFile {
        package: PathBuf,
        /// Relative path of the file inside the package (e.g. data/textures/hero.dds).
        file_path: PathBuf,
        /// Output directory (default: current directory).
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    /// Run compression benchmarks vs. zstd and brotli.
    Benchmark {
        path: PathBuf,
        #[arg(long, default_value = "benchmark.json")]
        report: PathBuf,
        /// Skip brotli compressors (recommended for datasets > 5 GB).
        #[arg(long, default_value_t = false)]
        no_brotli: bool,
        /// Skip zstd-19 (very slow; useful for large datasets where a rough comparison is enough).
        #[arg(long, default_value_t = false)]
        skip_zstd19: bool,
        /// Skip NeuroPack passes (use when disk space < compressed corpus size).
        #[arg(long, default_value_t = false)]
        skip_neuropack: bool,
    },
    /// Build a CDC binary-diff patch between two folder versions.
    Patch {
        old: PathBuf,
        new: PathBuf,
        #[arg(long, default_value = "patch.neuropack")]
        output: PathBuf,
    },
    /// Apply a patch produced by the `patch` command.
    ApplyPatch {
        patch: PathBuf,
        #[arg(long, default_value = ".")]
        target: PathBuf,
    },
    /// Scan a folder and report asset breakdown, exact duplicates, and similar files.
    Analyze {
        path: PathBuf,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Validate every entry's content hash in a package without extracting.
    Verify {
        package: PathBuf,
        /// Print every verified entry, not just failures.
        #[arg(long, default_value_t = false)]
        verbose: bool,
    },
    /// List package contents: paths, sizes, compression ratios, and duplicates.
    List {
        package: PathBuf,
        /// Emit JSON instead of a human-readable table.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Sort entries by uncompressed size (largest first).
        #[arg(long, default_value_t = false)]
        sort_by_size: bool,
    },
    /// Generate an Ed25519 signing key pair.
    KeyGen {
        /// Write the 32-byte signing key (private) to this path.
        #[arg(long, default_value = "neuropack.npkey")]
        signing_key: PathBuf,
        /// Write the 32-byte verifying key (public) to this path.
        #[arg(long, default_value = "neuropack.nppub")]
        verifying_key: PathBuf,
    },
    /// Sign a .neuropack package with an Ed25519 signing key.
    Sign {
        package: PathBuf,
        /// Path to the 32-byte signing key (*.npkey).
        #[arg(long)]
        key: PathBuf,
    },
    /// Verify the Ed25519 signature on a .neuropack package.
    VerifySig {
        package: PathBuf,
        /// Optional: check against a specific public key (*.nppub).
        /// Without this flag the embedded public key is used.
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Incrementally compress a folder: unchanged files are reused from the
    /// previous package without re-compression.
    CompressIncremental {
        path: PathBuf,
        #[arg(long, default_value = "output.neuropack")]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    // Launch GUI when the binary is run with no arguments.
    if std::env::args_os().len() == 1 {
        gui::launch().map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(());
    }

    let cli = Cli::parse();
    let cfg = load_config(cli.config.as_deref())?;

    match cli.command {
        Commands::Scan { path, json } => {
            let scanner = AssetScanner::default();
            let assets = scanner.scan(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&assets)?);
            } else {
                println!("Scanned {} asset entries", assets.len());
            }
        }

        Commands::Compress { path, output } => {
            let pipeline = Pipeline::from_config(cfg.compression.as_ref());
            pipeline.compress_folder(&path, &output)?;
        }

        Commands::Decompress { package, output } => {
            let reader = PackageReader::open(&package)?;
            std::fs::create_dir_all(&output)?;
            let failures = reader.extract_all(&output)?;
            for f in &failures {
                eprintln!("WARN  {}  — {}", f.path.display(), f.reason);
            }
            if failures.is_empty() {
                println!("Decompressed package to {}", output.display());
            } else {
                eprintln!(
                    "{} file(s) failed to extract — see warnings above",
                    failures.len()
                );
            }
        }

        Commands::ExtractFile { package, file_path, output } => {
            let reader = PackageReader::open(&package)?;
            reader.extract_file(&file_path, &output)?;
            println!(
                "Extracted {} to {}",
                file_path.display(),
                output.display()
            );
        }

        Commands::Benchmark { path, report, no_brotli, skip_zstd19, skip_neuropack } => {
            let runner = BenchmarkRunner::default();
            runner.run(&path, &report, no_brotli, skip_zstd19, skip_neuropack)?;
            println!("Benchmark report saved to {}", report.display());
        }

        Commands::Patch { old, new, output } => {
            let patcher = PatchBuilder::default();
            patcher.build(&old, &new, &output)?;
            println!("Created patch {}", output.display());
        }

        Commands::ApplyPatch { patch, target } => {
            PatchApplier::apply(&patch, &target)?;
            println!("Applied patch to {}", target.display());
        }

        Commands::Analyze { path, json } => {
            cmd_analyze(&path, json)?;
        }

        Commands::Verify { package, verbose } => {
            let reader = PackageReader::open(&package)?;
            let report = reader.verify()?;

            if verbose {
                for entry in &reader.index {
                    if entry.duplicate_of.is_none() {
                        println!("  ok  {}", entry.relative_path.display());
                    }
                }
            }

            for failure in &report.failed {
                eprintln!("FAIL  {}  — {}", failure.path.display(), failure.reason);
            }

            println!(
                "{}/{} entries verified{}",
                report.verified,
                report.total_entries,
                if report.failed.is_empty() { " — all ok" } else { " — FAILURES above" }
            );

            if !report.failed.is_empty() {
                anyhow::bail!("{} integrity failure(s)", report.failed.len());
            }
        }

        Commands::List { package, json, sort_by_size } => {
            let reader = PackageReader::open(&package)?;
            let mut entries = reader.list_entries();

            if sort_by_size {
                entries.sort_by(|a, b| b.uncompressed_bytes.cmp(&a.uncompressed_bytes));
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                cmd_list_table(&entries, &reader);
            }
        }

        Commands::KeyGen { signing_key, verifying_key } => {
            let (sk, vk) = signing::generate_keypair();
            signing::save_signing_key(&signing_key, &sk)?;
            signing::save_verifying_key(&verifying_key, &vk)?;
            println!("Signing key  → {}", signing_key.display());
            println!("Verifying key→ {}", verifying_key.display());
            println!("Guard the .npkey file — it is the private key.");
        }

        Commands::Sign { package, key } => {
            let sk = signing::load_signing_key(&key)?;
            signing::sign_package(&package, &sk)?;
            let vk = sk.verifying_key();
            println!(
                "Signed {}  (pubkey {})",
                package.display(),
                hex_bytes(&vk.to_bytes())
            );
        }

        Commands::VerifySig { package, key } => {
            let embedded_vk = signing::verify_package_signature(&package)?;
            if let Some(key_path) = key {
                let expected = signing::load_verifying_key(&key_path)?;
                if embedded_vk.to_bytes() != expected.to_bytes() {
                    anyhow::bail!(
                        "signature is valid but public key does not match {}",
                        key_path.display()
                    );
                }
            }
            println!(
                "Signature OK  pubkey {}",
                hex_bytes(&embedded_vk.to_bytes())
            );
        }

        Commands::CompressIncremental { path, output } => {
            let pipeline = Pipeline::from_config(cfg.compression.as_ref());
            let stats = pipeline.compress_folder_incremental(&path, &output, None)?;
            println!(
                "Incremental compress: {}/{} files reused, {} recompressed, {} removed → {}",
                stats.reused,
                stats.total,
                stats.recompressed,
                stats.deleted,
                output.display()
            );
        }
    }

    Ok(())
}

fn cmd_list_table(entries: &[crate::decompression::ListEntry], reader: &PackageReader) {
    let mb = |b: u64| -> String {
        if b == 0 { "---".to_string() } else { format!("{:.1} MB", b as f64 / 1_048_576.0) }
    };

    println!(
        "{:<55} {:<9} {:>10} {:>10} {:>7}  {}",
        "PATH", "TYPE", "UNCOMPR", "COMPR", "RATIO", "DUP"
    );
    println!("{}", "-".repeat(100));

    for e in entries {
        let ratio_str = if e.is_duplicate {
            "  dup".to_string()
        } else if e.compressed_bytes == 0 {
            "  ---".to_string()
        } else {
            format!("{:.2}x", e.ratio)
        };

        let dup_note = match &e.duplicate_of {
            Some(orig) => format!("→ {}", orig),
            None => String::new(),
        };

        let path_display = if e.path.len() > 54 {
            format!("…{}", &e.path[e.path.len() - 53..])
        } else {
            e.path.clone()
        };

        println!(
            "{:<55} {:<9} {:>10} {:>10} {:>7}  {}",
            path_display,
            &e.asset_type[..e.asset_type.len().min(9)],
            mb(e.uncompressed_bytes),
            mb(e.compressed_bytes),
            ratio_str,
            dup_note,
        );
    }

    println!("{}", "-".repeat(100));

    let total_uncompressed: u64 = reader.index.iter().map(|e| e.uncompressed_length).sum();
    let total_compressed: u64 = reader
        .index
        .iter()
        .filter(|e| e.duplicate_of.is_none())
        .map(|e| e.compressed_length)
        .sum();
    let overall_ratio = if total_compressed == 0 {
        0.0
    } else {
        total_uncompressed as f64 / total_compressed as f64
    };

    println!(
        "{} entries | {:.1} MB uncompressed | {:.1} MB compressed | {:.2}x overall",
        entries.len(),
        total_uncompressed as f64 / 1_048_576.0,
        total_compressed as f64 / 1_048_576.0,
        overall_ratio,
    );
}

fn cmd_analyze(path: &std::path::Path, json: bool) -> anyhow::Result<()> {
    use serde::Serialize;

    #[derive(Serialize)]
    struct TypeStats { files: usize, bytes: u64 }
    #[derive(Serialize)]
    struct DuplicateGroup { hash: u64, files: usize, bytes_each: u64, paths: Vec<std::path::PathBuf> }
    #[derive(Serialize)]
    struct SimilarPair { paths: Vec<std::path::PathBuf>, shared_chunks: usize }
    #[derive(Serialize)]
    struct AnalysisReport {
        total_files: usize, total_bytes: u64,
        textures: TypeStats, meshes: TypeStats, audio: TypeStats, unknown: TypeStats,
        exact_duplicate_clusters: usize, bytes_wasted_by_duplicates: u64,
        duplicates: Vec<DuplicateGroup>, similar_groups: Vec<SimilarPair>,
    }

    let scanner = AssetScanner::default();
    let assets = scanner.scan(path)?;

    let mut tex     = TypeStats { files: 0, bytes: 0 };
    let mut mesh    = TypeStats { files: 0, bytes: 0 };
    let mut audio   = TypeStats { files: 0, bytes: 0 };
    let mut unknown = TypeStats { files: 0, bytes: 0 };
    let total_bytes: u64 = assets.iter().map(|a| a.size).sum();

    for a in &assets {
        let bucket = match a.asset_type {
            AssetType::Texture => &mut tex,
            AssetType::Mesh    => &mut mesh,
            AssetType::Audio   => &mut audio,
            AssetType::Unknown => &mut unknown,
        };
        bucket.files += 1;
        bucket.bytes += a.size;
    }

    let dup_clusters = ExactDuplicateCluster::find(&assets);
    let mut bytes_wasted = 0u64;
    let mut dup_groups: Vec<DuplicateGroup> = Vec::new();
    for cluster in &dup_clusters {
        bytes_wasted += cluster.size * (cluster.paths.len() as u64 - 1);
        dup_groups.push(DuplicateGroup {
            hash: cluster.hash,
            files: cluster.paths.len(),
            bytes_each: cluster.size,
            paths: cluster.paths.clone(),
        });
    }

    let similar = find_similar_files(&assets, 4096, 3);
    let similar_pairs: Vec<SimilarPair> = similar
        .iter()
        .map(|g| SimilarPair { paths: g.paths.clone(), shared_chunks: g.shared_chunk_count })
        .collect();

    let report = AnalysisReport {
        total_files: assets.len(), total_bytes,
        textures: tex, meshes: mesh, audio, unknown,
        exact_duplicate_clusters: dup_clusters.len(),
        bytes_wasted_by_duplicates: bytes_wasted,
        duplicates: dup_groups, similar_groups: similar_pairs,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let mb  = |b: u64| b as f64 / 1_048_576.0;
    let pct = |b: u64| if total_bytes == 0 { 0.0 } else { b as f64 / total_bytes as f64 * 100.0 };

    println!("Asset Summary  ({} files, {:.1} MB)", report.total_files, mb(report.total_bytes));
    println!("  Textures : {:5} files  ({:.1} MB, {:.0}%)", report.textures.files, mb(report.textures.bytes), pct(report.textures.bytes));
    println!("  Meshes   : {:5} files  ({:.1} MB, {:.0}%)", report.meshes.files, mb(report.meshes.bytes), pct(report.meshes.bytes));
    println!("  Audio    : {:5} files  ({:.1} MB, {:.0}%)", report.audio.files, mb(report.audio.bytes), pct(report.audio.bytes));
    println!("  Unknown  : {:5} files  ({:.1} MB, {:.0}%)", report.unknown.files, mb(report.unknown.bytes), pct(report.unknown.bytes));
    println!();
    println!("Exact Duplicates: {} cluster(s), {:.1} MB wasted", dup_clusters.len(), mb(bytes_wasted));
    for g in &report.duplicates {
        println!("  {} files × {:.1} MB", g.files, mb(g.bytes_each));
        for p in &g.paths { println!("    {}", p.display()); }
    }
    println!();
    println!("Similar File Groups: {} found (≥3 shared chunks)", report.similar_groups.len());
    for g in &report.similar_groups {
        println!("  {} shared chunks", g.shared_chunks);
        for p in &g.paths { println!("    {}", p.display()); }
    }

    Ok(())
}

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|byte| format!("{byte:02x}")).collect()
}
