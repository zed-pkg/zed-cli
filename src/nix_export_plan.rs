//! Read-only planning for deterministic Zed → Nix artifact exports.
//!
//! Version 1 deliberately plans only dependency-free, no-build packages and
//! prebuilt executables already present in the immutable Zed artifact. The
//! plan hashes exact manifest/lock bytes and the deterministic packed artifact,
//! but never retains credentials, registry URLs, absolute workspace paths,
//! timestamps, hostnames, usernames, or mutable resolution state.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use flags2env::BundledFlags2Env;
use flate2::read::GzDecoder;
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::manifest::is_sha256_hex;
use zed_interfaces::paths::{ARCHIVE_ROOT, LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::{
    Lockfile, Manifest, NixBuilderNetwork, NixExportMode, NixExportSection, NixInteropArtifact,
    NixPackageIdentity, NixPolicyEvidence, NixPolicyProfile,
};

use crate::cli::Globals;
use crate::pack::{PackResult, pack, pack_all};

const INTEROP_CONTRACT: &str = include_str!("../.nix-interop-cli-flags.toml");
pub const NIX_EXPORT_PLAN_SCHEMA_V1: &str = "zed.nix-export-plan/v1";

#[derive(Debug, Clone, Args)]
pub struct NixExportPlanArgs {
    /// Require the existing `.zpkg.lock` as the only dependency authority.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    pub frozen: bool,

    /// Print canonical compact JSON instead of the human summary.
    #[arg(long, env = "ZED_PKG_NIX_PLAN_JSON")]
    pub json: bool,

    /// Explicit polyglot target. Single-language packages reject this flag.
    #[arg(long, env = "ZED_PKG_NIX_TARGET", value_name = "TARGET")]
    pub target: Option<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct InteropCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: InteropRoot,
}

#[derive(Debug, Subcommand)]
enum InteropRoot {
    /// Typed interoperability commands.
    Interop(InteropArgs),
}

#[derive(Debug, Args)]
struct InteropArgs {
    #[command(subcommand)]
    ecosystem: InteropEcosystem,
}

#[derive(Debug, Subcommand)]
enum InteropEcosystem {
    /// Nix planning, export, import, and verification.
    Nix(NixArgs),
}

#[derive(Debug, Args)]
struct NixArgs {
    #[command(subcommand)]
    command: NixCommand,
}

#[derive(Debug, Subcommand)]
enum NixCommand {
    /// Produce a read-only, credential-free operation plan.
    Plan(NixPlanArgs),
}

#[derive(Debug, Args)]
struct NixPlanArgs {
    #[command(subcommand)]
    command: NixPlanCommand,
}

#[derive(Debug, Subcommand)]
enum NixPlanCommand {
    /// Plan deterministic artifact-only Zed → Nix export.
    Export(NixExportPlanArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Interop,
    InteropHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannedPackageClass {
    Data,
    PrebuiltBin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedNixIntent {
    pub mode: NixExportMode,
    pub attribute: String,
    pub systems: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedZedArtifact {
    pub file_name: String,
    pub artifact: NixInteropArtifact,
    pub manifest_sha256: String,
    pub lock_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NixExportPlan {
    pub schema: &'static str,
    pub package: NixPackageIdentity,
    pub package_class: PlannedPackageClass,
    pub intent: ResolvedNixIntent,
    pub source: PlannedZedArtifact,
    pub bins: BTreeMap<String, String>,
    pub dependencies: Vec<PlannedDependency>,
    pub policy: NixPolicyEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedDependency {
    pub org: String,
    pub name: String,
    pub version: String,
    pub sha256: String,
}

impl NixExportPlan {
    pub fn validate(&self) -> Result<()> {
        if self.schema != NIX_EXPORT_PLAN_SCHEMA_V1 {
            bail!("unsupported Nix export plan schema `{}`", self.schema);
        }
        self.package
            .validate()
            .map_err(|error| anyhow::anyhow!(error))?;
        if self.intent.mode != NixExportMode::Artifact {
            bail!("Nix export plan v1 supports only artifact mode");
        }
        let intent = NixExportSection {
            mode: self.intent.mode,
            attribute: Some(self.intent.attribute.clone()),
            systems: self.intent.systems.clone(),
            outputs: self.intent.outputs.clone(),
        };
        intent
            .validate(&self.package.name)
            .map_err(|error| anyhow::anyhow!(error))?;
        self.source
            .artifact
            .validate("planned Zed artifact")
            .map_err(|error| anyhow::anyhow!(error))?;
        if !is_sha256_hex(&self.source.manifest_sha256) || !is_sha256_hex(&self.source.lock_sha256)
        {
            bail!("planned manifest and lock digests must be lowercase SHA-256 hex");
        }
        let file_name = Path::new(&self.source.file_name);
        if file_name.file_name() != Some(file_name.as_os_str())
            || self.source.file_name.starts_with('.')
        {
            bail!("planned artifact filename must be one safe basename");
        }
        if !self.dependencies.is_empty() {
            bail!("Nix export plan v1 currently accepts dependency-free packages only");
        }
        self.policy
            .validate()
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(())
    }

    /// Stable compact JSON for hashing, review, and later export execution.
    pub fn canonical_json_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).context("serializing canonical Nix export plan")
    }

    pub fn canonical_json_string(&self) -> Result<String> {
        String::from_utf8(self.canonical_json_bytes()?)
            .context("canonical Nix export plan was not UTF-8")
    }
}

struct SelectedPackage {
    target: Option<String>,
    manifest: Manifest,
    intent: NixExportSection,
    source_root: PathBuf,
}

/// Route only the modular `interop` family here. Existing commands continue
/// through the repository's established Clap command enum.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Interop => Some(run_cli(args)),
        Route::InteropHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("interop");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add the nested `interop nix plan export` command to root help/completions.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "interop")
    {
        return command;
    }
    let interop = InteropCli::command()
        .find_subcommand("interop")
        .expect("interop command is declared")
        .clone();
    command.subcommand(interop)
}

fn print_root_help() -> Result<()> {
    let mut command = augment_root_command(crate::fetch::augment_root_command(
        crate::dev::augment_root_command(crate::cli::Cli::command()),
    ));
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let string_args = utf8_args(&args)?;
    normalize_boolean_environment()?;
    validate_interop_flags(&string_args)?;

    let cli = match InteropCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .context("printing Nix interop argument error")?;
            return Ok(code);
        }
    };

    // Planning is intentionally credential- and home-independent. Global
    // settings are accepted for CLI consistency but never converted to Config,
    // accessed, serialized, or used to resolve mutable registry state.
    let InteropCli {
        globals: _,
        command,
    } = cli;
    let InteropRoot::Interop(interop) = command;
    let InteropEcosystem::Nix(nix) = interop.ecosystem;
    let NixCommand::Plan(plan) = nix.command;
    let NixPlanCommand::Export(options) = plan.command;

    let cwd = env::current_dir().context("reading current directory")?;
    let planned = build_plan(&cwd, &options)?;
    if options.json {
        println!("{}", planned.canonical_json_string()?);
    } else {
        print_human_plan(&planned);
    }
    Ok(0)
}

pub fn build_plan(requested_root: &Path, options: &NixExportPlanArgs) -> Result<NixExportPlan> {
    if !options.frozen {
        bail!(
            "`zed interop nix plan export` version 1 is frozen-only; pass --frozen or set ZED_PKG_FROZEN=1"
        );
    }

    let requested_root = fs::canonicalize(requested_root)
        .with_context(|| format!("reading invocation directory {}", requested_root.display()))?;
    let project = project_root(&requested_root).with_context(|| {
        format!(
            "Nix export planning requires {MANIFEST_FILE} at or above {}",
            requested_root.display()
        )
    })?;

    let manifest_path = project.join(MANIFEST_FILE);
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .with_context(|| format!("{} is not UTF-8", manifest_path.display()))?;
    let manifest = Manifest::parse(manifest_text).context("validating Nix export manifest")?;

    let lock_path = project.join(LOCKFILE_FILE);
    let lock_bytes = fs::read(&lock_path).with_context(|| {
        format!(
            "frozen Nix export planning requires existing lockfile {}",
            lock_path.display()
        )
    })?;
    let lock_text = std::str::from_utf8(&lock_bytes)
        .with_context(|| format!("{} is not UTF-8", lock_path.display()))?;
    let lock = Lockfile::parse(lock_text).context("validating frozen Zed lockfile")?;

    let selected = select_package(&project, &manifest, options.target.as_deref())?;
    enforce_v1_package_class(&selected.manifest, &lock)?;
    verify_prebuilt_bins(&selected.source_root, &selected.manifest.bin)?;

    let packed_dir = tempfile::tempdir().context("creating read-only Nix plan packing area")?;
    let packed = pack_selected(
        &project,
        &manifest,
        selected.target.as_deref(),
        packed_dir.path(),
    )?;
    verify_bins_in_artifact(&packed, &selected.manifest.bin)?;

    let mut systems = selected.intent.systems.clone();
    systems.sort();
    let mut outputs = selected.intent.outputs.clone();
    outputs.sort();
    let attribute = selected
        .intent
        .resolved_attribute(&selected.manifest.package.name);

    let package = NixPackageIdentity {
        org: selected.manifest.package.org.clone(),
        name: selected.manifest.package.name.clone(),
        version: selected.manifest.package.version.clone(),
        target: selected.target,
    };
    let package_class = if selected.manifest.bin.is_empty() {
        PlannedPackageClass::Data
    } else {
        PlannedPackageClass::PrebuiltBin
    };
    let file_name = packed
        .path
        .file_name()
        .context("packed artifact has no filename")?
        .to_string_lossy()
        .into_owned();

    let plan = NixExportPlan {
        schema: NIX_EXPORT_PLAN_SCHEMA_V1,
        package,
        package_class,
        intent: ResolvedNixIntent {
            mode: selected.intent.mode,
            attribute,
            systems,
            outputs,
        },
        source: PlannedZedArtifact {
            file_name,
            artifact: NixInteropArtifact {
                format: packed.format,
                sha256: packed.sha256,
                size: packed.size,
            },
            manifest_sha256: sha256_bytes(&manifest_bytes),
            lock_sha256: sha256_bytes(&lock_bytes),
        },
        bins: selected.manifest.bin,
        dependencies: Vec::new(),
        policy: strict_policy(),
    };
    plan.validate()?;
    Ok(plan)
}

fn select_package(
    project: &Path,
    manifest: &Manifest,
    requested_target: Option<&str>,
) -> Result<SelectedPackage> {
    if !manifest.is_polyglot() {
        if let Some(target) = requested_target {
            bail!(
                "single-language package `{}` has no target `{target}`; omit --target",
                manifest.full_name()
            );
        }
        let intent = manifest.publish.nix.clone().with_context(|| {
            format!(
                "package `{}` declares no [publish.nix] export intent",
                manifest.full_name()
            )
        })?;
        return Ok(SelectedPackage {
            target: None,
            manifest: manifest.clone(),
            intent,
            source_root: project.to_path_buf(),
        });
    }

    let requested = requested_target.with_context(|| {
        let routes = manifest
            .nix_export_routes()
            .into_iter()
            .map(|route| route.target)
            .collect::<Vec<_>>();
        if routes.is_empty() {
            format!(
                "polyglot package `{}` declares no [targets.<target>.nix] routes",
                manifest.full_name()
            )
        } else {
            format!(
                "polyglot Nix export planning requires --target; available routes: {}",
                routes.join(", ")
            )
        }
    })?;
    let target = manifest.resolve_target_key(requested).with_context(|| {
        let available = manifest.targets.keys().cloned().collect::<Vec<_>>();
        format!(
            "package `{}` has no target `{requested}`; available targets: {}",
            manifest.full_name(),
            available.join(", ")
        )
    })?;
    let section = manifest
        .targets
        .get(target)
        .context("resolved target disappeared from manifest")?;
    let intent = section.nix.clone().with_context(|| {
        format!("target `{target}` declares no [targets.{target}.nix] export intent")
    })?;
    let derived = manifest
        .manifest_for_target(target)
        .context("resolved target could not be re-rooted")?;
    let source_root = project.join(&section.dir);
    if !source_root.is_dir() {
        bail!(
            "target `{target}` source directory `{}` does not exist",
            section.dir
        );
    }
    Ok(SelectedPackage {
        target: Some(target.to_string()),
        manifest: derived,
        intent,
        source_root,
    })
}

fn enforce_v1_package_class(manifest: &Manifest, lock: &Lockfile) -> Result<()> {
    if manifest.is_workspace_root() {
        bail!("Nix export plan v1 does not export workspace roots");
    }
    if manifest.build.is_some()
        || !manifest.build_dependencies.is_empty()
        || !manifest.overrides.is_empty()
    {
        bail!(
            "Nix export plan v1 does not infer source builds; remove build hooks/build dependencies or export a prebuilt artifact"
        );
    }
    if !manifest.dependencies.is_empty() || !lock.packages.is_empty() {
        bail!(
            "Nix export plan v1 currently accepts dependency-free packages only; the frozen dependency-graph export is a separate reviewed slice"
        );
    }
    Ok(())
}

fn verify_prebuilt_bins(source_root: &Path, bins: &BTreeMap<String, String>) -> Result<()> {
    for (name, relative) in bins {
        let path = source_root.join(relative);
        if !path.is_file() {
            bail!(
                "prebuilt bin `{name}` points to `{relative}`, which is not a file in the selected package source"
            );
        }
    }
    Ok(())
}

fn pack_selected(
    project: &Path,
    manifest: &Manifest,
    target: Option<&str>,
    out_dir: &Path,
) -> Result<PackResult> {
    match target {
        None => pack(project, manifest, Some(out_dir)),
        Some(target) => pack_all(project, manifest, Some(out_dir))?
            .into_iter()
            .find(|package| package.target.as_deref() == Some(target))
            .map(|package| package.packed)
            .with_context(|| format!("packed output for target `{target}` was not produced")),
    }
}

fn verify_bins_in_artifact(packed: &PackResult, bins: &BTreeMap<String, String>) -> Result<()> {
    if bins.is_empty() {
        return Ok(());
    }
    if packed.format != ArtifactFormat::TarGz {
        bail!("Nix export planning currently expects the canonical tar.gz Zed artifact");
    }
    let file = fs::File::open(&packed.path)
        .with_context(|| format!("opening packed artifact {}", packed.path.display()))?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let mut files = BTreeSet::new();
    for entry in archive
        .entries()
        .context("reading packed artifact entries")?
    {
        let entry = entry.context("reading packed artifact entry")?;
        if entry.header().entry_type().is_file() {
            files.insert(entry.path()?.to_string_lossy().replace('\\', "/"));
        }
    }
    for (name, relative) in bins {
        let expected = format!("{ARCHIVE_ROOT}/{}", relative.replace('\\', "/"));
        if !files.contains(&expected) {
            bail!(
                "prebuilt bin `{name}` is absent from the immutable artifact at `{expected}` (check publish excludes)"
            );
        }
    }
    Ok(())
}

fn strict_policy() -> NixPolicyEvidence {
    NixPolicyEvidence {
        profile: NixPolicyProfile::StrictV1,
        pure_evaluation: true,
        import_from_derivation: false,
        sandbox_required: true,
        builder_network: NixBuilderNetwork::Disabled,
        dirty_source: false,
        publishable: true,
    }
}

fn project_root(requested: &Path) -> Option<PathBuf> {
    requested
        .ancestors()
        .find(|candidate| candidate.join(MANIFEST_FILE).is_file())
        .map(Path::to_path_buf)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn print_human_plan(plan: &NixExportPlan) {
    println!(
        "Nix export plan: {}/{}@{}",
        plan.package.org, plan.package.name, plan.package.version
    );
    if let Some(target) = &plan.package.target {
        println!("target: {target}");
    }
    println!("class: {:?}", plan.package_class);
    println!("attribute: {}", plan.intent.attribute);
    println!("systems: {}", plan.intent.systems.join(", "));
    println!("outputs: {}", plan.intent.outputs.join(", "));
    println!("artifact: {}", plan.source.file_name);
    println!("artifact sha256: {}", plan.source.artifact.sha256);
    println!("manifest sha256: {}", plan.source.manifest_sha256);
    println!("lock sha256: {}", plan.source.lock_sha256);
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
        "interop" => Route::Interop,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "interop" => Route::InteropHelp {
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

fn validate_interop_flags(argv: &[String]) -> Result<()> {
    let parser_argv = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect::<Vec<_>>();
    let parsed = parse_embedded(&parser_argv)?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown Nix interop option(s): {}",
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
            "flags2env rejected invalid Nix interop value(s): {}",
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
    let contract_dir = tempfile::tempdir().context("creating Nix interop flags directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, INTEROP_CONTRACT).context("writing Nix interop flag contract")?;
    let contract_path = contract_path
        .to_str()
        .context("Nix interop flag contract path is not UTF-8")?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("Nix interop flags2env audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("Nix interop flags2env parse failed: {error}"))
}

fn normalize_boolean_environment() -> Result<()> {
    for key in [
        "ZED_PKG_FROZEN",
        "ZED_PKG_NIX_PLAN_JSON",
        "ZED_PKG_INTERACTIVE",
    ] {
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
    fn modular_route_and_help_include_interop() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            route(&args(&[
                "zed", "interop", "nix", "plan", "export", "--frozen", "--json",
            ])),
            Route::Interop
        );
        assert!(matches!(
            route(&args(&["zed", "help", "interop"])),
            Route::InteropHelp { .. }
        ));
        assert_eq!(route(&args(&["zed", "install"])), Route::Existing);

        let command = augment_root_command(crate::fetch::augment_root_command(
            crate::dev::augment_root_command(crate::cli::Cli::command()),
        ));
        let interop = command
            .get_subcommands()
            .find(|command| command.get_name() == "interop")
            .expect("interop command must be present");
        assert!(
            interop
                .get_subcommands()
                .any(|command| command.get_name() == "nix")
        );
    }

    #[test]
    fn strict_plan_policy_is_publishable_and_network_disabled() {
        strict_policy().validate().unwrap();
    }
}
