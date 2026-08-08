//! Resolver-only export of a frozen Zed dependency graph.
//!
//! `zed fetch --frozen --output <dir>` verifies the existing `.zpkg.lock`,
//! downloads and extracts each exact artifact through the normal hardened
//! registry/store boundary, and atomically publishes a content-addressed
//! bundle. It never creates project adapters, project references, build-cache
//! output, `zed_modules/`, or a rewritten lockfile.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use flags2env::BundledFlags2Env;
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::is_slug;
use zed_interfaces::paths::LOCKFILE_FILE;
use zed_interfaces::registry::VersionMetadata;

use crate::cli::Globals;
use crate::config::Config;
use crate::registry::{Registry, registry_for};
use crate::store::{Store, require_sha256};

const FETCH_CONTRACT: &str = include_str!("../.fetch-cli-flags.toml");
const FETCH_SCHEMA: &str = "zed.fetch/v1";

#[derive(Debug, Clone, Args)]
pub struct FetchArgs {
    /// Use `.zpkg.lock` as the sole dependency authority. Version 1 requires
    /// this explicit opt-in so the command can never silently resolve latest
    /// versions or mutate the lock.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    pub frozen: bool,

    /// New directory to create atomically. It must be outside the project tree
    /// and must not already exist.
    #[arg(long, env = "ZED_PKG_FETCH_OUTPUT", value_name = "DIR")]
    pub output: PathBuf,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct FetchCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: FetchCommand,
}

#[derive(Debug, Subcommand)]
enum FetchCommand {
    /// Verify and export exactly the artifact graph pinned by `.zpkg.lock`.
    Fetch(FetchArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Fetch,
    FetchHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    pub output: PathBuf,
    pub packages: usize,
    pub lock_sha256: String,
}

#[derive(Debug, Serialize)]
struct FetchIndex {
    schema: &'static str,
    lock_sha256: String,
    packages: Vec<FetchedPackage>,
}

#[derive(Debug, Serialize)]
struct FetchedPackage {
    org: String,
    name: String,
    version: String,
    sha256: String,
    size: u64,
    format: String,
    vcs_tag: String,
    vcs_commit: Option<String>,
    source_kind: &'static str,
    path: String,
}

/// Route only the modular `fetch` command and augmented root help here. Every
/// established command continues through the repository's existing `Cli` enum.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Fetch => Some(run_cli(args)),
        Route::FetchHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("fetch");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add the modular command to top-level help and generated shell completions.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "fetch")
    {
        return command;
    }

    let fetch = <FetchArgs as Args>::augment_args(
        clap::Command::new("fetch")
            .about("Verify and export exactly the artifact graph pinned by .zpkg.lock"),
    );
    command.subcommand(fetch)
}

fn print_root_help() -> Result<()> {
    let mut command =
        augment_root_command(crate::dev::augment_root_command(crate::cli::Cli::command()));
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let string_args = utf8_args(&args)?;
    normalize_boolean_environment()?;
    validate_fetch_flags(&string_args)?;

    let cli = match FetchCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().context("printing zed fetch argument error")?;
            return Ok(code);
        }
    };

    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = env::current_dir().context("reading the current directory")?;
    match cli.command {
        FetchCommand::Fetch(options) => {
            let report = run(&cwd, &cfg, options)?;
            println!(
                "fetched {} package(s) to {} (lock sha256 {})",
                report.packages,
                report.output.display(),
                report.lock_sha256
            );
            Ok(0)
        }
    }
}

