#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if new in text:
        return
    if old not in text:
        raise SystemExit(f"expected snippet not found in {path}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "src/cli.rs",
    '''    /// Resolve and install dependencies into zed_modules/
    #[command(alias = "i")]
    Install {
        /// Install exactly what .zpkg.lock pins; fail on any drift
        #[arg(long, env = "ZED_PKG_FROZEN")]
        frozen: bool,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_INSTALL_MODE",
            default_value = "symlink"
        )]
        install_mode: InstallMode,
        /// Also link packages where the language ecosystem expects them,
        /// inferred from the project by default (experimental; python
        /// site-packages and deeper maven integration are planned)
        #[arg(long, value_enum, env = "ZED_PKG_ADAPTER", default_value = "auto")]
        adapter: Adapter,
        /// Run dependencies' [build] commands (arbitrary code from the
        /// package author — off by default; builds are cached per
        /// (artifact, platform, command) under ~/.zed-pkg/builds)
        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
        allow_build: bool,
        /// Which language subtree to take from polyglot dependencies (a repo
        /// shipping e.g. node/, python/, go/). Overrides [install].target;
        /// omitted = infer from the project
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
    },
''',
    '''    /// Resolve and install dependencies into zed_modules/
    #[command(alias = "i")]
    Install {
        /// Install exactly what .zpkg.lock pins; fail on any drift
        #[arg(long, env = "ZED_PKG_FROZEN")]
        frozen: bool,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_INSTALL_MODE",
            default_value = "symlink"
        )]
        install_mode: InstallMode,
        /// Also link packages where the language ecosystem expects them,
        /// inferred from the project by default (experimental; python
        /// site-packages and deeper maven integration are planned)
        #[arg(long, value_enum, env = "ZED_PKG_ADAPTER", default_value = "auto")]
        adapter: Adapter,
        /// Run dependencies' [build] commands (arbitrary code from the
        /// package author — off by default; builds are cached per
        /// (artifact, platform, command) under ~/.zed-pkg/builds)
        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
        allow_build: bool,
        /// Which language subtree to take from polyglot dependencies (a repo
        /// shipping e.g. node/, python/, go/). Overrides [install].target;
        /// omitted = infer from the project
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
        /// Proceed without .zpkg.toml. Without this flag, a terminal prompt is
        /// required; non-interactive invocations fail closed.
        #[arg(
            long,
            visible_alias = "skip-manifest",
            env = "ZED_PKG_ALLOW_NO_MANIFEST"
        )]
        allow_no_manifest: bool,
        /// Packages to install when the current folder has no .zpkg.toml.
        /// Each value is org/name[@semver-req].
        specs: Vec<String>,
    },
    /// Generate a static shell-completion script from the clap command model
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
''',
)

replace_once(
    "src/cli.rs",
    '''    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
''',
    '''    #[test]
    fn manifestless_install_flags_and_completion_command_parse() {
        let cli = Cli::try_parse_from([
            "zed",
            "install",
            "--skip-manifest",
            "zed-pkg-test/portable-greeter@^1",
            "zed-pkg-test/portable-slugify",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Install {
                allow_no_manifest,
                specs,
                ..
            } => {
                assert!(allow_no_manifest);
                assert_eq!(specs.len(), 2);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["zed", "completions", "bash"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Completions { .. }));
    }

    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
''',
)

