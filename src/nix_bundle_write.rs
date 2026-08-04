//! Command routing for deterministic standalone Zed → Nix bundle publication.
//!
//! `zed interop nix bundle write` composes three existing trust boundaries:
//! the frozen export planner, the pure flake-bundle renderer, and the atomic
//! no-clobber persistence API. It performs no registry resolution, credential
//! lookup, signing, cache upload, Nix execution, or overwrite.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use flags2env::BundledFlags2Env;
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::paths::MANIFEST_FILE;

use crate::cli::Globals;
use crate::config::read_manifest;
use crate::nix_export_bundle::{
    PersistNixExportBundleOutcome, RenderedNixExportBundle, persist_nix_export_bundle,
    render_nix_export_bundle,
};
use crate::nix_export_plan::{NixExportPlan, NixExportPlanArgs, build_plan};
use crate::pack::{PackResult, pack, pack_all};

const INTEROP_CONTRACT: &str = include_str!("../.nix-interop-cli-flags.toml");
const WRITE_RECEIPT_SCHEMA_V1: &str = "zed.nix-flake-bundle-write/v1";

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
    /// Nix planning, bundle creation, import, and verification.
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
    /// Render and persist deterministic standalone flake bundles.
    Bundle(NixBundleArgs),
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

#[derive(Debug, Args)]
struct NixBundleArgs {
    #[command(subcommand)]
    command: NixBundleCommand,
}

#[derive(Debug, Subcommand)]
enum NixBundleCommand {
    /// Render and atomically create one standalone flake directory.
    Write(NixBundleWriteArgs),
}

#[derive(Debug, Clone, Args)]
struct NixBundleWriteArgs {
    /// Require the existing `.zpkg.lock` as the only dependency authority.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    frozen: bool,

    /// Explicit polyglot target. Single-language packages reject this flag.
    #[arg(long, env = "ZED_PKG_NIX_TARGET", value_name = "TARGET")]
    target: Option<String>,

    /// Approved immutable `flake.lock` containing the pinned Nixpkgs input.
    #[arg(long, env = "ZED_PKG_NIX_FLAKE_LOCK", value_name = "PATH")]
    flake_lock: PathBuf,

    /// New standalone flake directory. Existing non-identical state is never overwritten.
    #[arg(
        long,
        visible_alias = "output",
        env = "ZED_PKG_NIX_BUNDLE_OUT",
        value_name = "DIR"
    )]
    out: PathBuf,

    /// Print canonical compact JSON instead of the human receipt.
    #[arg(long, env = "ZED_PKG_NIX_PLAN_JSON")]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Interop,
    InteropHelp {
        help_index: usize,
        target_index: usize,
    },
    Existing,
}

#[derive(Debug, Serialize)]
struct BundleWriteReceipt<'a> {
    schema: &'static str,
    outcome: &'static str,
    destination: String,
    package: &'a zed_interfaces::NixPackageIdentity,
    plan_sha256: &'a str,
    flake_lock_sha256: &'a str,
    bundle_sha256: &'a str,
    nixpkgs_reference: &'a str,
    nixpkgs_rev: &'a str,
    nixpkgs_nar_hash: &'a str,
}

/// Route the complete modular `interop` family here. Planning delegates to the
/// established planner; bundle writing adds composition only.
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
        Route::Existing => None,
    }
}

/// Replace or add the nested `interop` tree for root help and completions.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    let interop = InteropCli::command()
        .find_subcommand("interop")
        .expect("interop command is declared")
        .clone();
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "interop")
    {
        command.mut_subcommand("interop", |_| interop)
    } else {
        command.subcommand(interop)
    }
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

    // These commands are local, frozen, and credential-independent. Global
    // settings remain accepted for CLI consistency but are never converted to
    // Config, accessed, serialized, or used for mutable resolution.
    let InteropCli {
        globals: _,
        command,
    } = cli;
    let InteropRoot::Interop(interop) = command;
    let InteropEcosystem::Nix(nix) = interop.ecosystem;

    let cwd = env::current_dir().context("reading current directory")?;
    match nix.command {
        NixCommand::Plan(plan) => {
            let NixPlanCommand::Export(options) = plan.command;
            let planned = build_plan(&cwd, &options)?;
            if options.json {
                println!("{}", planned.canonical_json_string()?);
            } else {
                print_human_plan(&planned);
            }
        }
        NixCommand::Bundle(bundle) => {
            let NixBundleCommand::Write(options) = bundle.command;
            write_bundle(&cwd, &options)?;
        }
    }
    Ok(0)
}