/// Verify and export the exact graph in the nearest ancestor lockfile.
///
/// The output is intentionally not installation-shaped. It contains immutable
/// package payloads addressed by artifact digest plus a source-redacted index:
///
/// ```text
/// output/
/// ├── packages/<sha256>/pkg/...
/// └── metadata/
///     ├── index.json
///     ├── lock.sha256
///     └── zed-version.txt
/// ```
pub fn run(requested_root: &Path, cfg: &Config, options: FetchArgs) -> Result<FetchReport> {
    if !options.frozen {
        bail!("`zed fetch` version 1 is frozen-only; pass --frozen or set ZED_PKG_FROZEN=1");
    }

    let requested_root = fs::canonicalize(requested_root)
        .with_context(|| format!("reading invocation directory {}", requested_root.display()))?;
    let project = lock_root(&requested_root).with_context(|| {
        format!(
            "`zed fetch --frozen` requires {LOCKFILE_FILE} at or above {}",
            requested_root.display()
        )
    })?;
    let lock_path = project.join(LOCKFILE_FILE);
    let lock_bytes = fs::read(&lock_path)
        .with_context(|| format!("reading frozen lockfile {}", lock_path.display()))?;
    let lock_text = std::str::from_utf8(&lock_bytes)
        .with_context(|| format!("{} is not UTF-8", lock_path.display()))?;
    let lock = Lockfile::parse(lock_text).context("parsing frozen lockfile")?;
    let packages = validate_locked_packages(&lock, &cfg.registry)?;
    let lock_sha256 = sha256_bytes(&lock_bytes);

    let output = prepare_output_path(&requested_root, &project, &options.output)?;
    let parent = output.parent().context("fetch output has no parent")?;

    // Both temporary directories live beside the final output, so the final
    // directory rename is atomic on one filesystem. The temporary Store is
    // isolated from `ZED_PKG_HOME` and is removed on success or failure.
    let staging = tempfile::Builder::new()
        .prefix(".zed-fetch-bundle-")
        .tempdir_in(parent)
        .context("creating atomic fetch staging directory")?;
    let isolated_store = tempfile::Builder::new()
        .prefix(".zed-fetch-store-")
        .tempdir_in(parent)
        .context("creating isolated fetch store")?;
    let store = Store::new(isolated_store.path());

    fs::create_dir_all(staging.path().join("packages"))?;
    fs::create_dir_all(staging.path().join("metadata"))?;

    let mut registries: BTreeMap<String, Box<dyn Registry>> = BTreeMap::new();
    let mut fetched = Vec::with_capacity(packages.len());
    for locked in &packages {
        let source = effective_source(locked, &cfg.registry);
        if !registries.contains_key(source) {
            registries.insert(source.to_string(), registry_for(source)?);
        }
        let registry = registries
            .get(source)
            .context("registry cache lost a just-inserted source")?;
        let metadata = registry
            .get_version(&locked.org, &locked.name, &locked.version)
            .with_context(|| {
                format!(
                    "reading frozen registry record for {}@{}",
                    locked.full_name(),
                    locked.version
                )
            })?;
        verify_registry_metadata(locked, &metadata)?;
        let package = ensure_artifact(registry.as_ref(), &store, &metadata)?;

        let relative = PathBuf::from("packages").join(&locked.sha256).join("pkg");
        let destination = staging.path().join(&relative);
        if !destination.exists() {
            copy_package_tree(&package, &destination)?;
        }

        fetched.push(FetchedPackage {
            org: locked.org.clone(),
            name: locked.name.clone(),
            version: locked.version.clone(),
            sha256: locked.sha256.clone(),
            size: locked.size,
            format: locked.format.to_string(),
            vcs_tag: locked.vcs_tag.clone(),
            vcs_commit: locked.vcs_commit.clone(),
            source_kind: source_kind(source),
            path: relative.to_string_lossy().replace('\\', "/"),
        });
    }

    let index = FetchIndex {
        schema: FETCH_SCHEMA,
        lock_sha256: lock_sha256.clone(),
        packages: fetched,
    };
    fs::write(
        staging.path().join("metadata/index.json"),
        format!("{}\n", serde_json::to_string_pretty(&index)?),
    )?;
    fs::write(
        staging.path().join("metadata/lock.sha256"),
        format!("{lock_sha256}  {LOCKFILE_FILE}\n"),
    )?;
    fs::write(
        staging.path().join("metadata/zed-version.txt"),
        format!("{}\n", env!("CARGO_PKG_VERSION")),
    )?;

    let staged_path = staging.keep();
    if let Err(error) = fs::rename(&staged_path, &output) {
        let _ = fs::remove_dir_all(&staged_path);
        return Err(error).with_context(|| {
            format!(
                "atomically publishing frozen fetch bundle to {}",
                output.display()
            )
        });
    }

    Ok(FetchReport {
        output,
        packages: packages.len(),
        lock_sha256,
    })
}

fn lock_root(requested: &Path) -> Option<PathBuf> {
    requested
        .ancestors()
        .find(|candidate| candidate.join(LOCKFILE_FILE).is_file())
        .map(Path::to_path_buf)
}

