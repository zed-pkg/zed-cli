use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use zed_interfaces::dependents::{
    AutomationModeV1, CONSUMER_REGISTRATION_PROTOCOL_V1, ConsumerKindV1,
    ConsumerRegistrationReceiptV1, ConsumerRegistrationRequestV1, LockedDependencySnapshotV1,
};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;

use crate::config::Config;

/// Repository-owned consumer automation policy. `.zed/` is intentionally local
/// scratch in zed-cli, so durable team policy lives beside `.zpkg.toml` instead.
pub const DEPENDENTS_CONFIG_PATH: &str = ".zpkg-dependents.toml";

fn default_ttl() -> u64 { 86_400 }
fn yes() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DependentsConfig {
    pub version: u32,
    pub consumer_kind: ConsumerKindV1,
    pub automation_mode: AutomationModeV1,
    pub default_branch: Option<String>,
    pub registration_ttl_seconds: u64,
    pub notify_major: bool,
    pub notify_minor: bool,
    pub notify_patch: bool,
    pub security_patch_notify: bool,
    pub validation_commands: Vec<String>,
}

impl Default for DependentsConfig {
    fn default() -> Self {
        Self {
            version: 1,
            consumer_kind: ConsumerKindV1::Server,
            automation_mode: AutomationModeV1::NotifyOnly,
            default_branch: None,
            registration_ttl_seconds: default_ttl(),
            notify_major: yes(),
            notify_minor: yes(),
            notify_patch: false,
            security_patch_notify: yes(),
            validation_commands: Vec::new(),
        }
    }
}

pub fn init_config(project: &Path, kind: ConsumerKindV1, mode: AutomationModeV1) -> Result<()> {
    let path = project.join(DEPENDENTS_CONFIG_PATH);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let mut config = DependentsConfig::default();
    config.consumer_kind = kind;
    config.automation_mode = mode;
    fs::write(&path, toml::to_string_pretty(&config)?)?;
    println!("created {}", path.display());
    Ok(())
}

pub fn load_config(project: &Path) -> Result<DependentsConfig> {
    let path = project.join(DEPENDENTS_CONFIG_PATH);
    let raw = fs::read_to_string(&path).with_context(|| {
        format!("missing {}; run `zed dependents init`", path.display())
    })?;
    let config: DependentsConfig = toml::from_str(&raw)
        .with_context(|| format!("invalid {}", path.display()))?;
    ensure!(config.version == 1, "unsupported dependents config version {}", config.version);
    ensure!((300..=2_592_000).contains(&config.registration_ttl_seconds), "registration_ttl_seconds must be 300..=2592000");
    Ok(config)
}

pub fn build_registration(project: &Path) -> Result<ConsumerRegistrationRequestV1> {
    let config = load_config(project)?;
    let manifest = read_manifest(project)?;
    let lock = read_lock(project)?;
    ensure!(!lock.packages.is_empty(), ".zpkg.lock has no packages to register");

    let direct = manifest
        .dependencies
        .keys()
        .chain(manifest.build_dependencies.keys())
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    let dependencies = lock
        .packages
        .iter()
        .map(|locked| {
            let package = format!("{}/{}", locked.org, locked.name);
            let requirement = manifest
                .dependencies
                .get(&package)
                .or_else(|| manifest.build_dependencies.get(&package))
                .cloned();
            LockedDependencySnapshotV1 {
                package: package.clone(),
                resolved_version: locked.version.clone(),
                requirement,
                source: locked.source.clone(),
                checksum_sha256: Some(locked.sha256.clone()),
                direct: direct.contains(package.as_str()),
            }
        })
        .collect::<Vec<_>>();

    let repository = github_repository(project)?;
    let default_branch = match config.default_branch {
        Some(branch) => branch,
        None => discover_default_branch(project)?,
    };
    let source_commit = git(project, &["rev-parse", "HEAD"])?;
    let request = ConsumerRegistrationRequestV1 {
        protocol: CONSUMER_REGISTRATION_PROTOCOL_V1.to_owned(),
        repository,
        default_branch,
        source_commit,
        consumer_kind: config.consumer_kind,
        automation_mode: config.automation_mode,
        registration_ttl_seconds: config.registration_ttl_seconds,
        notify_major: config.notify_major,
        notify_minor: config.notify_minor,
        notify_patch: config.notify_patch,
        security_patch_notify: config.security_patch_notify,
        dependencies,
        validation_commands: config.validation_commands,
    };
    request.validate().map_err(anyhow::Error::msg)?;
    Ok(request)
}

