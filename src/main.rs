use std::ffi::{OsStr, OsString};

use zed_cli::asdf_environment;
use zed_cli::auth;
use zed_cli::cli::EnvCmd;
use zed_cli::cli::{
    AuthCmd, CacheCmd, Cli, Cmd, EnvironmentManagerArg, OrgCmd, ReleaseCmd, StoreCmd,
};
use zed_cli::completion;
use zed_cli::config::Config;
use zed_cli::dev;
use zed_cli::environment;
use zed_cli::fetch;
use zed_cli::git_submodules as submodules;
use zed_cli::global;
use zed_cli::managed_install;
use zed_cli::mise_export::{self, MiseExportMode};
use zed_cli::nix_bundle_write;
use zed_cli::nix_export_plan;
use zed_cli::ops;
use zed_cli::preflight;
use zed_cli::r2g::{self, R2gOptions};
use zed_cli::release;
use zed_cli::store::Store;
use zed_cli::update;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    zed_cli::cli_model::prepare_environment(&args);
    if let Err(error) = zed_cli::flags::normalize_global_boolean_environment(&args) {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    if root_help_requested(&args) {
        if let Err(error) = completion::print_root_help() {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    let global_requested = args.iter().skip(1).any(|argument| {
        let argument = argument.as_os_str();
        argument == OsStr::new("global") || argument == OsStr::new("--global")
    });
    if global_requested && let Some(result) = global::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = submodules::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = nix_bundle_write::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = nix_export_plan::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = fetch::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = dev::dispatch(args) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }

    if let Err(error) = zed_cli::flags::apply_cli_flags() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    let cli = zed_cli::cli_model::parse();
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn root_help_requested(args: &[OsString]) -> bool {
    let mut index = 1;
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if token == "--help" || token == "-h" {
            return true;
        }
        if token == "help" {
            return args
                .iter()
                .skip(index + 1)
                .all(|argument| argument.to_string_lossy().starts_with('-'));
        }
        if root_global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return false;
    }
    false
}

fn root_global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--token",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
        "--global-bin-dir",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::from_globals(&cli.globals)?;
    let git_submodules = cli.globals.git_submodules;
    let cwd = std::env::current_dir()?;
    if cwd.join(zed_cli::transaction::STAGING_DIR).is_dir() {
        // Every live project transaction already owns this kernel-backed
        // install lock. Recover under the same lock so a concurrent process
        // cannot mistake an in-flight rollback journal for an abandoned one.
        let store = Store::new(&cfg.home);
        let _recovery_lock = store.install_lock()?;
        zed_cli::transaction::recover_pending(&cwd)?;
    }
    match cli.cmd {
        Cmd::Init { org, name } => ops::init(&cwd, org, name, cfg.interactive),
        Cmd::Add { spec } => ops::add(&cwd, &cfg, &spec),
        Cmd::Remove { spec } => ops::remove(&cwd, &cfg, &spec),
        Cmd::Install {
            specs,
            frozen,
            install_mode,
            adapter,
            allow_build,
            target,
            allow_no_manifest,
            allow_ecosystem_mismatch,
        } => {
            if git_submodules {
                submodules::sync(&cwd)?;
            }
            managed_install::install(
                &cwd,
                &cfg,
                &specs,
                frozen,
                install_mode,
                adapter,
                allow_build,
                target.as_deref(),
                allow_no_manifest,
                allow_ecosystem_mismatch,
            )
            .map(|_| ())
        }
        Cmd::Uninstall { specs } => ops::uninstall(&cwd, &cfg, &specs),
        Cmd::Env { cmd } => match cmd {
            EnvCmd::Import {
                manager,
                config,
                lock,
                frozen,
                json,
            } => match manager {
                EnvironmentManagerArg::Mise => {
                    let imported =
                        environment::import_mise(&cwd, config.as_deref(), lock.as_deref(), frozen)?;
                    environment::print_import(&imported, json)
                }
                EnvironmentManagerArg::Asdf => {
                    let imported = asdf_environment::import_asdf(
                        &cwd,
                        config.as_deref(),
                        lock.as_deref(),
                        frozen,
                    )?;
                    asdf_environment::print_import(&imported, json)
                }
            },
            EnvCmd::Export {
                manager: EnvironmentManagerArg::Mise,
                plan,
                output,
                check,
                write,
                json,
            } => {
                if check && write {
                    anyhow::bail!("the arguments '--check' and '--write' cannot be used together");
                }
                let mode = if check {
                    MiseExportMode::Check
                } else if write {
                    MiseExportMode::Write
                } else {
                    MiseExportMode::Print
                };
                let exported = mise_export::export_mise(&cwd, &plan, &output, mode)?;
                mise_export::print_export(&exported, json)
            }
            EnvCmd::Export {
                manager: EnvironmentManagerArg::Asdf,
                ..
            } => anyhow::bail!("asdf export is not implemented; use `zed env export mise`"),
            EnvCmd::Verify {
                manager,
                config,
                lock,
                frozen,
                json,
            } => match manager {
                EnvironmentManagerArg::Mise => {
                    let imported =
                        environment::import_mise(&cwd, config.as_deref(), lock.as_deref(), frozen)?;
                    environment::print_verification(&imported, json)
                }
                EnvironmentManagerArg::Asdf => {
                    let imported = asdf_environment::import_asdf(
                        &cwd,
                        config.as_deref(),
                        lock.as_deref(),
                        frozen,
                    )?;
                    asdf_environment::print_verification(&imported, json)
                }
            },
        },
        Cmd::Completions { shell } => {
            completion::print(shell.into());
            Ok(())
        }
        Cmd::Build { force } => ops::build_cmd(&cwd, &cfg, force),
        Cmd::Run { command, args } => match ops::run(&cwd, &command, &args) {
            Ok(code) => std::process::exit(code),
            Err(error) => Err(error),
        },
        Cmd::Gc {
            older_than,
            dry_run,
        } => ops::gc(&cfg, &older_than, dry_run),
        Cmd::Find { query } => ops::find(&cfg, &query),
        Cmd::Pack { out } => ops::pack_cmd(&cwd, out.as_deref()).map(|_| ()),
        Cmd::Env { cmd } => match cmd {
            EnvCmd::Import {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
                let imported =
                    environment::import_mise(&cwd, config.as_deref(), lock.as_deref(), frozen)?;
                environment::print_import(&imported, json)
            }
            EnvCmd::Verify {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
                let imported =
                    environment::import_mise(&cwd, config.as_deref(), lock.as_deref(), frozen)?;
                environment::print_verification(&imported, json)
            }
        },
        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
            ReleaseCmd::Preflight => preflight::preflight(&cwd),
        },
        Cmd::Publish {
            dry_run,
            allow_dirty,
            skip_vcs_checks,
        } => {
            managed_install::ensure_publishable(&cwd)?;
            ops::publish(&cwd, &cfg, dry_run, allow_dirty, skip_vcs_checks)
        }
        Cmd::Yank { spec, undo } => ops::yank(&cfg, &spec, undo),
        Cmd::R2g {
            registry_mode,
            docker,
            image,
            runtime,
            root,
            clean,
        } => r2g::run(
            &cwd,
            &cfg,
            &R2gOptions {
                registry_mode,
                docker,
                image,
                runtime,
                root,
                clean,
            },
        ),
        Cmd::SelfUpdate {
            check,
            force,
            skip_checksum,
        } => update::self_update(env!("CARGO_PKG_VERSION"), check, force, skip_checksum),
        Cmd::Login {
            email,
            provider,
            password_stdin,
        } => auth::login(&cfg, email.as_deref(), provider, password_stdin),
        Cmd::Signup {
            email,
            provider,
            display_name,
            password_stdin,
        } => auth::signup(
            &cfg,
            email.as_deref(),
            provider,
            display_name.as_deref(),
            password_stdin,
        ),
        Cmd::Logout => auth::signout(&cfg),
        Cmd::Auth { cmd } => match cmd {
            AuthCmd::Login {
                email,
                provider,
                password_stdin,
            } => auth::login(&cfg, email.as_deref(), provider, password_stdin),
            AuthCmd::Signup {
                email,
                provider,
                display_name,
                password_stdin,
            } => auth::signup(
                &cfg,
                email.as_deref(),
                provider,
                display_name.as_deref(),
                password_stdin,
            ),
            AuthCmd::Signout => auth::signout(&cfg),
            AuthCmd::ImportToken => ops::login(&cfg),
            AuthCmd::Status => auth::status(&cfg),
            AuthCmd::Refresh => auth::refresh(&cfg),
            AuthCmd::Token => auth::print_token(&cfg),
        },
        Cmd::Org { cmd } => match cmd {
            OrgCmd::Claim { slug } => ops::org_claim(&cfg, &slug),
            OrgCmd::Audit { slug, limit } => ops::org_audit(&cfg, &slug, limit),
        },
        Cmd::Store { cmd } => match cmd {
            StoreCmd::Status => ops::store_status(&cfg),
            StoreCmd::Path => {
                println!("{}", Store::new(&cfg.home).root().display());
                Ok(())
            }
            StoreCmd::Prune => ops::store_prune(&cfg),
        },
        Cmd::Cache { cmd } => match cmd {
            CacheCmd::Clean => ops::cache_clean(&cfg),
        },
    }
}