fn validate_locked_packages(
    lock: &Lockfile,
    fallback_registry: &str,
) -> Result<Vec<LockedPackage>> {
    let mut packages = lock.packages.clone();
    packages.sort_by(|left, right| {
        (&left.org, &left.name, &left.version, &left.sha256).cmp(&(
            &right.org,
            &right.name,
            &right.version,
            &right.sha256,
        ))
    });

    let mut identities = BTreeSet::new();
    for package in &packages {
        if !is_slug(&package.org) || !is_slug(&package.name) {
            bail!(
                "lockfile entry `{}/{}` has an invalid package identity",
                package.org,
                package.name
            );
        }
        require_sha256(&package.sha256)?;
        if package.version.trim().is_empty() {
            bail!(
                "lockfile entry `{}` has an empty version",
                package.full_name()
            );
        }
        if !identities.insert((package.org.clone(), package.name.clone())) {
            bail!(
                "lockfile contains duplicate package identity `{}`; refusing an ambiguous frozen graph",
                package.full_name()
            );
        }
        validate_source(effective_source(package, fallback_registry))?;
    }
    Ok(packages)
}

fn effective_source<'a>(package: &'a LockedPackage, fallback_registry: &'a str) -> &'a str {
    let source = package.source.trim();
    if source.is_empty() {
        fallback_registry.trim_end_matches('/')
    } else {
        source.trim_end_matches('/')
    }
}

fn validate_source(source: &str) -> Result<()> {
    if source.starts_with("file://") {
        let url = reqwest::Url::parse(source).map_err(|_| {
            anyhow::anyhow!("frozen lockfile contains an invalid file registry source")
        })?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "frozen file registry sources may not embed credentials, query strings, or fragments"
            );
        }
        if url.host_str().is_some() {
            bail!("frozen file registry source is not a local absolute path");
        }
        let path = url.to_file_path().map_err(|_| {
            anyhow::anyhow!("frozen file registry source is not a local absolute path")
        })?;
        if !path.is_absolute() {
            bail!("frozen file registry source is not a local absolute path");
        }
        return Ok(());
    }
    if source.starts_with("https://") || source.starts_with("http://") {
        let url = reqwest::Url::parse(source).map_err(|_| {
            anyhow::anyhow!("frozen lockfile contains an invalid HTTP registry source")
        })?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "frozen registry sources may not embed credentials, query strings, or fragments; use an explicit secret-delivery mechanism"
            );
        }
        return Ok(());
    }
    bail!("frozen lockfile contains an unsupported registry source scheme")
}

fn source_kind(source: &str) -> &'static str {
    if let Some(path) = source.strip_prefix("file://") {
        if Path::new(path).starts_with("/nix/store") {
            "immutable-nix-store-input"
        } else {
            "file"
        }
    } else if source.starts_with("https://") {
        "https"
    } else if source.starts_with("http://") {
        "http"
    } else {
        "other"
    }
}

fn verify_registry_metadata(locked: &LockedPackage, metadata: &VersionMetadata) -> Result<()> {
    if metadata.org != locked.org
        || metadata.name != locked.name
        || metadata.version != locked.version
    {
        bail!(
            "registry identity changed for {}@{}; refusing frozen fetch",
            locked.full_name(),
            locked.version
        );
    }
    require_sha256(&metadata.sha256)?;
    if metadata.sha256 != locked.sha256 {
        bail!(
            "registry artifact digest changed for {}@{} (lock {} vs registry {}); refusing",
            locked.full_name(),
            locked.version,
            locked.sha256,
            metadata.sha256
        );
    }
    if metadata.size != locked.size {
        bail!(
            "registry artifact size changed for {}@{} (lock {} vs registry {}); refusing",
            locked.full_name(),
            locked.version,
            locked.size,
            metadata.size
        );
    }
    if metadata.format != locked.format {
        bail!(
            "registry artifact format changed for {}@{}; refusing",
            locked.full_name(),
            locked.version
        );
    }
    if metadata.vcs_tag != locked.vcs_tag || metadata.vcs_commit != locked.vcs_commit {
        bail!(
            "registry provenance changed for {}@{}; refusing frozen fetch",
            locked.full_name(),
            locked.version
        );
    }
    Ok(())
}

fn ensure_artifact(
    registry: &dyn Registry,
    store: &Store,
    metadata: &VersionMetadata,
) -> Result<PathBuf> {
    if store.has_package(&metadata.sha256) {
        return Ok(store.package_dir(&metadata.sha256));
    }
    let artifact = registry.download_artifact(metadata)?;
    let expected = require_sha256(&metadata.sha256)?;
    store.verify_artifact(&artifact, &expected, metadata.size)?;
    store.extract_verified(&artifact, metadata.format, &metadata.sha256)
}