replace_once(
    "src/main.rs",
    '''use zed_cli::config::Config;
''',
    '''use zed_cli::completion;
use zed_cli::config::Config;
''',
)
replace_once(
    "src/main.rs",
    '''fn main() {
    zed_cli::flags::apply_cli_flags();
    let cli = Cli::parse();
''',
    '''fn main() {
    if let Err(error) = zed_cli::flags::apply_cli_flags() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    let cli = Cli::parse();
''',
)
replace_once(
    "src/main.rs",
    '''fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = std::env::current_dir()?;
    match cli.cmd {
''',
    '''fn run(cli: Cli) -> anyhow::Result<()> {
    if let Cmd::Completions { shell } = &cli.cmd {
        completion::print(*shell);
        return Ok(());
    }
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = std::env::current_dir()?;
    match cli.cmd {
''',
)
replace_once(
    "src/main.rs",
    '''        Cmd::Install {
            frozen,
            install_mode,
            adapter,
            allow_build,
            target,
        } => ops::install(
            &cwd,
            &cfg,
            frozen,
            install_mode,
            adapter,
            allow_build,
            target.as_deref(),
        )
        .map(|_| ()),
''',
    '''        Cmd::Install {
            frozen,
            install_mode,
            adapter,
            allow_build,
            target,
            allow_no_manifest,
            specs,
        } => ops::install(
            &cwd,
            &cfg,
            frozen,
            install_mode,
            adapter,
            allow_build,
            target.as_deref(),
            allow_no_manifest,
            &specs,
        )
        .map(|_| ()),
        Cmd::Completions { .. } => unreachable!("handled before config initialization"),
''',
)

replace_once(
    "src/ops.rs",
    '''use std::io::BufRead;
''',
    '''use std::io::{self, BufRead, IsTerminal, Write};
''',
)

install_old = '''#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    install_locked(
        project,
        cfg,
        &store,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
    )
}

/// Install body, called with the store lock already held. Split out so the
/// build-hook path can install `[build-dependencies]` into a staging dir
/// under the same lock without deadlocking on a re-acquire.
#[allow(clippy::too_many_arguments)]
fn install_locked(
    project: &Path,
    cfg: &Config,
    store: &Store,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    let manifest = read_manifest(project)?;
'''

