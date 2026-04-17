pub mod asset_scanner;
pub mod benchmark;
pub mod classifier;
pub mod compression;
pub mod config;
pub mod decompression;
pub mod dictionary;
pub mod duplicate;
pub mod ffi;
pub mod format;
pub mod game_optimizations;
pub mod incremental;
pub mod patch;
pub mod progress;
pub mod signing;

pub use asset_scanner::AssetScanner;
pub use benchmark::BenchmarkRunner;
pub use compression::Pipeline;
pub use config::{load_config, CompressionConfig, NeuropackConfig};
pub use decompression::{ExtractFailure, ListEntry, PackageReader, VerifyFailure, VerifyReport};
pub use duplicate::find_similar_files;
pub use format::{AssetChunkRef, AssetIndexEntry, LARGE_FILE_THRESHOLD};
pub use incremental::{BuildManifest, IncrementalStats};
pub use patch::{PatchApplier, PatchBuilder};
pub use progress::ProgressToken;
pub use signing::{
    generate_keypair, is_signed, load_signing_key, load_verifying_key, save_signing_key,
    save_verifying_key, sign_package, verify_package_signature,
};