fn prepare_output_path(requested: &Path, project: &Path, output: &Path) -> Result<PathBuf> {
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        requested.join(output)
    };
    let parent = absolute
        .parent()
        .context("fetch output must have an existing parent directory")?;
    let canonical_parent = fs::canonicalize(parent).with_context(|| {
        format!(
            "fetch output parent must already exist and be a directory: {}",
            parent.display()
        )
    })?;
    if !canonical_parent.is_dir() {
        bail!(
            "fetch output parent must already exist and be a directory: {}",
            parent.display()
        );
    }
    let file_name = absolute
        .file_name()
        .filter(|name| !name.is_empty())
        .context("fetch output must name a new directory")?;
    if file_name == OsStr::new(".") || file_name == OsStr::new("..") {
        bail!("fetch output must name a new directory");
    }
    let canonical_output = canonical_parent.join(file_name);
    if canonical_output.exists() {
        bail!(
            "fetch output already exists; refusing to overwrite {}",
            canonical_output.display()
        );
    }
    if canonical_output.starts_with(project) || project.starts_with(&canonical_output) {
        bail!(
            "canonical fetch output must be outside the project tree: {}",
            canonical_output.display()
        );
    }
    Ok(canonical_output)
}

fn copy_package_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "verified package tree contains a symlink at {}; refusing frozen export",
                relative.display()
            );
        }
        if metadata.is_dir() {
            fs::create_dir_all(&target)?;
        } else if metadata.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn source_kind(source: &str) -> &'static str {
    if let Some(path) = source.strip_prefix("file://") {
        if Path::new(path).starts_with("/nix/store") {
            "immutable-nix-store-input"
        } else {
            "file"
        }
    } else if source.starts_with("https://") {
        "https"
    } else if source.starts_with("http://") {
        "http"
    } else {
        "other"
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn validate_fetch_flags(args: &[String]) -> Result<()> {
    let parser = BundledFlags2Env::new();
    parser
        .audit_config_from_str(FETCH_CONTRACT)
        .context("auditing embedded zed fetch flags contract")?;
    let parsed = parser
        .parse_structured_from_str(args, FETCH_CONTRACT)
        .context("parsing zed fetch arguments through flags-2-env")?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "unknown zed fetch option(s): {}",
            parsed.unknown_options.join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!("invalid zed fetch arguments: {}", parsed.errors.join("; "));
    }
    for (key, value) in parsed.provided_flags {
        if matches!(key.as_str(), "ZED_PKG_FROZEN" | "ZED_PKG_FETCH_OUTPUT") {
            // SAFETY: fetch CLI parsing is single-threaded and completes before
            // any worker is started.
            unsafe { env::set_var(key, value) };
        }
    }
    Ok(())
}

fn normalize_boolean_environment() -> Result<()> {
    if let Ok(value) = env::var("ZED_PKG_FROZEN") {
        let normalized = match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => "true",
            "0" | "false" | "no" | "off" | "" => "false",
            _ => bail!("ZED_PKG_FROZEN must be a boolean"),
        };
        // SAFETY: fetch CLI parsing is single-threaded and completes before any
        // worker is started.
        unsafe { env::set_var("ZED_PKG_FROZEN", normalized) };
    }
    Ok(())
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>> {
    args.iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .context("zed fetch arguments must be UTF-8")
        })
        .collect()
}

fn option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--home"
            | "--registry"
            | "--cache"
            | "--jobs"
            | "--color"
            | "--log-level"
            | "--lock-timeout"
            | "--lock-timeout-seconds"
            | "--output"
    )
}

fn is_help(argument: &str) -> bool {
    matches!(argument, "-h" | "--help")
}

