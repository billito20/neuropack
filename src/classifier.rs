#![allow(dead_code)]
use crate::asset_scanner::AssetMetadata;

#[derive(Debug, Clone)]
pub enum TextureRole {
    Albedo,
    Normal,
    Roughness,
    Metallic,
    Unknown,
}

pub struct AssetClassifier;

impl AssetClassifier {
    pub fn classify_asset(asset: &AssetMetadata) -> String {
        match asset.asset_type {
            crate::asset_scanner::AssetType::Texture => format!("texture/{:?}", Self::texture_role(&asset.relative_path)),
            crate::asset_scanner::AssetType::Mesh => "mesh".to_string(),
            crate::asset_scanner::AssetType::Audio => "audio".to_string(),
            crate::asset_scanner::AssetType::Unknown => "unknown".to_string(),
        }
    }

    pub fn texture_role(path: &std::path::Path) -> TextureRole {
        let name = path.to_string_lossy().to_lowercase();
        if name.contains("normal") || name.contains("nrm") {
            TextureRole::Normal
        } else if name.contains("rough") || name.contains("rgh") {
            TextureRole::Roughness
        } else if name.contains("metal") || name.contains("spec") {
            TextureRole::Metallic
        } else if name.contains("albedo") || name.contains("diffuse") || name.contains("basecolor") {
            TextureRole::Albedo
        } else {
            TextureRole::Unknown
        }
    }
}