pub fn snapshot(project: &Path, json: bool) -> Result<()> {
    let request = build_registration(project)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&request)?);
    } else {
        println!("consumer: {} @ {}", request.repository, request.source_commit);
        println!("branch: {}", request.default_branch);
        println!("automation: {:?}", request.automation_mode);
        println!("locked dependencies: {}", request.dependencies.len());
        for dependency in request.dependencies {
            println!(
                "  {}@{}{}",
                dependency.package,
                dependency.resolved_version,
                if dependency.direct { " (direct)" } else { " (transitive)" }
            );
        }
    }
    Ok(())
}

pub fn register(project: &Path, cfg: &Config, dry_run: bool, json: bool) -> Result<()> {
    let request = build_registration(project)?;
    if dry_run {
        if json {
            println!("{}", serde_json::to_string_pretty(&request)?);
        } else {
            println!("would register {} with {} locked dependencies", request.repository, request.dependencies.len());
        }
        return Ok(());
    }
    ensure!(cfg.registry.starts_with("https://") || cfg.registry.starts_with("http://"), "dependent registration requires an HTTP(S) registry API");
    let token = cfg.resolve_token()?.context("dependent registration requires authentication")?;
    let url = format!("{}/v1/dependents/registrations", cfg.registry.trim_end_matches('/'));
    let response = reqwest::blocking::Client::new()
        .put(&url)
        .bearer_auth(token)
        .json(&request)
        .send()
        .with_context(|| format!("failed to register consumer at {url}"))?;
    let status = response.status();
    let body = response.text()?;
    ensure!(status.is_success(), "consumer registration failed ({status}): {body}");
    let receipt: ConsumerRegistrationReceiptV1 = serde_json::from_str(&body)
        .context("registry returned an invalid consumer registration receipt")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        println!("registered {}", receipt.repository);
        println!("registration: {}", receipt.registration_id);
        println!("expires-at-unix-ms: {}", receipt.expires_at_unix_ms);
        println!("dependency-snapshot-sha256: {}", receipt.dependency_snapshot_sha256);
    }
    Ok(())
}

fn read_manifest(project: &Path) -> Result<Manifest> {
    let path = project.join(".zpkg.toml");
    let raw = fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
}

fn read_lock(project: &Path) -> Result<Lockfile> {
    let path = project.join(".zpkg.lock");
    let raw = fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Lockfile::parse(&raw).map_err(anyhow::Error::msg)
}

fn github_repository(project: &Path) -> Result<String> {
    let remote = git(project, &["remote", "get-url", "origin"])?;
    parse_github_repository(&remote)
}

fn parse_github_repository(remote: &str) -> Result<String> {
    let remote = remote.trim().trim_end_matches(".git");
    let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = remote.strip_prefix("ssh://git@github.com/") {
        path
    } else if let Some(path) = remote.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = remote.strip_prefix("http://github.com/") {
        path
    } else {
        bail!("origin remote is not an unambiguous github.com repository: `{remote}`");
    };
    let Some((owner, name)) = path.split_once('/') else {
        bail!("GitHub remote must be owner/repo: `{remote}`");
    };
    ensure!(!owner.is_empty() && !name.is_empty() && !name.contains('/'), "GitHub remote must resolve to one owner/repo");
    Ok(format!("{owner}/{name}"))
}

fn discover_default_branch(project: &Path) -> Result<String> {
    if let Ok(value) = git(project, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        && let Some(branch) = value.strip_prefix("origin/")
    {
        return Ok(branch.to_owned());
    }
    git(project, &["branch", "--show-current"])
}

fn git(project: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .with_context(|| format!("failed to invoke git {}", args.join(" ")))?;
    ensure!(output.status.success(), "git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_supported_github_remote_forms() {
        for (remote, expected) in [
            ("git@github.com:acme/api.git", "acme/api"),
            ("https://github.com/acme/api.git", "acme/api"),
            ("ssh://git@github.com/acme/api", "acme/api"),
        ] {
            assert_eq!(parse_github_repository(remote).unwrap(), expected);
        }
    }

    #[test]
    fn refuses_ambiguous_or_non_github_remotes() {
        assert!(parse_github_repository("https://gitlab.com/acme/api.git").is_err());
        assert!(parse_github_repository("https://github.com/acme/nested/api.git").is_err());
    }
}