install_new = '''fn resolve_dependency_spec(cfg: &Config, spec: &str) -> Result<(String, String)> {
    let (rest, req) = match spec.split_once('@') {
        Some((rest, req)) => (rest, Some(req)),
        None => (spec, None),
    };
    let (org, name) = split_key(rest)?;
    let req = match req {
        Some(req) if req.trim().is_empty() => bail!("empty requirement for {org}/{name}"),
        Some(req) => req.to_string(),
        None => {
            let reg = registry_for(&cfg.registry)?;
            let pkg = reg.get_package(&org, &name)?;
            let latest = pkg
                .latest
                .with_context(|| format!("{org}/{name} has no published versions"))?;
            match version::parse_version(&latest) {
                Some(_) => format!("^{latest}"),
                None => latest,
            }
        }
    };
    Ok((format!("{org}/{name}"), req))
}

fn dependencies_from_lock(project: &Path) -> Result<BTreeMap<String, String>> {
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path).with_context(|| {
        format!(
            "manifestless --frozen install needs package operands or {}",
            lock_path.display()
        )
    })?;
    let lock = Lockfile::parse(&text)?;
    if lock.packages.is_empty() {
        bail!("{} contains no packages", lock_path.display());
    }
    Ok(lock
        .packages
        .into_iter()
        .map(|package| (package.full_name(), package.version))
        .collect())
}

fn manifestless_manifest(dependencies: BTreeMap<String, String>) -> Result<Manifest> {
    let manifest = Manifest {
        package: PackageSection {
            org: "zed-local".to_string(),
            name: "manifestless-consumer".to_string(),
            version: "0.0.0".to_string(),
            version_scheme: version::VersionScheme::Semver,
            description: Some("in-memory manifest for a local consumer install".to_string()),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: "https://localhost/zed-local/manifestless-consumer".to_string(),
            },
            keywords: Vec::new(),
        },
        workspace: None,
        dependencies,
        build_dependencies: BTreeMap::new(),
        build: None,
        overrides: Default::default(),
        bin: BTreeMap::new(),
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
        install: Default::default(),
        targets: Default::default(),
    };
    manifest.validate()?;
    Ok(manifest)
}

fn adapter_label(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Auto => "auto",
        Adapter::None => "none",
        Adapter::Node => "node",
        Adapter::Java => "java",
    }
}

fn confirm_manifestless_install(
    project: &Path,
    dependencies: &BTreeMap<String, String>,
    adapter: Adapter,
    target: Option<&str>,
    allow_no_manifest: bool,
) -> Result<()> {
    if allow_no_manifest {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!(
            "no {MANIFEST_FILE} found in {}; non-interactive manifestless installs require \
             --allow-no-manifest or --skip-manifest",
            project.display()
        );
    }

    let inferred_target = target
        .map(str::to_string)
        .or_else(|| detect_target(project))
        .unwrap_or_else(|| "whole-package".to_string());
    let inferred_adapter = match adapter {
        Adapter::Auto => detect_adapter(project),
        other => other,
    };
    let requested = dependencies
        .iter()
        .map(|(name, req)| format!("{name}@{req}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut stderr = io::stderr().lock();
    writeln!(stderr, "no {MANIFEST_FILE} found in {}", project.display())?;
    writeln!(stderr, "packages: {requested}")?;
    writeln!(
        stderr,
        "inferred target: {inferred_target}; adapter: {}; install dir: {MODULES_DIR}",
        adapter_label(inferred_adapter)
    )?;
    write!(stderr, "install without creating a manifest? [y/N] ")?;
    stderr.flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("manifestless install cancelled");
    }
    Ok(())
}

fn install_manifest(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    adapter: Adapter,
    target: Option<&str>,
    allow_no_manifest: bool,
    specs: &[String],
) -> Result<Manifest> {
    let manifest_path = project.join(MANIFEST_FILE);
    if manifest_path.exists() {
        if !specs.is_empty() {
            bail!(
                "package operands with an existing {MANIFEST_FILE} are ambiguous; use `zed add \
                 <org/name[@req]>` to persist a dependency or run bare `zed install`"
            );
        }
        return read_manifest(project);
    }

    let dependencies = if specs.is_empty() {
        if frozen {
            dependencies_from_lock(project)?
        } else {
            bail!(
                "no {MANIFEST_FILE} found in {}; supply at least one package operand, e.g. \
                 `zed install --allow-no-manifest acme/http-kit@^1`",
                project.display()
            );
        }
    } else {
        let mut dependencies = BTreeMap::new();
        for spec in specs {
            let (key, req) = resolve_dependency_spec(cfg, spec)?;
            if let Some(previous) = dependencies.insert(key.clone(), req.clone())
                && previous != req
            {
                bail!("conflicting requirements for {key}: `{previous}` and `{req}`");
            }
        }
        dependencies
    };

    confirm_manifestless_install(
        project,
        &dependencies,
        adapter,
        target,
        allow_no_manifest,
    )?;
    manifestless_manifest(dependencies)
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_no_manifest: bool,
    specs: &[String],
) -> Result<InstallOutcome> {
    let manifest = install_manifest(
        project,
        cfg,
        frozen,
        adapter,
        target,
        allow_no_manifest,
        specs,
    )?;
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    install_locked(
        project,
        cfg,
        &store,
        &manifest,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
    )
}

/// Install body, called with the store lock already held. Split out so the
/// build-hook path can install `[build-dependencies]` into a staging dir
/// under the same lock without deadlocking on a re-acquire.
#[allow(clippy::too_many_arguments)]
fn install_locked(
    project: &Path,
    cfg: &Config,
    store: &Store,
    manifest: &Manifest,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
'''
replace_once("src/ops.rs", install_old, install_new)
replace_once(
    "src/ops.rs",
    '''    let resolved_target = resolve_target(project, &manifest, target);
''',
    '''    let resolved_target = resolve_target(project, manifest, target);
''',
)
replace_once(
    "src/ops.rs",
    '''        install_locked(
            &deps_dir,
            cfg,
            store,
            false,
''',
    '''        install_locked(
            &deps_dir,
            cfg,
            store,
            &staging_manifest,
            false,
''',
)

