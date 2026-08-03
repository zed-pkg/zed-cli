use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    BUNDLE_INVENTORY_PATH, NIX_FLAKE_BUNDLE_SCHEMA_V1, NixFlakeBundleEntry,
};
use crate::nix_export_bundle::paths::validate_bundle_path;

pub(super) fn inventory_entries(
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<NixFlakeBundleEntry>> {
    files
        .iter()
        .filter(|(path, _)| path.as_str() != BUNDLE_INVENTORY_PATH)
        .map(|(path, bytes)| {
            validate_bundle_path(path)?;
            Ok(NixFlakeBundleEntry {
                path: path.clone(),
                sha256: sha256_bytes(bytes),
                size: u64::try_from(bytes.len()).context("bundle file size exceeds u64")?,
            })
        })
        .collect()
}

pub(super) fn bundle_sha256(entries: &[NixFlakeBundleEntry]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(NIX_FLAKE_BUNDLE_SCHEMA_V1.as_bytes());
    hasher.update([0]);
    for entry in entries {
        validate_bundle_path(&entry.path)?;
        let digest = hex::decode(&entry.sha256)
            .with_context(|| format!("invalid SHA-256 for bundle entry `{}`", entry.path))?;
        if digest.len() != 32 {
            bail!("invalid SHA-256 length for bundle entry `{}`", entry.path);
        }
        hasher.update(
            u64::try_from(entry.path.len())
                .context("bundle entry path length exceeds u64")?
                .to_be_bytes(),
        );
        hasher.update(entry.path.as_bytes());
        hasher.update(entry.size.to_be_bytes());
        hasher.update(digest);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) fn insert_bundle_file(
    files: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    bytes: Vec<u8>,
) -> Result<()> {
    validate_bundle_path(path)?;
    if files.insert(path.to_string(), bytes).is_some() {
        bail!("generated Nix flake bundle path `{path}` appears more than once");
    }
    Ok(())
}

pub(super) fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value).context("serializing Nix flake bundle JSON")?;
    serde_json::to_vec(&canonicalize_json(value))
        .context("encoding canonical Nix flake bundle JSON")
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}
