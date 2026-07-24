use clap::Parser;
use zed_cli::cli::{CacheCmd, Cli, Cmd, OrgCmd, StoreCmd, UpdateCmd};
use zed_cli::config::Config;
use zed_cli::ops;
use zed_cli::r2g::{self, R2gOptions};
use zed_cli::store::Store;
use zed_cli::update;

fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = std::env::current_dir()?;
    match cli.cmd {
        Cmd::Init { org, name } => ops::init(&cwd, org, name),
        Cmd::Add { spec } => ops::add(&cwd, &cfg, &spec),
        Cmd::Remove { spec } => ops::remove(&cwd, &cfg, &spec),
        Cmd::Install {
            frozen,
            install_mode,
            adapter,
        } => ops::install(&cwd, &cfg, frozen, install_mode, adapter).map(|_| ()),
        Cmd::Build { target, force } => ops::build(&cwd, &cfg, target, force),
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
        Cmd::Publish {
            dry_run,
            allow_dirty,
            skip_vcs_checks,
        } => ops::publish(&cwd, &cfg, dry_run, allow_dirty, skip_vcs_checks),
        Cmd::R2g {
            docker,
            image,
            runtime,
            root,
            clean,
        } => r2g::run(
            &cwd,
            &cfg,
            &R2gOptions {
                docker,
                image,
                runtime,
                root,
                clean,
            },
        ),
        Cmd::Update { cmd } => match cmd {
            UpdateCmd::SelfUpdate { check, force } => {
                update::self_update(env!("CARGO_PKG_VERSION"), check, force)
            }
        },
        Cmd::Login => ops::login(&cfg),
        Cmd::Org { cmd } => match cmd {
            OrgCmd::Claim { slug } => ops::org_claim(&cfg, &slug),
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
