//! First-class project-owned CLI runtime installation.
//!
//! The user-facing `zed install --cli` route resolves a deliberately small,
//! repository-owned catalog into the shared `EnvironmentLock`, downloads only
//! exact HTTPS artifacts with locked sizes and SHA-256 values, and delegates
//! extraction/profile construction to the hardened tool-profile installer.
//! Copy mode keeps every runtime byte and command link below `.zed/tools`, so
//! an OCI stage can copy the project without copying Zed's global store.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use zed_interfaces::{
    ENVIRONMENT_LOCK_SCHEMA_VERSION, EnvironmentLock, EnvironmentLockValidationMode,
    LockedArtifact, LockedArtifactFormat, LockedExecutable, LockedInstall, LockedPlatform,
    LockedSource, LockedSourceKind, LockedTool,
};

use crate::cli::InstallMode;
use crate::pack::sha256_file;
use crate::project_lock;
use crate::store::Store;
use crate::tool_profile::{
    LockMode, ToolInstallReceipt, install_offline_with_mode, load_environment_lock,
};

const LOCK_PATH: &str = ".zed/environment.lock.toml";
const PROFILE_PATH: &str = ".zed/tools";
const STABLE_BIN_PATH: &str = ".zed/tools/bin";
const CATALOG_BACKEND: &str = "zed-catalog";
const CATALOG_BACKEND_VERSION: &str = "1";
const RAW_ARCHIVE_LAYOUT_EXTENSION: &str = "zed-pkg.archive-layout";

const NODE_VERSION: &str = "24.19.0";
const NODE_X86_64_SHA256: &str = "f625d97cd707df4ff96254916fbc5ff014f09c09effe5a1e0ca8f6d41a8789d4";
const NODE_X86_64_SIZE: u64 = 57_409_532;
const NODE_AARCH64_SHA256: &str =
    "d28c8a5bf0a808f0ed434a1dce8c54ae98f0371c0bd86ac58abc613f73e6643f";
const NODE_AARCH64_SIZE: u64 = 57_128_466;

const PYTHON_VERSION: &str = "3.14.7";
const PYTHON_BUILD: &str = "20260807";
const PYTHON_X86_64_SHA256: &str =
    "a2478d654ed51d443bae21ec20ad927f116b4f5aae4094ab74918a6aa38f0575";
const PYTHON_X86_64_SIZE: u64 = 35_940_499;
const PYTHON_AARCH64_SHA256: &str =
    "4dba8d7e06199f841a9d6b54e4eb58d446a5c20c65085a916190dd0162c6e93b";
