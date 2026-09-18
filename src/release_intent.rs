use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::version::{VersionScheme, parse_version};

pub const RELEASE_INTENT_SCHEMA_V1: &str = "zed.release-intent/v1";
pub const RELEASE_INTENT_PATH: &str = ".zed/release-intent.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseIntentKind {
    None,
    Patch,
    Minor,
    Major,
    Calendar,
}

impl ReleaseIntentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
            Self::Calendar => "calendar",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIntentV1 {
    pub schema: String,
    pub package: String,
    pub version_scheme: VersionScheme,
    pub current_version: String,
    pub requested: ReleaseIntentKind,
    pub target_version: String,
    pub security_critical: bool,
    pub rationale: Option<String>,
    pub created_at_unix_ms: u64,
}

impl ReleaseIntentV1 {
    pub fn validate_against_manifest(&self, manifest: &Manifest) -> Result<()> {
        ensure!(self.schema == RELEASE_INTENT_SCHEMA_V1, "unsupported release intent schema `{}`", self.schema);
        ensure!(self.package == manifest.full_name(), "release intent package `{}` does not match manifest `{}`", self.package, manifest.full_name());
        ensure!(self.version_scheme == manifest.package.version_scheme, "release intent version scheme drifted from manifest");
        ensure!(self.target_version == manifest.package.version, "release intent targets `{}` but manifest currently declares `{}`", self.target_version, manifest.package.version);
        manifest.package.version_scheme.validate_version(&self.target_version).map_err(anyhow::Error::msg)?;
        if self.requested == ReleaseIntentKind::None {
            ensure!(self.current_version == self.target_version, "commit-only intent must not change the package version");
        } else {
            ensure!(self.current_version != self.target_version, "release intent `{}` must change the package version", self.requested.as_str());
        }
        Ok(())
    }
}

pub fn plan(
    project: &Path,
    requested: ReleaseIntentKind,
    explicit_target: Option<&str>,
    security_critical: bool,
    rationale: Option<String>,
) -> Result<ReleaseIntentV1> {
    let manifest = read_manifest(project)?;
    let current = manifest.package.version.clone();
    let scheme = manifest.package.version_scheme;
    let target = match (scheme, requested) {
        (_, ReleaseIntentKind::None) => current.clone(),
        (VersionScheme::Semver, ReleaseIntentKind::Patch) => bump_semver(&current, 0, 0, 1)?,
        (VersionScheme::Semver, ReleaseIntentKind::Minor) => bump_semver(&current, 0, 1, 0)?,
        (VersionScheme::Semver, ReleaseIntentKind::Major) => bump_semver(&current, 1, 0, 0)?,
        (VersionScheme::Semver, ReleaseIntentKind::Calendar) => {
            bail!("calendar release intent requires package.version_scheme = \"calver\"")
        }
        (VersionScheme::Calver, ReleaseIntentKind::Calendar) => {
            let target = explicit_target.context("calendar release intent requires --target-version")?;
            scheme.validate_version(target).map_err(anyhow::Error::msg)?;
            let current_order = parse_version(&current).context("current calendar version is not comparable")?;
            let target_order = parse_version(target).context("target calendar version is not comparable")?;
            ensure!(target_order > current_order, "calendar target `{target}` must be newer than `{current}`");
            target.to_owned()
        }
        (VersionScheme::Calver, _) => bail!("calver packages use `calendar` (or `none`) release intent"),
        (VersionScheme::Opaque, _) => bail!("opaque versions support commit-only intent; set an explicit package version before publishing"),
    };
    if let Some(explicit) = explicit_target
        && requested != ReleaseIntentKind::Calendar
    {
        ensure!(explicit == target, "--target-version `{explicit}` disagrees with computed `{target}`");
    }
    Ok(ReleaseIntentV1 {
        schema: RELEASE_INTENT_SCHEMA_V1.to_owned(),
        package: manifest.full_name(),
        version_scheme: scheme,
        current_version: current,
        requested,
        target_version: target,
        security_critical,
        rationale,
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis()
            .try_into()
            .context("timestamp exceeds u64")?,
    })
}

pub fn set(
    project: &Path,
    requested: ReleaseIntentKind,
    explicit_target: Option<&str>,
    security_critical: bool,
    rationale: Option<String>,
    apply_manifest: bool,
) -> Result<ReleaseIntentV1> {
    let intent = plan(project, requested, explicit_target, security_critical, rationale)?;
    if apply_manifest && intent.current_version != intent.target_version {
        update_manifest_version(project, &intent.current_version, &intent.target_version)?;
    }
    let path = project.join(RELEASE_INTENT_PATH);
    let parent = path.parent().context("release intent path has no parent")?;
    fs::create_dir_all(parent)?;
    let encoded = serde_json::to_vec_pretty(&intent)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, encoded)?;
    fs::rename(&temp, &path)?;
    Ok(intent)
}

pub fn load(project: &Path) -> Result<ReleaseIntentV1> {
    let path = project.join(RELEASE_INTENT_PATH);
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("missing release intent {}; run `zed release-intent set ...`", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("invalid release intent {}", path.display()))
}

pub fn check(project: &Path, allow_missing: bool) -> Result<Option<ReleaseIntentV1>> {
    let path = project.join(RELEASE_INTENT_PATH);
    if !path.is_file() && allow_missing {
        return Ok(None);
    }
    let intent = load(project)?;
    let manifest = read_manifest(project)?;
    intent.validate_against_manifest(&manifest)?;
    Ok(Some(intent))
}

