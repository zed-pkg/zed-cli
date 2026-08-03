use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use zed_interfaces::nix::is_sha256_sri;

use super::LockedNixpkgs;

pub(super) fn validate_flake_lock(bytes: &[u8]) -> Result<LockedNixpkgs> {
    let value: Value = serde_json::from_slice(bytes)
        .context("approved Nixpkgs flake lock is not valid JSON")?;
    let root = object(&value, "flake.lock root")?;
    require_exact_keys(root, &["nodes", "root", "version"], "flake.lock root")?;

    if root.get("version").and_then(Value::as_u64) != Some(7) {
        bail!("Nix flake bundle v1 requires flake.lock version 7");
    }
    if root.get("root").and_then(Value::as_str) != Some("root") {
        bail!("Nix flake bundle v1 requires the canonical root node `root`");
    }

    let nodes = object(
        root.get("nodes").context("flake.lock is missing `nodes`")?,
        "flake.lock nodes",
    )?;
    require_exact_keys(nodes, &["nixpkgs", "root"], "flake.lock nodes")?;

    let root_node = object(
        nodes.get("root").context("flake.lock is missing root node")?,
        "flake.lock root node",
    )?;
    require_exact_keys(root_node, &["inputs"], "flake.lock root node")?;
    let inputs = object(
        root_node
            .get("inputs")
            .context("flake.lock root node is missing inputs")?,
        "flake.lock root inputs",
    )?;
    require_exact_keys(inputs, &["nixpkgs"], "flake.lock root inputs")?;
    if inputs.get("nixpkgs").and_then(Value::as_str) != Some("nixpkgs") {
        bail!("flake.lock root input `nixpkgs` must select node `nixpkgs`");
    }

    let nixpkgs_node = object(
        nodes
            .get("nixpkgs")
            .context("flake.lock is missing nixpkgs node")?,
        "flake.lock nixpkgs node",
    )?;
    require_exact_keys(
        nixpkgs_node,
        &["locked", "original"],
        "flake.lock nixpkgs node",
    )?;
    let locked = object(
        nixpkgs_node
            .get("locked")
            .context("flake.lock nixpkgs node is missing locked evidence")?,
        "flake.lock nixpkgs locked evidence",
    )?;
    require_allowed_keys(
        locked,
        &["lastModified", "narHash", "owner", "repo", "rev", "type"],
        &["narHash", "owner", "repo", "rev", "type"],
        "flake.lock nixpkgs locked evidence",
    )?;
    let original = object(
        nixpkgs_node
            .get("original")
            .context("flake.lock nixpkgs node is missing original selector")?,
        "flake.lock nixpkgs original selector",
    )?;
    require_exact_keys(
        original,
        &["owner", "repo", "rev", "type"],
        "flake.lock nixpkgs original selector",
    )?;

    for (field, expected) in [
        ("type", "github"),
        ("owner", "NixOS"),
        ("repo", "nixpkgs"),
    ] {
        if locked.get(field).and_then(Value::as_str) != Some(expected)
            || original.get(field).and_then(Value::as_str) != Some(expected)
        {
            bail!(
                "approved flake.lock must pin github:NixOS/nixpkgs; `{field}` did not match `{expected}`"
            );
        }
    }

    let rev = locked
        .get("rev")
        .and_then(Value::as_str)
        .context("flake.lock nixpkgs locked evidence is missing string rev")?;
    let original_rev = original
        .get("rev")
        .and_then(Value::as_str)
        .context("flake.lock nixpkgs original selector is missing string rev")?;
    if rev != original_rev || !is_lower_hex_revision(rev) {
        bail!("flake.lock must use one exact lowercase 40- or 64-character Nixpkgs revision");
    }

    let nar_hash = locked
        .get("narHash")
        .and_then(Value::as_str)
        .context("flake.lock nixpkgs locked evidence is missing string narHash")?;
    if !is_sha256_sri(nar_hash) {
        bail!("flake.lock Nixpkgs narHash must be an exact SHA-256 SRI value");
    }
    if let Some(last_modified) = locked.get("lastModified")
        && last_modified.as_u64().is_none()
    {
        bail!("flake.lock Nixpkgs lastModified must be an unsigned integer");
    }

    Ok(LockedNixpkgs {
        reference: format!("github:NixOS/nixpkgs/{rev}"),
        rev: rev.to_string(),
        nar_hash: nar_hash.to_string(),
    })
}

fn is_lower_hex_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

fn object<'a>(value: &'a Value, field: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{field} must be a JSON object"))
}

fn require_exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
    field: &str,
) -> Result<()> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        bail!("{field} has unsupported or missing keys");
    }
    Ok(())
}

fn require_allowed_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    required: &[&str],
    field: &str,
) -> Result<()> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    let required = required.iter().copied().collect::<BTreeSet<_>>();
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if !actual.is_subset(&allowed) || !required.is_subset(&actual) {
        bail!("{field} has unsupported or missing keys");
    }
    Ok(())
}
