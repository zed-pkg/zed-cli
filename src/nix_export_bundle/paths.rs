use anyhow::{Context, Result, bail};
use zed_interfaces::artifact::ArtifactFormat;

use crate::nix_export_plan::{NixExportPlan, PlannedPackageClass};

use super::inventory::sha256_bytes;

pub(super) fn validate_renderable_plan(plan: &NixExportPlan) -> Result<()> {
    if plan.source.artifact.format != ArtifactFormat::TarGz {
        bail!("Nix flake bundle v1 accepts only canonical tar.gz Zed artifacts");
    }
    if plan.intent.outputs.len() != 1 || plan.intent.outputs[0] != "out" {
        bail!("Nix flake bundle v1 requires the single explicit output `out`");
    }
    if !is_sorted_unique(&plan.intent.systems) {
        bail!("Nix export plan systems must be sorted and unique before rendering");
    }
    if !is_sorted_unique(&plan.intent.outputs) {
        bail!("Nix export plan outputs must be sorted and unique before rendering");
    }
    match plan.package_class {
        PlannedPackageClass::Data if !plan.bins.is_empty() => {
            bail!("data-package plans may not declare executable mappings")
        }
        PlannedPackageClass::PrebuiltBin if plan.bins.is_empty() => {
            bail!("prebuilt-bin plans require at least one executable mapping")
        }
        PlannedPackageClass::Data | PlannedPackageClass::PrebuiltBin => {}
    }

    validate_safe_segment("package organization", &plan.package.org)?;
    validate_safe_segment("package name", &plan.package.name)?;
    validate_safe_segment("package version", &plan.package.version)?;
    if let Some(target) = &plan.package.target {
        validate_safe_segment("package target", target)?;
    }

    let expected_file_name = format!(
        "{}-{}-{}.{}",
        plan.package.org,
        plan.package.name,
        plan.package.version,
        plan.source.artifact.format.extension()
    );
    if plan.source.file_name != expected_file_name {
        bail!(
            "planned artifact filename `{}` is not canonical; expected `{expected_file_name}`",
            plan.source.file_name
        );
    }
    validate_bundle_path(&format!("artifacts/{}", plan.source.file_name))?;

    for (name, relative) in &plan.bins {
        validate_bin_name(name)?;
        validate_payload_path(relative)?;
    }
    Ok(())
}

pub(super) fn verify_artifact_identity(plan: &NixExportPlan, bytes: &[u8]) -> Result<()> {
    let actual_size = u64::try_from(bytes.len()).context("artifact size exceeds u64")?;
    if actual_size != plan.source.artifact.size {
        bail!(
            "Zed artifact size drift: plan records {}, renderer received {actual_size}",
            plan.source.artifact.size
        );
    }
    if sha256_bytes(bytes) != plan.source.artifact.sha256 {
        bail!("Zed artifact SHA-256 drift detected before Nix bundle rendering");
    }
    Ok(())
}

pub(super) fn validate_bundle_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
    {
        bail!("unsafe generated bundle path `{path}`");
    }
    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            bail!("unsafe generated bundle path `{path}`");
        }
        if !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
        {
            bail!("generated bundle path `{path}` contains unsupported characters");
        }
    }
    Ok(())
}

fn validate_safe_segment(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('.')
        || value.starts_with('-')
        || value.ends_with('.')
        || matches!(value, "." | "..")
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    {
        bail!("{field} `{value}` is not a portable Nix bundle path segment");
    }
    Ok(())
}

fn validate_bin_name(name: &str) -> Result<()> {
    validate_safe_segment("prebuilt bin name", name)?;
    if name.contains('/') {
        bail!("prebuilt bin name `{name}` must not contain a path separator");
    }
    Ok(())
}

fn validate_payload_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
    {
        bail!("unsafe prebuilt bin path `{path}`");
    }
    for component in path.split('/') {
        validate_safe_segment("prebuilt bin path component", component)?;
    }
    Ok(())
}

fn is_sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_rejects_parent_and_backslash_components() {
        assert!(validate_bundle_path("metadata/../secret").is_err());
        assert!(validate_bundle_path("metadata\\secret").is_err());
        assert!(validate_bundle_path("metadata/plan.json").is_ok());
    }
}