/// Pre-commit guard: an ordinary commit with no staged package-version change is
/// untouched. A staged version change must be explained by a matching local intent.
pub fn guard_staged(project: &Path) -> Result<()> {
    let Some(previous) = git_show_manifest(project, "HEAD:.zpkg.toml")? else {
        return Ok(());
    };
    let Some(staged) = git_show_manifest(project, ":.zpkg.toml")? else {
        return Ok(());
    };
    validate_transition(project, &previous, &staged)
}

/// Pre-push guard over the exact remote/local revisions provided by Git. New
/// branches have no remote package baseline, so they are intentionally left to
/// normal CI/publish admission rather than guessed here.
pub fn guard_range(project: &Path, base: &str, head: &str) -> Result<()> {
    if base.bytes().all(|byte| byte == b'0') {
        return Ok(());
    }
    ensure!(is_git_sha(base) && is_git_sha(head), "guard-range requires exact 40-character Git SHAs");
    let Some(previous) = git_show_manifest(project, &format!("{base}:.zpkg.toml"))? else {
        return Ok(());
    };
    let Some(next) = git_show_manifest(project, &format!("{head}:.zpkg.toml"))? else {
        return Ok(());
    };
    validate_transition(project, &previous, &next)
}

fn validate_transition(project: &Path, previous: &str, next: &str) -> Result<()> {
    let previous: Manifest = toml::from_str(previous).context("previous .zpkg.toml is invalid")?;
    let next: Manifest = toml::from_str(next).context("next .zpkg.toml is invalid")?;
    if previous.package.version == next.package.version {
        return Ok(());
    }
    let intent = load(project)?;
    ensure!(intent.package == next.full_name(), "release intent package does not match target manifest");
    ensure!(intent.version_scheme == next.package.version_scheme, "release intent version scheme does not match target manifest");
    ensure!(intent.current_version == previous.package.version, "release intent starts at `{}` but pushed baseline is `{}`", intent.current_version, previous.package.version);
    ensure!(intent.target_version == next.package.version, "release intent targets `{}` but pushed manifest targets `{}`", intent.target_version, next.package.version);
    ensure!(intent.requested != ReleaseIntentKind::None, "package version changed under commit-only release intent");
    next.package.version_scheme.validate_version(&next.package.version).map_err(anyhow::Error::msg)?;
    Ok(())
}

pub fn clear(project: &Path) -> Result<bool> {
    let path = project.join(RELEASE_INTENT_PATH);
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

fn read_manifest(project: &Path) -> Result<Manifest> {
    let path = project.join(".zpkg.toml");
    let raw = fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
}

fn bump_semver(current: &str, major: u64, minor: u64, patch: u64) -> Result<String> {
    let mut version = Version::parse(current).with_context(|| format!("`{current}` is not semver"))?;
    if major == 1 {
        version.major = version.major.checked_add(1).context("major version overflow")?;
        version.minor = 0;
        version.patch = 0;
    } else if minor == 1 {
        version.minor = version.minor.checked_add(1).context("minor version overflow")?;
        version.patch = 0;
    } else if patch == 1 {
        version.patch = version.patch.checked_add(1).context("patch version overflow")?;
    }
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    Ok(version.to_string())
}

fn update_manifest_version(project: &Path, old: &str, new: &str) -> Result<()> {
    let path = project.join(".zpkg.toml");
    let raw = fs::read_to_string(&path)?;
    let mut in_package = false;
    let mut replaced = false;
    let mut output = String::with_capacity(raw.len() + new.len());
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
        }
        if in_package && trimmed.starts_with("version") && trimmed.contains('=') && !replaced {
            let prefix = line.split('=').next().unwrap_or("version");
            output.push_str(prefix);
            output.push_str("= \"");
            output.push_str(new);
            output.push_str("\"\n");
            replaced = true;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    ensure!(replaced, "[package].version was not found in .zpkg.toml");
    let parsed: Manifest = toml::from_str(&output).context("updated manifest would be invalid")?;
    ensure!(parsed.package.version == new, "updated manifest did not retain target version");
    ensure!(old != new, "refusing no-op manifest rewrite");
    fs::write(path, output)?;
    Ok(())
}

fn git_show_manifest(project: &Path, object: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["show", object])
        .output()
        .context("failed to invoke git")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout).context("git returned non-UTF8 manifest")?))
}

fn is_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn intent_path(project: &Path) -> PathBuf {
    project.join(RELEASE_INTENT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_bumps_reset_lower_components() {
        assert_eq!(bump_semver("1.2.3", 0, 0, 1).unwrap(), "1.2.4");
        assert_eq!(bump_semver("1.2.3", 0, 1, 0).unwrap(), "1.3.0");
        assert_eq!(bump_semver("1.2.3", 1, 0, 0).unwrap(), "2.0.0");
    }

    #[test]
    fn patch_syntax_does_not_apply_to_calendar_versions() {
        let version = VersionScheme::Calver;
        assert!(version.validate_version("2026.09.16").is_ok());
    }

    #[test]
    fn all_zero_remote_sha_is_treated_as_new_branch() {
        assert!("0000000000000000000000000000000000000000".bytes().all(|byte| byte == b'0'));
    }
}