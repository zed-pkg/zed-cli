//! Pure rendering for deterministic standalone Zed → Nix flake bundles.
//!
//! The renderer consumes a validated `zed.nix-export-plan/v1`, the exact
//! immutable Zed artifact bytes named by that plan, and one approved immutable
//! `flake.lock`. It performs no filesystem writes, network requests, Nix
//! evaluation, store realization, credential lookup, or publication.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::nix_export_plan::NixExportPlan;

mod archive;
mod inventory;
mod lock;
mod paths;
pub mod persist;
mod templates;

use archive::{inspect_artifact, verify_planned_bins};
use inventory::{bundle_sha256, canonical_json_bytes, insert_bundle_file, inventory_entries};
use lock::validate_flake_lock;
use paths::{validate_bundle_path, validate_renderable_plan};
use templates::{render_flake, render_package, render_readme};

pub use persist::{
    PersistNixExportBundleOutcome, persist_nix_export_bundle, verify_persisted_bundle,
};

pub const NIX_FLAKE_BUNDLE_SCHEMA_V1: &str = "zed.nix-flake-bundle/v1";

const BUNDLE_INVENTORY_PATH: &str = "metadata/bundle.json";

/// The immutable Nixpkgs identity retained by a generated bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockedNixpkgs {
    pub reference: String,
    pub rev: String,
    pub nar_hash: String,
}

/// Digest evidence for one file in a generated bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixFlakeBundleEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// Canonical source-redacted inventory for a generated standalone flake.
///
/// `entries` intentionally excludes `metadata/bundle.json` to avoid a
/// self-referential digest. `bundle_sha256` is a domain-separated digest over
/// the sorted path, size, and raw SHA-256 digest of every listed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixFlakeBundleInventory {
    pub schema: &'static str,
    pub plan_sha256: String,
    pub flake_lock_sha256: String,
    pub nixpkgs: LockedNixpkgs,
    pub entries: Vec<NixFlakeBundleEntry>,
    pub bundle_sha256: String,
}

/// Complete in-memory output of the pure bundle renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedNixExportBundle {
    pub files: BTreeMap<String, Vec<u8>>,
    pub inventory: NixFlakeBundleInventory,
}

impl RenderedNixExportBundle {
    /// Recompute every entry, immutable input digest, and bundle digest.
    pub fn validate(&self) -> Result<()> {
        if self.inventory.schema != NIX_FLAKE_BUNDLE_SCHEMA_V1 {
            bail!(
                "unsupported Nix flake bundle schema `{}`",
                self.inventory.schema
            );
        }
        for path in self.files.keys() {
            validate_bundle_path(path)?;
        }

        let plan_bytes = self
            .files
            .get("metadata/plan.json")
            .context("rendered Nix flake bundle is missing metadata/plan.json")?;
        if inventory::sha256_bytes(plan_bytes) != self.inventory.plan_sha256 {
            bail!("Nix flake bundle plan digest does not match metadata/plan.json");
        }

        let flake_lock_bytes = self
            .files
            .get("flake.lock")
            .context("rendered Nix flake bundle is missing flake.lock")?;
        if inventory::sha256_bytes(flake_lock_bytes) != self.inventory.flake_lock_sha256 {
            bail!("Nix flake bundle lock digest does not match flake.lock");
        }
        if validate_flake_lock(flake_lock_bytes)? != self.inventory.nixpkgs {
            bail!("Nix flake bundle lock identity does not match inventory evidence");
        }

        let expected_entries = inventory_entries(&self.files)?;
        if self.inventory.entries != expected_entries {
            bail!("Nix flake bundle inventory entries do not match rendered files");
        }
        let expected_bundle_sha256 = bundle_sha256(&expected_entries)?;
        if self.inventory.bundle_sha256 != expected_bundle_sha256 {
            bail!("Nix flake bundle digest does not match rendered files");
        }

        let inventory_bytes = canonical_json_bytes(&self.inventory)?;
        match self.files.get(BUNDLE_INVENTORY_PATH) {
            Some(actual) if actual == &inventory_bytes => Ok(()),
            Some(_) => bail!("metadata/bundle.json is not the canonical bundle inventory"),
            None => bail!("rendered Nix flake bundle is missing metadata/bundle.json"),
        }
    }
}

/// Render a deterministic standalone flake without writing or executing it.
pub fn render_nix_export_bundle(
    plan: &NixExportPlan,
    artifact_bytes: &[u8],
    flake_lock_bytes: &[u8],
) -> Result<RenderedNixExportBundle> {
    plan.validate()
        .context("validating frozen Nix export plan before rendering")?;
    validate_renderable_plan(plan)?;
    paths::verify_artifact_identity(plan, artifact_bytes)?;
    let archive = inspect_artifact(artifact_bytes)?;
    verify_planned_bins(plan, &archive)?;
    let nixpkgs = validate_flake_lock(flake_lock_bytes)?;

    let plan_bytes = plan.canonical_json_bytes()?;
    let plan_sha256 = inventory::sha256_bytes(&plan_bytes);
    let flake_lock_sha256 = inventory::sha256_bytes(flake_lock_bytes);

    let mut files = BTreeMap::new();
    insert_bundle_file(
        &mut files,
        "flake.nix",
        render_flake(plan, &nixpkgs).into_bytes(),
    )?;
    insert_bundle_file(&mut files, "flake.lock", flake_lock_bytes.to_vec())?;
    insert_bundle_file(
        &mut files,
        "package.nix",
        render_package(plan, &plan_sha256).into_bytes(),
    )?;
    insert_bundle_file(
        &mut files,
        "README.md",
        render_readme(plan, &plan_sha256, &flake_lock_sha256).into_bytes(),
    )?;
    insert_bundle_file(&mut files, "metadata/plan.json", plan_bytes)?;
    insert_bundle_file(
        &mut files,
        &format!("artifacts/{}", plan.source.file_name),
        artifact_bytes.to_vec(),
    )?;

    let entries = inventory_entries(&files)?;
    let inventory = NixFlakeBundleInventory {
        schema: NIX_FLAKE_BUNDLE_SCHEMA_V1,
        plan_sha256,
        flake_lock_sha256,
        nixpkgs,
        bundle_sha256: bundle_sha256(&entries)?,
        entries,
    };
    insert_bundle_file(
        &mut files,
        BUNDLE_INVENTORY_PATH,
        canonical_json_bytes(&inventory)?,
    )?;

    let rendered = RenderedNixExportBundle { files, inventory };
    rendered.validate()?;
    Ok(rendered)
}
