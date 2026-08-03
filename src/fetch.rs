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
    if source.starts_with("file:") {
        let url = reqwest::Url::parse(source)
            .map_err(|_| anyhow::anyhow!("frozen lockfile contains an invalid file registry source"))?;
        if url.scheme() != "file" {
            bail!("frozen lockfile contains an unsupported registry source scheme");
        }
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
        let path = url.path();
        if path.is_empty() || !path.starts_with('/') {
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
            "registry artifact format changed for {}@{} (lock {} vs registry {}); refusing",
            locked.full_name(),
            locked.version,
            locked.format,
            metadata.format
        );
    }
    if metadata.vcs_tag != locked.vcs_tag || metadata.vcs_commit != locked.vcs_commit {
        bail!(
            "registry VCS provenance changed for {}@{}; refusing frozen fetch",
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
    require_sha256(&metadata.sha256)?;
    if !is_slug(&metadata.org) || !is_slug(&metadata.name) {
        bail!("registry returned an invalid package identity");
    }
    if store.has(&metadata.sha256) {
        return Ok(store.pkg_dir(&metadata.sha256));
    }
    let cached = store.cached_artifact(&metadata.sha256);
    if !cached.exists() {
        registry.download(metadata, &cached)?;
    }
    store.add_artifact(&cached, &metadata.sha256)
}

fn prepare_output_path(
    requested_root: &Path,
    project: &Path,
    requested_output: &Path,
) -> Result<PathBuf> {
    if requested_output.as_os_str().is_empty()
        || requested_output
            .components()
            .any(|component| component == Component::ParentDir)
    {
        bail!("--output must be a non-empty path without `..` components");
    }

    let raw = if requested_output.is_absolute() {
        requested_output.to_path_buf()
    } else {
        requested_root.join(requested_output)
    };
    let project = fs::canonicalize(project)?;

    // Reject project-tree and project-ancestor destinations before inspecting
    // the parent, so the source tree remains an immutable input on every path.
    if raw.starts_with(&project) || project.starts_with(&raw) {
        bail!(
            "--output must be outside the project tree and may not contain it ({})",
            project.display()
        );
    }
    if fs::symlink_metadata(&raw).is_ok() {
        bail!("fetch output already exists: {}", raw.display());
    }

    let parent = raw.parent().context("fetch output has no parent")?;
    let parent_metadata = fs::metadata(parent).with_context(|| {
        format!(
            "fetch output parent must already exist and be a directory: {}",
            parent.display()
        )
    })?;
    if !parent_metadata.is_dir() {
        bail!(
            "fetch output parent must already exist and be a directory: {}",
            parent.display()
        );
    }
    let canonical_parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing fetch output parent {}", parent.display()))?;
    let name = raw
        .file_name()
        .filter(|name| !name.is_empty())
        .context("fetch output has no directory name")?;
    let output = canonical_parent.join(name);

    // A symlinked parent can redirect an apparently external path back into
    // the project. Canonicalize it before creating staging or final state.
    if output.starts_with(&project) || project.starts_with(&output) {
        bail!("canonical fetch output must remain outside the project tree");
    }
    if fs::symlink_metadata(&output).is_ok() {
        bail!("fetch output already exists: {}", output.display());
    }
    Ok(output)
}

fn copy_package_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        if entry.depth() == 0 {
            fs::create_dir_all(destination)?;
            continue;
        }
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            bail!(
                "verified store package unexpectedly contains a symlink at {}",
                relative.display()
            );
        }
        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
        } else if file_type.is_file() {
            fs::create_dir_all(target.parent().context("package file parent")?)?;
            fs::copy(entry.path(), &target)?;
        } else {
            bail!(
                "verified store package contains unsupported special file {}",
                relative.display()
            );
        }
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn route(args: &[OsString]) -> Route {
    let help_requested = args
        .iter()
        .skip(1)
        .any(|value| value == OsStr::new("--help") || value == OsStr::new("-h"));
    let Some((command_index, command)) = first_command(args) else {
        return if help_requested {
            Route::RootHelp
        } else {
            Route::Existing
        };
    };

    match command.as_str() {
        "fetch" => Route::Fetch,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "fetch" => Route::FetchHelp {
                help_index: command_index,
                target_index,
            },
            None => Route::RootHelp,
            _ => Route::Existing,
        },
        _ => Route::Existing,
    }
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if token == "--" {
            return next_positional(args, index + 1);
        }
        if global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, token.into_owned()));
    }
    None
}

fn next_positional(args: &[OsString], mut index: usize) -> Option<(usize, String)> {
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if !token.starts_with('-') {
            return Some((index, token.into_owned()));
        }
        index += 1;
    }
    None
}

fn global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--token",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>> {
    args.iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags-2-env requires UTF-8 command-line arguments")
        })
        .collect()
}

fn validate_fetch_flags(argv: &[String]) -> Result<()> {
    let parser_argv: Vec<String> = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect();
    let parsed = parse_embedded(&parser_argv)?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed fetch option(s): {}",
            parsed
                .unknown_options
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid zed fetch value(s): {}",
            parsed
                .errors
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

fn parse_embedded(argv: &[String]) -> Result<flags2env::StructuredParse> {
    let contract_dir = tempfile::tempdir().context("creating zed fetch flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, FETCH_CONTRACT).context("writing embedded zed fetch contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded zed fetch contract path is not valid UTF-8")?;

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed fetch flags2env audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed fetch flags2env parse failed: {error}"))
}

fn normalize_boolean_environment() -> Result<()> {
    for key in ["ZED_PKG_FROZEN", "ZED_PKG_INTERACTIVE"] {
        let Some(raw) = env::var_os(key) else {
            continue;
        };
        let raw = raw
            .to_str()
            .with_context(|| format!("boolean environment variable `{key}` is not UTF-8"))?;
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "true",
            "false" | "0" | "no" | "off" => "false",
            _ => bail!(
                "boolean environment variable `{key}` must be true/false, 1/0, yes/no, or on/off"
            ),
        };
        if raw != normalized {
            // SAFETY: this runs once at process startup before worker threads.
            unsafe { env::set_var(key, normalized) };
        }
    }
    Ok(())
}

fn redact_option_value(value: &str) -> String {
    match value.split_once('=') {
        Some((option, _)) if option.starts_with('-') => format!("{option}=<redacted>"),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_urls_are_accepted_without_platform_conversion() {
        assert!(validate_source("file:///tmp/zed-registry").is_ok());
        assert!(validate_source("file:///nix/store/example-zed-registry").is_ok());
    }

    #[test]
    fn non_local_or_secret_bearing_file_urls_fail_without_echoing_values() {
        for source in [
            "file://remote.invalid/private/registry",
            "file:///tmp/registry?token=super-secret",
            "file:///tmp/registry#private-fragment",
        ] {
            let error = validate_source(source).unwrap_err().to_string();
            assert!(!error.contains("super-secret"));
            assert!(!error.contains("private-fragment"));
            assert!(!error.contains("remote.invalid"));
        }
    }

    #[test]
    fn fetch_route_remains_modular() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            route(&args(&["zed", "fetch", "--frozen", "--output", "x"])),
            Route::Fetch
        );
        assert!(matches!(
            route(&args(&["zed", "help", "fetch"])),
            Route::FetchHelp { .. }
        ));
        assert_eq!(route(&args(&["zed", "install"])), Route::Existing);
    }
}