const PYTHON_AARCH64_SIZE: u64 = 30_243_151;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestedTool {
    name: String,
    requirement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInstallReceipt {
    pub action: String,
    pub lock: String,
    pub lock_sha256: String,
    pub target: String,
    pub profile: String,
    pub bin: String,
    pub tools: Vec<String>,
}

pub fn install(
    project: &Path,
    home: &Path,
    specs: &[String],
    target: Option<&str>,
    frozen: bool,
    mode: InstallMode,
) -> Result<CliInstallReceipt> {
    ensure!(!specs.is_empty(), "--cli requires at least one tool name");
    ensure!(
        mode == InstallMode::Copy,
        "built-in CLI runtimes require --cli-install-mode copy so the complete runtime is project-owned"
    );
    let project = project
        .canonicalize()
        .with_context(|| format!("canonicalizing project `{}`", project.display()))?;
    ensure!(project.is_dir(), "project root must be a directory");
    ensure_lock_directory(&project)?;
    let target = target.map(str::to_owned).map_or_else(detect_target, Ok)?;
    let requested = specs
        .iter()
        .map(|spec| parse_requested_tool(spec))
        .collect::<Result<Vec<_>>>()?;

    if !frozen {
        project_lock::with_lock(&project, "resolve project CLI runtimes", || {
            update_lock(&project, &requested)
        })?;
    }

    let loaded = load_environment_lock(
        &project,
        Some(Path::new(LOCK_PATH)),
        LockMode::Portable,
        None,
    )?;
    if frozen {
        verify_frozen_requests(&loaded.lock, &requested)?;
    }
    download_target_artifacts(home, &loaded.lock, &target)?;
    let installed = install_offline_with_mode(
        &project,
        &loaded,
        &target,
        Some(Path::new(PROFILE_PATH)),
        home,
        mode,
    )?;
    project_lock::with_lock(&project, "activate project CLI runtime path", || {
        activate_stable_bin(&project, &target)
    })?;
    Ok(public_receipt(installed))
}

fn parse_requested_tool(input: &str) -> Result<RequestedTool> {
    let input = input.trim();
    ensure!(!input.is_empty(), "CLI tool name cannot be empty");
    let (raw_name, raw_requirement) = input
        .rsplit_once('@')
        .map_or((input, None), |(name, requirement)| {
            (name, Some(requirement))
        });
    let name = match raw_name.to_ascii_lowercase().as_str() {
        "node" | "nodejs" => "nodejs",
        "python" | "python3" => "python3",
        other => bail!("unsupported built-in CLI tool `{other}`; supported tools: nodejs, python3"),
    };
    let requirement = match name {
        "nodejs" => normalize_requirement(
            raw_requirement,
            NODE_VERSION,
            &["24", "24.19", "lts", "latest-lts"],
            name,
        )?,
        "python3" => normalize_requirement(raw_requirement, PYTHON_VERSION, &["3", "3.14"], name)?,
        _ => unreachable!(),
    };
    Ok(RequestedTool {
        name: name.to_string(),
        requirement,
    })
}

fn normalize_requirement(
    requested: Option<&str>,
    exact: &str,
    aliases: &[&str],
    tool: &str,
) -> Result<String> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    match requested {
        None | Some("latest") => Ok(exact.to_string()),
        Some(value) if value == exact || aliases.contains(&value) => Ok(exact.to_string()),
        Some(value) => bail!(
            "unsupported {tool} requirement `{value}`; this catalog currently resolves to {exact}"
        ),
    }
}

fn detect_target() -> Result<String> {
    ensure!(
        std::env::consts::OS == "linux",
        "built-in CLI runtimes currently support Linux GNU targets; pass --cli-target explicitly when cross-building"
    );
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("unsupported Linux architecture `{other}` for built-in CLI runtimes"),
    };
    let gnu_loader = match arch {
        "x86_64" => [
            Path::new("/lib64/ld-linux-x86-64.so.2"),
            Path::new("/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"),
        ],
        "aarch64" => [
            Path::new("/lib/ld-linux-aarch64.so.1"),
            Path::new("/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1"),
        ],
        _ => unreachable!(),
    };
    ensure!(
        gnu_loader.iter().any(|path| path.exists()),
        "built-in CLI runtimes are glibc-linked; use a GNU/Linux builder or pass a future supported --cli-target"
    );
    Ok(format!("{arch}-unknown-linux-gnu"))
}

fn update_lock(project: &Path, requested: &[RequestedTool]) -> Result<()> {
    let directory = ensure_lock_directory(project)?;
    let path = directory.join("environment.lock.toml");
    let mut lock = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "`{LOCK_PATH}` must be a regular project-owned file"
            );
            EnvironmentLock::parse_toml(&fs::read_to_string(&path)?)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EnvironmentLock {
            schema_version: ENVIRONMENT_LOCK_SCHEMA_VERSION,
            ..EnvironmentLock::default()
        },
        Err(error) => return Err(error).context("inspecting project CLI environment lock"),
    };
    for tool in requested {
        lock.tools
            .insert(tool.name.clone(), catalog_variants(tool)?);
    }
    lock.plan_digest_sha256 = plan_digest(&lock)?;
    lock.validate(EnvironmentLockValidationMode::Portable)?;
    let contents = lock.to_toml_string()?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .context("atomically writing project CLI environment lock")?;
    #[cfg(unix)]
    fs::File::open(&directory)?.sync_all()?;
    Ok(())
}

