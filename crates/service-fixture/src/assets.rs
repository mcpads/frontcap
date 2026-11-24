//! Explicit URL mappings frozen before serving; no request ever reads a source file.
use bytes::Bytes;
use frontcap_core::{
    native::service::AssetIdentity,
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_ASSET: usize = 32 << 20;
const MAX_ASSETS: usize = 512;
const MAX_TOTAL: usize = 256 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetInput {
    pub path: String,
    pub source: PathBuf,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_security_policy: Option<String>,
}

pub(crate) struct FrozenAsset {
    pub identity: AssetIdentity,
    pub body: Bytes,
}

pub(crate) fn freeze(
    directory: &Path,
    inputs: Vec<AssetInput>,
    budget: Option<&WriteBudget>,
) -> io::Result<BTreeMap<String, FrozenAsset>> {
    if inputs.len() > MAX_ASSETS {
        return Err(io::Error::other("asset count budget exceeded"));
    }
    super::save_budgeted(budget, &directory.join("asset-inputs.json"), &inputs)?;
    let mut assets = BTreeMap::new();
    let mut total = 0;
    for input in inputs {
        let uri: hyper::Uri = input.path.parse().map_err(io::Error::other)?;
        if input.path.len() > 2048
            || !input.path.starts_with('/')
            || input.path.starts_with("//")
            || input.path.contains('#')
            || uri.query().is_some()
            || uri.path() != input.path
            || input.path == "/control"
            || assets.contains_key(&input.path)
        {
            return Err(io::Error::other("invalid or duplicate asset URL path"));
        }
        if input.content_type.len() > 128 || !input.content_type.contains('/') {
            return Err(io::Error::other("invalid asset content type"));
        }
        hyper::header::HeaderValue::from_str(&input.content_type).map_err(io::Error::other)?;
        if let Some(policy) = &input.content_security_policy {
            if policy.is_empty() || policy.len() > 4096 {
                return Err(io::Error::other("invalid content security policy length"));
            }
            hyper::header::HeaderValue::from_str(policy).map_err(io::Error::other)?;
        }
        let file = File::open(&input.source)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("asset source is not a regular file"));
        }
        let mut body = Vec::new();
        file.take((MAX_ASSET + 1) as u64).read_to_end(&mut body)?;
        total += body.len();
        if body.len() > MAX_ASSET || total > MAX_TOTAL {
            return Err(io::Error::other("asset byte budget exceeded"));
        }
        let identity = AssetIdentity {
            sha256: Sha256::digest(&body)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            size: body.len() as u64,
            content_type: input.content_type,
            content_security_policy: input.content_security_policy,
        };
        assets.insert(
            input.path,
            FrozenAsset {
                identity,
                body: Bytes::from(body),
            },
        );
    }
    // Retain the exact in-memory bytes; subsequent source edits cannot change this generation.
    let blobs = directory.join("assets");
    std::fs::create_dir(&blobs)?;
    for asset in assets.values() {
        let path = blobs.join(&asset.identity.sha256);
        if !path.exists() {
            if let Some(budget) = budget {
                budget.charge(Class::Ordinary, asset.body.len() as u64)?;
            }
            let mut file = File::create(path)?;
            file.write_all(&asset.body)?;
            file.sync_all()?;
        }
    }
    File::open(&blobs)?.sync_all()?;
    let manifest: BTreeMap<_, _> = assets
        .iter()
        .map(|(path, asset)| (path, &asset.identity))
        .collect();
    super::save_budgeted(budget, &directory.join("assets.json"), &manifest)?;
    Ok(assets)
}
