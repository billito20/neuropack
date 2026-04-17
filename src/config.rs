use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NeuropackConfig {
    pub compression: Option<CompressionConfig>,
}

/// Optional per-field overrides for the compression pipeline.
/// Any field left as `None` falls back to the compiled-in default in `Pipeline`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CompressionConfig {
    pub default_zstd_level: Option<i32>,
    pub texture_zstd_level: Option<i32>,
    pub audio_zstd_level: Option<i32>,
    pub enable_mesh_delta: Option<bool>,
    pub bypass_audio_dictionary: Option<bool>,
    pub dictionary_window: Option<usize>,
    pub min_pattern_frequency: Option<usize>,
}

/// Load config from an explicit path, `./neuropack.toml`, or return defaults.
///
/// Search order:
/// 1. `explicit` path if provided via `--config`
/// 2. `./neuropack.toml` in the current working directory
/// 3. Empty config (all defaults)
pub fn load_config(explicit: Option<&Path>) -> anyhow::Result<NeuropackConfig> {
    let path = if let Some(p) = explicit {
        Some(p.to_path_buf())
    } else {
        let candidate = std::env::current_dir()?.join("neuropack.toml");
        if candidate.exists() { Some(candidate) } else { None }
    };

    match path {
        None => Ok(NeuropackConfig::default()),
        Some(p) => {
            let text = std::fs::read_to_string(&p)?;
            let cfg: NeuropackConfig = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid config {}: {}", p.display(), e))?;
            Ok(cfg)
        }
    }
}