fn write_bundle(requested_root: &Path, options: &NixBundleWriteArgs) -> Result<()> {
    if !options.frozen {
        bail!(
            "`zed interop nix bundle write` version 1 is frozen-only; pass --frozen or set ZED_PKG_FROZEN=1"
        );
    }

    let plan_options = NixExportPlanArgs {
        frozen: true,
        json: false,
        target: options.target.clone(),
    };
    let plan = build_plan(requested_root, &plan_options)?;
    let artifact_bytes = repack_planned_artifact(requested_root, &plan)?;
    let flake_lock_bytes = fs::read(&options.flake_lock).with_context(|| {
        format!(
            "reading approved immutable flake lock {}",
            options.flake_lock.display()
        )
    })?;
    let rendered = render_nix_export_bundle(&plan, &artifact_bytes, &flake_lock_bytes)?;
    let destination = resolve_destination(&options.out)?;
    let outcome = persist_nix_export_bundle(&rendered, &destination)?;
    let canonical_destination = fs::canonicalize(&destination).with_context(|| {
        format!(
            "canonicalizing successfully persisted Nix bundle {}",
            destination.display()
        )
    })?;
    print_receipt(
        &rendered,
        &plan,
        outcome,
        &canonical_destination,
        options.json,
    )
}

fn repack_planned_artifact(requested_root: &Path, plan: &NixExportPlan) -> Result<Vec<u8>> {
    let requested_root = fs::canonicalize(requested_root)
        .with_context(|| format!("reading invocation directory {}", requested_root.display()))?;
    let project = requested_root
        .ancestors()
        .find(|candidate| candidate.join(MANIFEST_FILE).is_file())
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "Nix bundle writing requires {MANIFEST_FILE} at or above {}",
                requested_root.display()
            )
        })?;
    let manifest = read_manifest(&project).context("reading Nix bundle source manifest")?;
    let packed_dir = tempfile::tempdir().context("creating Nix bundle packing area")?;
    let packed = match plan.package.target.as_deref() {
        None => pack(&project, &manifest, Some(packed_dir.path()))?,
        Some(target) => pack_all(&project, &manifest, Some(packed_dir.path()))?
            .into_iter()
            .find(|package| package.target.as_deref() == Some(target))
            .map(|package| package.packed)
            .with_context(|| format!("packed output for target `{target}` was not produced"))?,
    };
    verify_packed_identity(&packed, plan)?;
    let bytes = fs::read(&packed.path)
        .with_context(|| format!("reading repacked artifact {}", packed.path.display()))?;
    let actual_sha256 = hex::encode(Sha256::digest(&bytes));
    if actual_sha256 != plan.source.artifact.sha256 || bytes.len() as u64 != plan.source.artifact.size
    {
        bail!(
            "deterministic repack does not match frozen Nix export plan (expected {} bytes at {}, got {} bytes at {})",
            plan.source.artifact.size,
            plan.source.artifact.sha256,
            bytes.len(),
            actual_sha256
        );
    }
    Ok(bytes)
}

fn verify_packed_identity(packed: &PackResult, plan: &NixExportPlan) -> Result<()> {
    let file_name = packed
        .path
        .file_name()
        .context("repacked artifact has no filename")?
        .to_string_lossy();
    if file_name != plan.source.file_name
        || packed.format != plan.source.artifact.format
        || packed.sha256 != plan.source.artifact.sha256
        || packed.size != plan.source.artifact.size
    {
        bail!("deterministic repack metadata does not match the frozen Nix export plan");
    }
    Ok(())
}