replace_once(
    "src/ops.rs",
    '''pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    let (rest, req) = match spec.split_once('@') {
        Some((rest, req)) => (rest.to_string(), Some(req.to_string())),
        None => (spec.to_string(), None),
    };
    let (org, name) = split_key(&rest)?;
    let req = match req {
        Some(req) => {
            if req.trim().is_empty() {
                bail!("empty requirement for {org}/{name}");
            }
            // Any non-empty spec is valid: a semver range or an opaque tag.
            req
        }
        None => {
            let reg = registry_for(&cfg.registry)?;
            let pkg = reg.get_package(&org, &name)?;
            let latest = pkg
                .latest
                .with_context(|| format!("{org}/{name} has no published versions"))?;
            // Caret-range a semver-ish latest; pin an opaque tag exactly.
            match version::parse_version(&latest) {
                Some(_) => format!("^{latest}"),
                None => latest,
            }
        }
    };
    let mut manifest = read_manifest(project)?;
    manifest
        .dependencies
        .insert(format!("{org}/{name}"), req.clone());
''',
    '''pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    let (key, req) = resolve_dependency_spec(cfg, spec)?;
    let (org, name) = split_key(&key)?;
    let mut manifest = read_manifest(project)?;
    manifest.dependencies.insert(key, req.clone());
''',
)
replace_once(
    "src/ops.rs",
    '''        // [install].target or project inference, same as a bare `zed install`.
        None,
    )?;
''',
    '''        // [install].target or project inference, same as a bare `zed install`.
        None,
        false,
        &[],
    )?;
''',
)
# The remove call has the same trailing snippet; replace the remaining copy.
replace_once(
    "src/ops.rs",
    '''        // [install].target or project inference, same as a bare `zed install`.
        None,
    )?;
''',
    '''        // [install].target or project inference, same as a bare `zed install`.
        None,
        false,
        &[],
    )?;
''',
)

replace_once(
    ".cli-flags.toml",
    '''allow_unknown = true
''',
    '''allow_unknown = false
''',
)
replace_once(
    ".cli-flags.toml",
    '''[flags.force]
''',
    '''[flags.allow_no_manifest]
env = "ZED_PKG_ALLOW_NO_MANIFEST"
aliases = ["allow-no-manifest", "skip-manifest"]
type = "bool"
default = "false"
help = "Allow a best-effort install when .zpkg.toml is absent."

[flags.force]
''',
)
replace_once(
    ".cli-flags.toml",
    '''[commands.build]
''',
    '''[commands.completions]
help = "Generate a static shell-completion script."

[commands.build]
''',
)

replace_once(
    "README.md",
    '''# consume packages
zed add acme/http-kit@^1
zed install
zed find http
''',
    '''# consume packages with a manifest
zed add acme/http-kit@^1
zed install

# or install directly into an existing Node/Rust/Go/Python/etc. folder
zed install --allow-no-manifest acme/http-kit@^1
zed find http
''',
)
replace_once(
    "README.md",
    '''| `zed install` (`zed i`) | Resolve, download once into the store, symlink into `zed_modules/` |
''',
    '''| `zed install` (`zed i`) | Resolve manifest dependencies, or install explicit package operands into a manifestless consumer folder after confirmation |
''',
)
replace_once(
    "README.md",
    '''| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |
''',
    '''| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |
| `zed completions bash` | Print a static Bash completion script generated from the clap command model |
''',
)
replace_once(
    "README.md",
    '''### Where dependencies land (`[install].dir`)
''',
    '''### Installing without `.zpkg.toml`

`zed install` also accepts package operands directly in an existing consumer
folder:

```sh
zed install acme/http-kit@^1
```

When `.zpkg.toml` is absent and stdin/stderr are terminals, zed prints the
packages, inferred language target, adapter, and install directory, then asks
for confirmation. CI and other non-interactive callers must opt in explicitly:

```sh
zed install --allow-no-manifest acme/http-kit@^1 acme/logkit
# visible alias with identical behavior:
zed install --skip-manifest acme/http-kit@^1
```

The native project markers (`package.json`, `Cargo.toml`, `go.mod`,
`pyproject.toml`, and others) select a polyglot target. Node and Java projects
also get their ecosystem adapter; other ecosystems receive the inferred target
under `zed_modules/`. zed writes `.zpkg.lock` and installed files but does not
create a synthetic `.zpkg.toml`. A later manifestless `--frozen` install may
reconstruct its direct package set from that lockfile.

### Bash completion

```sh
mkdir -p ~/.local/share/bash-completion/completions
zed completions bash > ~/.local/share/bash-completion/completions/zed
# current shell only:
source <(zed completions bash)
```

The generated script is static: Bash completion does not invoke zed or parse
TOML on every tab press.

### Where dependencies land (`[install].dir`)
''',
)
replace_once(
    "README.md",
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
''',
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--allow-no-manifest` / `--skip-manifest` | `ZED_PKG_ALLOW_NO_MANIFEST` | off; otherwise requires an interactive confirmation |
''',
)

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Test (nextest)
        run: cargo nextest run
        working-directory: zed-cli
      # nextest does not run doctests; keep them covered (cargo test used to).
