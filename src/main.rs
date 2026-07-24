use clap::Parser;
use zed_cli::cli::{CacheCmd, Cli, Cmd, OrgCmd, StoreCmd};
use zed_cli::config::Config;
use zed_cli::ops;
use zed_cli::store::Store;

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
            allow_build,
        } => ops::install(&cwd, &cfg, frozen, install_mode, adapter, allow_build).map(|_| ()),
        Cmd::Find { query } => ops::find(&cfg, &query),
        Cmd::Pack { out } => ops::pack_cmd(&cwd, out.as_deref()).map(|_| ()),
        Cmd::Publish {
            dry_run,
            allow_dirty,
            skip_vcs_checks,
        } => ops::publish(&cwd, &cfg, dry_run, allow_dirty, skip_vcs_checks),
        Cmd::TestLocal => ops::test_local(&cwd, &cfg),
        Cmd::Run { bin, args } => {
            let code = ops::run_bin(&cwd, &bin, &args)?;
            std::process::exit(code);
        }
        Cmd::Yank { spec, undo } => ops::yank(&cfg, &spec, undo),
        Cmd::Gc { max_age_days } => ops::gc(&cfg, max_age_days),
        Cmd::SelfUpdate { check } => ops::self_update(check),
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