fn ensure_lock_directory(project: &Path) -> Result<std::path::PathBuf> {
    let directory = project.join(".zed");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "project CLI state directory `.zed` must be a real directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&directory).context("creating project CLI state directory")?;
        }
        Err(error) => return Err(error).context("inspecting project CLI state directory"),
    }
    Ok(directory)
}

fn verify_frozen_requests(lock: &EnvironmentLock, requested: &[RequestedTool]) -> Result<()> {
    for request in requested {
        let variants = lock.tools.get(&request.name).with_context(|| {
            format!(
                "frozen CLI lock does not contain `{}`; run without --frozen to add it",
                request.name
            )
        })?;
        ensure!(
            variants
                .iter()
                .all(|variant| variant.requirement == request.requirement),
            "frozen CLI requirement drift for `{}`: requested {}, lock records {}",
            request.name,
            request.requirement,
            variants
                .first()
                .map(|variant| variant.requirement.as_str())
                .unwrap_or("<none>")
        );
    }
    Ok(())
}

fn plan_digest(lock: &EnvironmentLock) -> Result<String> {
    let requirements = lock
        .tools
        .iter()
        .map(|(name, variants)| {
            let requirement = variants
                .first()
                .map(|variant| variant.requirement.clone())
                .unwrap_or_default();
            (name.clone(), requirement)
        })
        .collect::<BTreeMap<_, _>>();
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:cli-tools-plan:v1\0");
    hasher.update(serde_json::to_vec(&requirements)?);
    Ok(hex::encode(hasher.finalize()))
}

fn catalog_variants(request: &RequestedTool) -> Result<Vec<LockedTool>> {
    match request.name.as_str() {
        "nodejs" => Ok(vec![
            node_variant(request, "x86_64", NODE_X86_64_SHA256, NODE_X86_64_SIZE),
            node_variant(request, "aarch64", NODE_AARCH64_SHA256, NODE_AARCH64_SIZE),
        ]),
        "python3" => Ok(vec![
            python_variant(request, "x86_64", PYTHON_X86_64_SHA256, PYTHON_X86_64_SIZE),
            python_variant(
                request,
                "aarch64",
                PYTHON_AARCH64_SHA256,
                PYTHON_AARCH64_SIZE,
            ),
        ]),
        other => bail!("unsupported built-in CLI tool `{other}`"),
    }
}

fn node_variant(request: &RequestedTool, arch: &str, sha256: &str, size: u64) -> LockedTool {
    let upstream_arch = if arch == "x86_64" { "x64" } else { "arm64" };
    let archive = format!("node-v{NODE_VERSION}-linux-{upstream_arch}.tar.gz");
    let url = format!("https://nodejs.org/dist/v{NODE_VERSION}/{archive}");
    locked_variant(
        request,
        NODE_VERSION,
        arch,
        &url,
        sha256,
        size,
        &format!("node-v{NODE_VERSION}-linux-{upstream_arch}"),
        vec![
            executable("node", "bin/node", &["nodejs"]),
            executable("npm", "lib/node_modules/npm/bin/npm-cli.js", &[]),
            executable("npx", "lib/node_modules/npm/bin/npx-cli.js", &[]),
            executable(
                "corepack",
                "lib/node_modules/corepack/dist/corepack.js",
                &[],
            ),
        ],
    )
}