fn resolve_destination(destination: &Path) -> Result<PathBuf> {
    destination
        .file_name()
        .filter(|name| !name.is_empty())
        .context("Nix bundle output must name a directory")?;
    if destination.is_absolute() {
        return Ok(destination.to_path_buf());
    }
    Ok(env::current_dir()
        .context("reading current directory for relative Nix bundle output")?
        .join(destination))
}

fn print_receipt(
    rendered: &RenderedNixExportBundle,
    plan: &NixExportPlan,
    outcome: PersistNixExportBundleOutcome,
    destination: &Path,
    json: bool,
) -> Result<()> {
    let outcome_text = match outcome {
        PersistNixExportBundleOutcome::Created => "created",
        PersistNixExportBundleOutcome::AlreadyCurrent => "already-current",
    };
    let receipt = BundleWriteReceipt {
        schema: WRITE_RECEIPT_SCHEMA_V1,
        outcome: outcome_text,
        destination: destination.to_string_lossy().into_owned(),
        package: &plan.package,
        plan_sha256: &rendered.inventory.plan_sha256,
        flake_lock_sha256: &rendered.inventory.flake_lock_sha256,
        bundle_sha256: &rendered.inventory.bundle_sha256,
        nixpkgs_reference: &rendered.inventory.nixpkgs.reference,
        nixpkgs_rev: &rendered.inventory.nixpkgs.rev,
        nixpkgs_nar_hash: &rendered.inventory.nixpkgs.nar_hash,
    };
    if json {
        println!("{}", serde_json::to_string(&receipt)?);
    } else {
        println!("Nix flake bundle: {outcome_text}");
        println!("destination: {}", destination.display());
        println!(
            "package: {}/{}@{}",
            plan.package.org, plan.package.name, plan.package.version
        );
        if let Some(target) = &plan.package.target {
            println!("target: {target}");
        }
        println!("bundle sha256: {}", rendered.inventory.bundle_sha256);
        println!("plan sha256: {}", rendered.inventory.plan_sha256);
        println!("nixpkgs rev: {}", rendered.inventory.nixpkgs.rev);
    }
    Ok(())
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
    let Some((command_index, command)) = first_command(args) else {
        return Route::Existing;
    };

    match command.as_str() {
        "interop" => Route::Interop,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "interop" => Route::InteropHelp {
                help_index: command_index,
                target_index,
            },
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

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn route_owns_interop_but_not_existing_commands() {
        assert_eq!(
            route(&args(&[
                "zed", "interop", "nix", "bundle", "write", "--frozen",
            ])),
            Route::Interop
        );
        assert_eq!(
            route(&args(&[
                "zed", "interop", "nix", "plan", "export", "--frozen",
            ])),
            Route::Interop
        );
        assert!(matches!(
            route(&args(&["zed", "help", "interop"])),
            Route::InteropHelp { .. }
        ));
        assert_eq!(route(&args(&["zed", "install"])), Route::Existing);
    }

    #[test]
    fn augmented_command_contains_plan_and_bundle_write() {
        let command = augment_root_command(crate::nix_export_plan::augment_root_command(
            crate::fetch::augment_root_command(
                crate::dev::augment_root_command(crate::cli::Cli::command()),
            ),
        ));
        let interop = command.find_subcommand("interop").unwrap();
        let nix = interop.find_subcommand("nix").unwrap();
        assert!(nix.find_subcommand("plan").is_some());
        let bundle = nix.find_subcommand("bundle").unwrap();
        let write = bundle.find_subcommand("write").unwrap();
        for option in ["frozen", "target", "flake_lock", "out", "json"] {
            assert!(write.get_arguments().any(|argument| argument.get_id() == option));
        }
    }

    #[test]
    fn relative_output_preserves_lexical_parent_until_persistence() {
        assert_eq!(
            resolve_destination(Path::new("bundle")).unwrap(),
            env::current_dir().unwrap().join("bundle")
        );
    }

    #[test]
    fn absolute_output_is_not_canonicalized_before_persistence() {
        let absolute = env::current_dir().unwrap().join("parent-link/bundle");
        assert_eq!(resolve_destination(&absolute).unwrap(), absolute);
    }
}
