use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use zed_cli::release_intent::ReleaseIntentKind;

#[derive(Debug, Parser)]
#[command(name = "zed-release-intent", about = "Record and validate commit-only vs package release intent")]
struct Args {
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Record a local release intent. `none` means an ordinary Git commit.
    Set {
        #[arg(value_enum)]
        kind: IntentArg,
        /// Explicit target for calendar versions. For semver it must agree with the computed bump.
        #[arg(long)]
        target_version: Option<String>,
        /// Allow a patch release to notify dependents despite patch suppression policy.
        #[arg(long)]
        security_critical: bool,
        #[arg(long)]
        rationale: Option<String>,
        /// Update [package].version in .zpkg.toml while preserving surrounding formatting.
        #[arg(long)]
        apply_manifest: bool,
        #[arg(long)]
        json: bool,
    },
    /// Validate the local intent against the current manifest.
    Check {
        #[arg(long)]
        allow_missing: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pre-commit guard: require an intent only when staged package.version changes.
    Guard,
    /// Pre-push guard over the exact remote and local revisions supplied by Git.
    GuardRange {
        base: String,
        head: String,
    },
    /// Print the current validated intent.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Remove the consumed local intent after a successful publish.
    Clear,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum IntentArg {
    None,
    Patch,
    Minor,
    Major,
    Calendar,
}

impl From<IntentArg> for ReleaseIntentKind {
    fn from(value: IntentArg) -> Self {
        match value {
            IntentArg::None => Self::None,
            IntentArg::Patch => Self::Patch,
            IntentArg::Minor => Self::Minor,
            IntentArg::Major => Self::Major,
            IntentArg::Calendar => Self::Calendar,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let project = if args.project.is_absolute() {
        args.project
    } else {
        std::env::current_dir()?.join(args.project)
    };
    match args.command {
        Command::Set {
            kind,
            target_version,
            security_critical,
            rationale,
            apply_manifest,
            json,
        } => {
            let intent = zed_cli::release_intent::set(
                &project,
                kind.into(),
                target_version.as_deref(),
                security_critical,
                rationale,
                apply_manifest,
            )?;
            print_intent(&intent, json)?;
        }
        Command::Check { allow_missing, json } => {
            match zed_cli::release_intent::check(&project, allow_missing)? {
                Some(intent) => print_intent(&intent, json)?,
                None if !json => println!("no release intent; ordinary commit/publish policy applies"),
                None => println!("null"),
            }
        }
        Command::Guard => {
            zed_cli::release_intent::guard_staged(&project)?;
            println!("release intent guard passed");
        }
        Command::GuardRange { base, head } => {
            zed_cli::release_intent::guard_range(&project, &base, &head)?;
            println!("release intent range guard passed");
        }
        Command::Show { json } => {
            let intent = zed_cli::release_intent::load(&project)?;
            let Some(validated) = zed_cli::release_intent::check(&project, false)? else {
                bail!("release intent disappeared while validating it");
            };
            debug_assert_eq!(intent, validated);
            print_intent(&intent, json)?;
        }
        Command::Clear => {
            if zed_cli::release_intent::clear(&project)? {
                println!("cleared {}", zed_cli::release_intent::intent_path(&project).display());
            } else {
                println!("no release intent to clear");
            }
        }
    }
    Ok(())
}

fn print_intent(intent: &zed_cli::release_intent::ReleaseIntentV1, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(intent)?);
    } else {
        println!("package: {}", intent.package);
        println!("intent: {}", intent.requested.as_str());
        println!("current: {}", intent.current_version);
        println!("target: {}", intent.target_version);
        println!("security-critical: {}", intent.security_critical);
    }
    Ok(())
}