fn python_variant(request: &RequestedTool, arch: &str, sha256: &str, size: u64) -> LockedTool {
    let archive = format!(
        "cpython-{PYTHON_VERSION}+{PYTHON_BUILD}-{arch}-unknown-linux-gnu-install_only_stripped.tar.gz"
    );
    let encoded_archive = archive.replace('+', "%2B");
    let url = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{PYTHON_BUILD}/{encoded_archive}"
    );
    locked_variant(
        request,
        PYTHON_VERSION,
        arch,
        &url,
        sha256,
        size,
        "python",
        vec![
            executable("python3", "bin/python3.14", &["python", "python3.14"]),
            executable("pip3", "bin/pip3", &["pip", "pip3.14"]),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn locked_variant(
    request: &RequestedTool,
    resolved: &str,
    arch: &str,
    url: &str,
    sha256: &str,
    size: u64,
    root: &str,
    executables: Vec<LockedExecutable>,
) -> LockedTool {
    LockedTool {
        requirement: request.requirement.clone(),
        resolved: resolved.to_string(),
        backend: CATALOG_BACKEND.to_string(),
        backend_version: Some(CATALOG_BACKEND_VERSION.to_string()),
        backend_options_digest_sha256: None,
        source: LockedSource {
            kind: LockedSourceKind::Http,
            locator: url.to_string(),
            revision: None,
            tree_sha256: None,
            immutable: true,
            portable: false,
            extensions: BTreeMap::new(),
        },
        artifact: LockedArtifact {
            sha256: sha256.to_string(),
            size,
            format: LockedArtifactFormat::TarGz,
            mirrors: vec![url.to_string()],
            signatures: Vec::new(),
            extensions: BTreeMap::from([(
                RAW_ARCHIVE_LAYOUT_EXTENSION.to_string(),
                serde_json::Value::String("raw".to_string()),
            )]),
        },
        platform: LockedPlatform {
            target: format!("{arch}-unknown-linux-gnu"),
            os: Some("linux".to_string()),
            arch: Some(arch.to_string()),
            libc: Some("gnu".to_string()),
            abi: None,
        },
        install: LockedInstall {
            root: root.to_string(),
            bin_dirs: vec!["bin".to_string()],
            executables,
            layout_digest_sha256: None,
            extensions: BTreeMap::new(),
        },
        extensions: BTreeMap::new(),
    }
}

fn executable(name: &str, path: &str, aliases: &[&str]) -> LockedExecutable {
    LockedExecutable {
        name: name.to_string(),
        path: path.to_string(),
        aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
    }
}

fn download_target_artifacts(home: &Path, lock: &EnvironmentLock, target: &str) -> Result<()> {
    let store = Store::new(home);
    for (name, variants) in &lock.tools {
        let matching = variants
            .iter()
            .filter(|variant| variant.platform.target == target)
            .collect::<Vec<_>>();
        ensure!(
            matching.len() == 1,
            "tool `{name}` must have exactly one locked variant for `{target}`"
        );
        download_artifact(&store, name, matching[0])?;
    }
    Ok(())
}

fn download_artifact(store: &Store, tool: &str, locked: &LockedTool) -> Result<()> {
    let destination = store.cached_artifact(&locked.artifact.sha256);
    if destination.exists() {
        verify_download(&destination, tool, locked)?;
        return Ok(());
    }
    let url = locked
        .artifact
        .mirrors
        .first()
        .map(String::as_str)
        .unwrap_or(&locked.source.locator);
    let parsed = reqwest::Url::parse(url)?;
    ensure!(
        parsed.scheme() == "https",
        "tool `{tool}` artifact must use HTTPS"
    );
    let cache = destination
        .parent()
        .context("tool cache path has a parent")?;
    fs::create_dir_all(cache)?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client
        .get(parsed)
        .send()
        .with_context(|| format!("downloading locked CLI tool `{tool}`"))?
        .error_for_status()?;
    ensure!(
        response.url().scheme() == "https",
        "tool `{tool}` download redirected away from HTTPS"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(cache)?;
    let copied = std::io::copy(
        &mut response
            .by_ref()
            .take(locked.artifact.size.saturating_add(1)),
        &mut temporary,
    )?;
    ensure!(
        copied == locked.artifact.size,
        "tool `{tool}` artifact size mismatch: lock={}, download={copied}",
        locked.artifact.size
    );
    temporary.as_file().sync_all()?;
    verify_download(temporary.path(), tool, locked)?;
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if destination.exists() => {
            verify_download(&destination, tool, locked)?;
            drop(error.file);
        }
        Err(error) => return Err(error.error).context("persisting locked CLI tool artifact"),
    }
    Ok(())
}

fn verify_download(path: &Path, tool: &str, locked: &LockedTool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "cached artifact for `{tool}` must be a regular file"
    );
    let (sha256, size) = sha256_file(path)?;
    ensure!(
        sha256 == locked.artifact.sha256 && size == locked.artifact.size,
        "cached artifact for `{tool}` does not match its locked SHA-256 and size"
    );
    Ok(())
}

fn activate_stable_bin(project: &Path, target: &str) -> Result<()> {
    let tools = project.join(PROFILE_PATH);
    let active_bin = tools.join("v1").join(target).join("bin");
    ensure!(
        active_bin.is_dir(),
        "installed CLI profile has no bin directory"
    );
    let stable = project.join(STABLE_BIN_PATH);
    if let Ok(metadata) = fs::symlink_metadata(&stable) {
        ensure!(
            metadata.file_type().is_symlink(),
            "`{STABLE_BIN_PATH}` exists but is not a Zed-owned symlink"
        );
    }
    #[cfg(unix)]
    {
        let temporary = tools.join(format!(".bin.staging-{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(Path::new("v1").join(target).join("bin"), &temporary)?;
        fs::rename(&temporary, &stable)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        bail!("built-in project CLI runtime activation is not yet supported on Windows")
    }
}

fn public_receipt(receipt: ToolInstallReceipt) -> CliInstallReceipt {
    CliInstallReceipt {
        action: receipt.action,
        lock: receipt.lock,
        lock_sha256: receipt.lock_sha256,
        target: receipt.target,
        profile: receipt.profile,
        bin: STABLE_BIN_PATH.to_string(),
        tools: receipt
            .tools
            .into_iter()
            .map(|tool| format!("{}@{}", tool.name, tool.resolved))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_names_and_version_channels_normalize_to_exact_catalog_entries() {
        assert_eq!(
            parse_requested_tool("node").unwrap(),
            RequestedTool {
                name: "nodejs".to_string(),
                requirement: NODE_VERSION.to_string(),
            }
        );
        assert_eq!(
            parse_requested_tool("python3@3.14").unwrap(),
            RequestedTool {
                name: "python3".to_string(),
                requirement: PYTHON_VERSION.to_string(),
            }
        );
        assert!(parse_requested_tool("ruby").is_err());
        assert!(parse_requested_tool("nodejs@23").is_err());
    }

    #[test]
    fn catalog_lock_is_portable_multi_arch_and_checksum_exact() {
        let request = parse_requested_tool("nodejs").unwrap();
        let mut lock = EnvironmentLock {
            schema_version: ENVIRONMENT_LOCK_SCHEMA_VERSION,
            tools: BTreeMap::from([(request.name.clone(), catalog_variants(&request).unwrap())]),
            ..EnvironmentLock::default()
        };
        lock.plan_digest_sha256 = plan_digest(&lock).unwrap();
        for variant in &lock.tools["nodejs"] {
            assert_eq!(variant.artifact.sha256.len(), 64, "{variant:?}");
            assert!(
                variant
                    .artifact
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
                "{variant:?}"
            );
        }
        lock.validate(EnvironmentLockValidationMode::Portable)
            .unwrap();
        assert_eq!(lock.tools["nodejs"].len(), 2);
        assert!(
            lock.tools["nodejs"]
                .iter()
                .all(|variant| variant.artifact.extensions[RAW_ARCHIVE_LAYOUT_EXTENSION] == "raw")
        );
    }
}