fn route(args: &[OsString]) -> Route {
    let strings = args
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>();
    let mut index = 1;
    let mut root_help = None;
    while index < strings.len() {
        let argument = strings[index].as_ref();
        if is_help(argument) {
            root_help = Some(index);
            index += 1;
            continue;
        }
        if argument == "fetch" {
            if let Some(help_index) = root_help {
                return Route::FetchHelp {
                    help_index,
                    target_index: index,
                };
            }
            return Route::Fetch;
        }
        if argument.starts_with('-') {
            if !argument.contains('=') && option_takes_value(argument) {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        return Route::Existing;
    }
    if root_help.is_some() {
        Route::RootHelp
    } else {
        Route::Existing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_package_identity() {
        let package = LockedPackage {
            org: "acme".into(),
            name: "demo".into(),
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            size: 1,
            format: zed_interfaces::artifact::ArtifactFormat::TarGz,
            vcs_tag: "v1.0.0".into(),
            vcs_commit: None,
            source: "https://registry.example.com".into(),
        };
        let lock = Lockfile {
            version: Lockfile::CURRENT_VERSION,
            packages: vec![package.clone(), package],
            native_dependencies: Vec::new(),
            nix_adapters: Vec::new(),
        };
        let error = validate_locked_packages(&lock, "https://registry.example.com")
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate package identity"));
    }

    #[test]
    fn rejects_malformed_sha256_before_network_access() {
        let lock = Lockfile {
            version: Lockfile::CURRENT_VERSION,
            packages: vec![LockedPackage {
                org: "acme".into(),
                name: "demo".into(),
                version: "1.0.0".into(),
                sha256: "not-a-digest".into(),
                size: 1,
                format: zed_interfaces::artifact::ArtifactFormat::TarGz,
                vcs_tag: "v1.0.0".into(),
                vcs_commit: None,
                source: "https://registry.example.com".into(),
            }],
            native_dependencies: Vec::new(),
            nix_adapters: Vec::new(),
        };
        assert!(
            validate_locked_packages(&lock, "https://registry.example.com")
                .unwrap_err()
                .to_string()
                .contains("64 lowercase hexadecimal")
        );
    }

    #[test]
    fn rejects_file_registry_authorities_without_echoing_source() {
        let source = "file://remote-registry.invalid/private/path";
        let error = validate_source(source).unwrap_err().to_string();
        assert!(error.contains("not a local absolute path"));
        assert!(!error.contains(source));
    }

    #[test]
    fn rejects_file_registry_query_without_echoing_secret() {
        let secret = "super-secret-query-value";
        let source = format!("file:///tmp/registry?token={secret}");
        let error = validate_source(&source).unwrap_err().to_string();
        assert!(error.contains("may not embed"));
        assert!(!error.contains(secret));
    }

    #[test]
    fn verifies_registry_metadata_against_lock() {
        let locked = LockedPackage {
            org: "acme".into(),
            name: "demo".into(),
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            size: 10,
            format: zed_interfaces::artifact::ArtifactFormat::TarGz,
            vcs_tag: "v1.0.0".into(),
            vcs_commit: Some("b".repeat(40)),
            source: "https://registry.example.com".into(),
        };
        let mut metadata = VersionMetadata {
            org: "acme".into(),
            name: "demo".into(),
            version: "1.0.0".into(),
            sha256: "a".repeat(64),
            size: 10,
            format: zed_interfaces::artifact::ArtifactFormat::TarGz,
            artifact_url: "https://registry.example.com/acme/demo/1.0.0.tar.gz".into(),
            vcs_tag: "v1.0.0".into(),
            vcs_commit: Some("b".repeat(40)),
        };
        verify_registry_metadata(&locked, &metadata).unwrap();
        metadata.size = 11;
        assert!(
            verify_registry_metadata(&locked, &metadata)
                .unwrap_err()
                .to_string()
                .contains("size changed")
        );
    }

    #[test]
    fn output_path_must_be_new_and_outside_project() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let external = root.path().join("external");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&external).unwrap();

        let inside = project.join("bundle");
        assert!(prepare_output_path(&project, &project, &inside).is_err());

        let outside = external.join("bundle");
        assert_eq!(
            prepare_output_path(&project, &project, &outside).unwrap(),
            outside
        );
        fs::create_dir(&outside).unwrap();
        assert!(prepare_output_path(&project, &project, &outside).is_err());
    }

    #[test]
    fn lock_root_walks_to_the_nearest_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.path().join(LOCKFILE_FILE), "version = 2\n").unwrap();
        assert_eq!(lock_root(&nested).unwrap(), root.path());
    }

    #[test]
    fn root_help_is_routed_to_the_existing_cli_with_fetch_augmented() {
        assert_eq!(
            route(&[OsString::from("zed"), OsString::from("--help")]),
            Route::RootHelp
        );
        assert_eq!(
            route(&[
                OsString::from("zed"),
                OsString::from("--help"),
                OsString::from("fetch")
            ]),
            Route::FetchHelp {
                help_index: 1,
                target_index: 2
            }
        );
    }

    #[test]
    fn unrelated_commands_remain_on_the_existing_parser() {
        assert_eq!(
            route(&[OsString::from("zed"), OsString::from("install")]),
            Route::Existing
        );
    }

    #[test]
    fn fetch_command_is_detected_after_global_options() {
        assert_eq!(
            route(&[
                OsString::from("zed"),
                OsString::from("--home"),
                OsString::from("/tmp/home"),
                OsString::from("fetch"),
                OsString::from("--frozen"),
                OsString::from("--output"),
                OsString::from("/tmp/out"),
            ]),
            Route::Fetch
        );
    }

    #[test]
    fn embedded_fetch_contract_stays_auditable() {
        BundledFlags2Env::new()
            .audit_config_from_str(FETCH_CONTRACT)
            .unwrap();
    }
}