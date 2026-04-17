use crate::asset_scanner::AssetMetadata;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ExactDuplicateCluster {
    pub hash: u64,
    pub size: u64,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SimilarityGroup {
    pub paths: Vec<PathBuf>,
    pub shared_chunk_count: usize,
}

impl ExactDuplicateCluster {
    pub fn find(assets: &[AssetMetadata]) -> Vec<Self> {
        let mut clusters: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        for item in assets {
            clusters
                .entry((item.size, item.hash))
                .or_default()
                .push(item.relative_path.clone());
        }

        clusters
            .into_iter()
            .filter_map(|((size, hash), paths)| {
                if paths.len() > 1 {
                    Some(ExactDuplicateCluster { hash, size, paths })
                } else {
                    None
                }
            })
            .collect()
    }
}

pub fn find_similar_files(assets: &[AssetMetadata], sample_window: usize, min_matches: usize) -> Vec<SimilarityGroup> {
    use std::fs::File;
    use std::io::{BufReader, Read};
    use xxhash_rust::xxh3::Xxh3;

    let sample_window = sample_window.max(4096);
    let target_assets: Vec<_> = assets.iter().collect();

    let file_signatures: Vec<HashSet<u64>> = target_assets
        .par_iter()
        .filter_map(|asset| {
            let file = File::open(&asset.path).ok()?;
            let mut reader = BufReader::with_capacity(128 * 1024, file);
            let mut signatures = HashSet::new();
            let mut buffer = vec![0u8; sample_window];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let mut hasher = Xxh3::new();
                hasher.update(&buffer[..read]);
                signatures.insert(hasher.digest());
            }
            Some(signatures)
        })
        .collect();

    let mut signature_index: HashMap<u64, Vec<usize>> = HashMap::new();
    for (file_idx, signatures) in file_signatures.iter().enumerate() {
        for signature in signatures {
            signature_index.entry(*signature).or_default().push(file_idx);
        }
    }

    let mut pair_scores: HashMap<(usize, usize), usize> = HashMap::new();
    for indices in signature_index.values() {
        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                let pair = (indices[i], indices[j]);
                *pair_scores.entry(pair).or_default() += 1;
            }
        }
    }

    let mut groups = Vec::new();
    for ((a, b), shared_count) in pair_scores.into_iter().filter(|(_, count)| *count >= min_matches) {
        let paths = vec![
            target_assets[a].relative_path.clone(),
            target_assets[b].relative_path.clone(),
        ];
        groups.push(SimilarityGroup { paths, shared_chunk_count: shared_count });
    }

    groups
}
