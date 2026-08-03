use clap::Parser;
use zed_cli::auth;
use zed_cli::cli::{AuthCmd, CacheCmd, Cli, Cmd, EnvCmd, OrgCmd, ReleaseCmd, StoreCmd};
use zed_cli::completion;
use zed_cli::config::Config;
use zed_cli::dev;
use zed_cli::environment;
use zed_cli::manifestless;
use zed_cli::ops;
use zed_cli::preflight;
use zed_cli::r2g::{self, R2gOptions};
use zed_cli::release;
use zed_cli::store::Store;
use zed_cli::update;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    if let Err(error) = zed_cli::flags::normalize_global_boolean_environment(&args) {
        eprintln!("error: {error:#}");
        std::process::exit(2);
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
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = std::env::current_dir()?;
    zed_cli::transaction::recover_pending(&cwd)?;
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
        } => manifestless::install(
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
        .map(|_| ()),
        Cmd::Uninstall { specs } => ops::uninstall(&cwd, &cfg, &specs),
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
        } => ops::publish(&cwd, &cfg, dry_run, allow_dirty, skip_vcs_checks),
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