''',
    '''      - name: Test (nextest)
        run: cargo nextest run --locked
        working-directory: zed-cli
      - name: Clippy
        run: cargo clippy --locked --all-targets -- -D warnings
        working-directory: zed-cli
      - name: Release build
        run: cargo build --locked --release
        working-directory: zed-cli
      - name: Bash completion uses real programmable-completion builtins
        shell: bash
        run: |
          set -euo pipefail
          target/release/zed completions bash > "$RUNNER_TEMP/zed.bash"
          bash -n "$RUNNER_TEMP/zed.bash"
          bash --noprofile --norc -c '
            set -euo pipefail
            source "$1"
            complete -p zed | grep -F "_zed"
          ' bash "$RUNNER_TEMP/zed.bash"
          grep -F -- "--allow-no-manifest" "$RUNNER_TEMP/zed.bash"
          grep -F -- "--skip-manifest" "$RUNNER_TEMP/zed.bash"
          grep -F -- "completions" "$RUNNER_TEMP/zed.bash"
        working-directory: zed-cli
      # nextest does not run doctests; keep them covered (cargo test used to).
''',
)
replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Doctests
        run: cargo test --doc
''',
    '''      - name: Doctests
        run: cargo test --locked --doc
''',
)
replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Symlink install works when the store is mounted
''',
    '''      - name: Manifestless install fails closed without explicit CI consent
        run: |
          cp -R "$RUNNER_TEMP/fixtures/node-app" "$RUNNER_TEMP/manifestless-node-app"
          rm -f "$RUNNER_TEMP/manifestless-node-app/.zpkg.toml" \
            "$RUNNER_TEMP/manifestless-node-app/.zpkg.lock"
          rm -rf "$RUNNER_TEMP/manifestless-node-app/.vendor" \
            "$RUNNER_TEMP/manifestless-node-app/node_modules" \
            "$RUNNER_TEMP/manifestless-node-app/.zed"
          if docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-node-app:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install \
              --registry file:///registry \
              --home /zed-home \
              zed-pkg/docker-node-lib@^1
          then
            echo "manifestless non-interactive install unexpectedly succeeded without consent"
            exit 1
          fi

      - name: Manifestless Node install infers target and adapter
        run: |
          docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-node-app:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            sh -euc '
              zed install \
                --allow-no-manifest \
                --registry file:///registry \
                --home /zed-home \
                zed-pkg/docker-node-lib@^1
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              test -L zed_modules/zed-pkg/docker-node-lib
              test -L node_modules/@zed-pkg/docker-node-lib
              node src/main.js
            '

      - name: Symlink install works when the store is mounted
''',
)
