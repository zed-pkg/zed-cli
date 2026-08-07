#!/usr/bin/env python3
"""Apply the reviewed manifestless-install implementation to zed-cli.

This is a one-shot branch materializer. The workflow removes this file before
committing the product diff.
"""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    source = file.read_text(encoding="utf-8")
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match in {path}, found {count}")
    file.write_text(source.replace(old, new, 1), encoding="utf-8")


MANIFESTLESS_RS = r'''//! Manifestless dependency installation.
//!
//! A missing `.zpkg.toml` is a consent boundary, not a reason to duplicate the
//! installer. This module builds a scoped in-memory consumer manifest and then
//! delegates to the normal resolver/materializer, so target inference, adapter
//! selection, locking, integrity checks, build consent, and store semantics stay
//! identical to a manifest-backed install.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::version;

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};
use crate::ops;
use crate::registry::registry_for;

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_no_manifest: bool,
) -> Result<ops::InstallOutcome> {
    if project.join(MANIFEST_FILE).is_file() {
        if !specs.is_empty() {
            if frozen {
                bail!("--frozen cannot be combined with package specs; update the manifest and lockfile first");
            }
            let additions = resolve_specs(cfg, specs)?;
            let mut manifest = config::read_manifest(project)?;
            for (key, requirement) in additions {
                manifest.dependencies.insert(key.clone(), requirement.clone());
                println!("added {key} = \"{requirement}\"");
            }
            config::write_manifest(project, &manifest)?;
        }
        return ops::install(
            project,
            cfg,
            frozen,
            mode,
            adapter,
            allow_build,
            target,
        );
    }

    let (dependencies, effective_frozen, source) = if specs.is_empty() {
        (
            dependencies_from_lock(project)?,
            true,
            format!("the existing {LOCKFILE_FILE}"),
        )
    } else {
        if frozen {
            bail!("--frozen cannot be combined with package specs when no {MANIFEST_FILE} exists");
        }
        (
            resolve_specs(cfg, specs)?,
            false,
            "the command line".to_string(),
        )
    };

    confirm_manifestless(project, &dependencies, &source, allow_no_manifest)?;

    let manifest_text = synthetic_manifest(project, &dependencies)?;
    Manifest::parse(&manifest_text).context("building the in-memory manifestless install plan")?;

    eprintln!(
        "manifestless install: no {MANIFEST_FILE} will be written; zed will use its normal project-marker target/adapter inference and will write {LOCKFILE_FILE} plus the inferred dependency layout"
    );

    config::with_manifest_override(project, manifest_text, || {
        ops::install(
            project,
            cfg,
            effective_frozen,
            mode,
            adapter,
            allow_build,
            target,
        )
    })
}

fn resolve_specs(cfg: &Config, specs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut dependencies = BTreeMap::new();
    for spec in specs {
        let (key, explicit) = split_dependency_spec(spec)?;
        let requirement = match explicit {
            Some(requirement) => requirement,
            None => {
                let (org, name) = ops::split_key(&key)?;
                let package = registry_for(&cfg.registry)?.get_package(&org, &name)?;
                let latest = package
                    .latest
                    .with_context(|| format!("{key} has no published versions"))?;
                if version::parse_version(&latest).is_some() {
                    format!("^{latest}")
                } else {
                    latest
                }
            }
        };
        if let Some(previous) = dependencies.insert(key.clone(), requirement.clone())
            && previous != requirement
        {
            bail!(
                "conflicting requirements for {key}: `{previous}` and `{requirement}`"
            );
        }
    }
    Ok(dependencies)
}

fn split_dependency_spec(spec: &str) -> Result<(String, Option<String>)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("dependency spec cannot be empty");
    }
    let (key, requirement) = match trimmed.rsplit_once('@') {
        Some((key, requirement)) if key.contains('/') => {
            if requirement.trim().is_empty() {
                bail!("empty requirement for {key}");
            }
            (key, Some(requirement.trim().to_string()))
        }
        _ => (trimmed, None),
    };
    let (org, name) = ops::split_key(key)?;
    Ok((format!("{org}/{name}"), requirement))
}

fn dependencies_from_lock(project: &Path) -> Result<BTreeMap<String, String>> {
    let path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "no {MANIFEST_FILE} and no dependency specs were provided; pass e.g. `zed install acme/http-kit`, or provide {LOCKFILE_FILE} for a locked reinstall"
        )
    })?;
    let lock = Lockfile::parse(&text).with_context(|| format!("invalid {}", path.display()))?;
    if lock.packages.is_empty() {
        bail!("{LOCKFILE_FILE} contains no packages to install");
    }
    Ok(lock
        .packages
        .iter()
        .map(|package| (format!("{}/{}", package.org, package.name), "*".to_string()))
        .collect())
}

fn synthetic_manifest(project: &Path, dependencies: &BTreeMap<String, String>) -> Result<String> {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());

    let mut repository = toml::map::Map::new();
    repository.insert("vcs".to_string(), toml::Value::String("git".to_string()));
    repository.insert(
        "url".to_string(),
        toml::Value::String(format!("https://example.invalid/manifestless/{name}")),
    );

    let mut package = toml::map::Map::new();
    package.insert(
        "org".to_string(),
        toml::Value::String("manifestless".to_string()),
    );
    package.insert("name".to_string(), toml::Value::String(name));
    package.insert(
        "version".to_string(),
        toml::Value::String("0.0.0".to_string()),
    );
    package.insert("repository".to_string(), toml::Value::Table(repository));

    let dependency_table = dependencies
        .iter()
        .map(|(key, requirement)| {
            (
                key.clone(),
                toml::Value::String(requirement.clone()),
            )
        })
        .collect();

    let mut root = toml::map::Map::new();
    root.insert("package".to_string(), toml::Value::Table(package));
    root.insert(
        "dependencies".to_string(),
        toml::Value::Table(dependency_table),
    );
    Ok(toml::to_string_pretty(&toml::Value::Table(root))?)
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn confirm_manifestless(
    project: &Path,
    dependencies: &BTreeMap<String, String>,
    source: &str,
    allow_no_manifest: bool,
) -> Result<()> {
    if allow_no_manifest {
        return Ok(());
    }
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_manifestless_with(
        project,
        dependencies,
        source,
        interactive,
        &mut input,
        &mut output,
    )
}

fn confirm_manifestless_with<R: BufRead, W: Write>(
    project: &Path,
    dependencies: &BTreeMap<String, String>,
    source: &str,
    interactive: bool,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    if !interactive {
        bail!(
            "no {MANIFEST_FILE} found in {} and stdin is not interactive; re-run with --allow-no-manifest or --skip-manifest",
            project.display()
        );
    }

    writeln!(
        output,
        "No {MANIFEST_FILE} was found in {}.",
        project.display()
    )?;
    writeln!(
        output,
        "Install {} package(s) from {source} without creating a manifest?",
        dependencies.len()
    )?;
    writeln!(
        output,
        "Zed will infer the language target and supported ecosystem adapter from the folder, write {LOCKFILE_FILE}, and materialize dependencies under its normal project layout."
    )?;
    write!(output, "Continue? [y/N] ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }
    bail!("manifestless install cancelled")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn explicit_specs_do_not_need_registry_access() {
        assert_eq!(
            split_dependency_spec("acme/http-kit@^1.2").unwrap(),
            ("acme/http-kit".to_string(), Some("^1.2".to_string()))
        );
        assert_eq!(
            split_dependency_spec("acme/http-kit").unwrap(),
            ("acme/http-kit".to_string(), None)
        );
    }

    #[test]
    fn synthetic_manifest_uses_real_dependency_keys_without_touching_disk() {
        let project = tempfile::tempdir().unwrap();
        let mut dependencies = BTreeMap::new();
        dependencies.insert("acme/http-kit".to_string(), "^1".to_string());
        let text = synthetic_manifest(project.path(), &dependencies).unwrap();
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(
            manifest.dependencies.get("acme/http-kit").map(String::as_str),
            Some("^1")
        );
        assert!(!project.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn interactive_confirmation_is_fail_closed() {
        let project = Path::new("/tmp/example");
        let dependencies = BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]);

        let mut output = Vec::new();
        confirm_manifestless_with(
            project,
            &dependencies,
            "the command line",
            true,
            &mut Cursor::new(b"yes\n"),
            &mut output,
        )
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("Continue?"));

        let error = confirm_manifestless_with(
            project,
            &dependencies,
            "the command line",
            true,
            &mut Cursor::new(b"no\n"),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn noninteractive_install_requires_an_explicit_bypass() {
        let error = confirm_manifestless_with(
            Path::new("/tmp/example"),
            &BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]),
            "the command line",
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--allow-no-manifest"));
        assert!(message.contains("--skip-manifest"));
    }
}
'''


Path("src/manifestless.rs").write_text(MANIFESTLESS_RS, encoding="utf-8")

replace_once(
    "src/lib.rs",
    "pub mod flags;\n",
    "pub mod flags;\npub mod manifestless;\n",
    "export manifestless module",
)

replace_once(
    "src/main.rs",
    "use zed_cli::flags;\n",
    "use zed_cli::flags;\nuse zed_cli::manifestless;\n",
    "import manifestless dispatcher",
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
        .map(|_| ()),''',
    '''        Cmd::Install {
            specs,
            frozen,
            install_mode,
            adapter,
            allow_build,
            target,
            allow_no_manifest,
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
        )
        .map(|_| ()),''',
    "route install through manifestless boundary",
)

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
    },''',
    '''    /// Resolve and install dependencies into the project
    #[command(alias = "i")]
    Install {
        /// Package specs to install (`org/name[@requirement]`). With a manifest
        /// they are added before installation; without one they form an
        /// in-memory install plan and no manifest is created.
        specs: Vec<String>,
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
        /// Permit installation when `.zpkg.toml` is absent without prompting.
        /// `--skip-manifest` is an equivalent spelling for automation.
        #[arg(
            long,
            visible_alias = "skip-manifest",
            env = "ZED_PKG_ALLOW_NO_MANIFEST"
        )]
        allow_no_manifest: bool,
    },''',
    "extend install CLI",
)

replace_once(
    "src/cli.rs",
    '''    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
    /// every user-facing option must be settable via a ZED_PKG_* env var.
    #[test]
    fn flags_2_env_convention_holds() {''',
    '''    #[test]
    fn install_accepts_manifestless_specs_and_bypass_aliases() {
        for bypass in ["--allow-no-manifest", "--skip-manifest"] {
            let cli = Cli::try_parse_from([
                "zed",
                "install",
                "acme/http-kit@^1",
                bypass,
            ])
            .unwrap();
            match cli.cmd {
                Cmd::Install {
                    specs,
                    allow_no_manifest,
                    ..
                } => {
                    assert_eq!(specs, vec!["acme/http-kit@^1"]);
                    assert!(allow_no_manifest);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
    /// every user-facing option must be settable via a ZED_PKG_* env var.
    #[test]
    fn flags_2_env_convention_holds() {''',
    "test manifestless CLI spellings",
)

replace_once(
    "src/config.rs",
    "use std::collections::BTreeMap;\n",
    "use std::cell::RefCell;\nuse std::collections::BTreeMap;\n",
    "import manifest override storage",
)
replace_once(
    "src/config.rs",
    "use anyhow::{Context, Result};\n",
    "use anyhow::{Context, Result, anyhow};\n",
    "import anyhow constructor",
)

replace_once(
    "src/config.rs",
    '''pub fn read_manifest(project: &Path) -> Result<Manifest> {
    let path = project.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no {MANIFEST_FILE} found in {}", project.display()))?;
    Manifest::parse(&text).with_context(|| format!("invalid manifest {}", path.display()))
}''',
    '''#[derive(Debug)]
struct ManifestOverride {
    project: PathBuf,
    text: String,
}

thread_local! {
    static MANIFEST_OVERRIDE: RefCell<Option<ManifestOverride>> = const { RefCell::new(None) };
}

struct ManifestOverrideGuard;

impl Drop for ManifestOverrideGuard {
    fn drop(&mut self) {
        MANIFEST_OVERRIDE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

fn normalized_project(project: &Path) -> PathBuf {
    fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf())
}

/// Run one installer operation with an in-memory root manifest. Package
/// manifests read from the store still come from disk; only the exact consumer
/// directory is overridden. The guard is thread-local and panic-safe, so a
/// manifestless install never creates or leaves a temporary `.zpkg.toml`.
pub(crate) fn with_manifest_override<T>(
    project: &Path,
    text: String,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let project = normalized_project(project);
    MANIFEST_OVERRIDE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(anyhow!("a manifest override is already active on this thread"));
        }
        *slot = Some(ManifestOverride { project, text });
        Ok(())
    })?;
    let guard = ManifestOverrideGuard;
    let result = operation();
    drop(guard);
    result
}

pub fn read_manifest(project: &Path) -> Result<Manifest> {
    let normalized = normalized_project(project);
    let override_text = MANIFEST_OVERRIDE.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|manifest| manifest.project == normalized)
            .map(|manifest| manifest.text.clone())
    });
    if let Some(text) = override_text {
        return Manifest::parse(&text)
            .with_context(|| format!("invalid in-memory manifest for {}", project.display()));
    }

    let path = project.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no {MANIFEST_FILE} found in {}", project.display()))?;
    Manifest::parse(&text).with_context(|| format!("invalid manifest {}", path.display()))
}''',
    "add scoped in-memory manifest override",
)

replace_once(
    "src/config.rs",
    '''    #[test]
    fn relative_home_is_resolved_from_the_invocation_directory() {''',
    '''    #[test]
    fn manifest_override_is_scoped_and_never_written_to_disk() {
        let project = tempfile::tempdir().unwrap();
        let text = r#"
[package]
org = "manifestless"
name = "consumer"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/manifestless/consumer"

[dependencies]
"acme/http-kit" = "^1"
"#;

        with_manifest_override(project.path(), text.to_string(), || {
            let manifest = read_manifest(project.path())?;
            assert_eq!(
                manifest.dependencies.get("acme/http-kit").map(String::as_str),
                Some("^1")
            );
            assert!(!project.path().join(MANIFEST_FILE).exists());
            Ok(())
        })
        .unwrap();

        assert!(read_manifest(project.path()).is_err());
        assert!(!project.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn nested_manifest_overrides_fail_closed() {
        let project = tempfile::tempdir().unwrap();
        let text = r#"
[package]
org = "manifestless"
name = "consumer"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/manifestless/consumer"
"#;
        let error = with_manifest_override(project.path(), text.to_string(), || {
            with_manifest_override(project.path(), text.to_string(), || Ok(()))
        })
        .unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[test]
    fn relative_home_is_resolved_from_the_invocation_directory() {''',
    "test manifest override lifecycle",
)

replace_once(
    ".cli-flags.toml",
    '''[flags.target]
env = "ZED_PKG_TARGET"
aliases = ["target"]
type = "string"
help = "Polyglot package target."

[flags.force]''',
    '''[flags.target]
env = "ZED_PKG_TARGET"
aliases = ["target"]
type = "string"
help = "Polyglot package target."

[flags.allow_no_manifest]
env = "ZED_PKG_ALLOW_NO_MANIFEST"
aliases = ["allow-no-manifest", "skip-manifest"]
type = "bool"
default = "false"
help = "Allow a non-interactive install without .zpkg.toml."

[flags.force]''',
    "declare manifestless flags-2-env contract",
)

replace_once(
    "README.md",
    '''# consume packages
zed add acme/http-kit@^1
zed install
zed find http''',
    '''# consume packages with a manifest
zed add acme/http-kit@^1
zed install

# or install directly into a folder that has no .zpkg.toml
zed install acme/http-kit@^1
zed find http''',
    "document manifestless quickstart",
)
replace_once(
    "README.md",
    '''| `zed install` (`zed i`) | Resolve, download once into the store, symlink into `zed_modules/` |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers) |''',
    '''| `zed install [<org>/<name>[@req] ...]` (`zed i`) | Resolve, download once into the store, and materialize dependencies; package specs are accepted with or without `.zpkg.toml` |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers, including manifestless locked reinstalls) |''',
    "update install command table",
)
replace_once(
    "README.md",
    '''### Where dependencies land (`[install].dir`)''',
    '''### Installing without `.zpkg.toml`

`zed install` can consume explicit package specs in any existing folder:

```sh
zed install acme/http-kit@^1 acme/logging
```

When `.zpkg.toml` is absent, zed does **not** silently invent or persist a
package manifest. In an interactive terminal it explains the inferred behavior
and asks for confirmation. Automation must opt in explicitly with either
`--allow-no-manifest` or its equivalent spelling `--skip-manifest` (or
`ZED_PKG_ALLOW_NO_MANIFEST=1`).

The normal installer remains authoritative: project markers such as
`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `pubspec.yaml`,
`gleam.toml`, and JVM build files select a polyglot target; Node and JVM
adapters add their ecosystem-specific links; every project still receives the
universal `zed_modules/` layout. Zed writes `.zpkg.lock` for reproducibility but
never writes `.zpkg.toml` in this mode. A later manifestless invocation with no
package specs uses the existing lockfile as a frozen reinstall.

With an existing manifest, `zed install <spec>...` adds those dependencies to
`[dependencies]` once and then runs the normal install. Use `zed install`
without specs to install the manifest as before.

### Where dependencies land (`[install].dir`)''',
    "add manifestless behavior section",
)
replace_once(
    "README.md",
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--force` (build) | `ZED_PKG_FORCE` | off |''',
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--allow-no-manifest` / `--skip-manifest` (install) | `ZED_PKG_ALLOW_NO_MANIFEST` | off; interactive confirmation is required when no manifest exists |
| `--force` (build) | `ZED_PKG_FORCE` | off |''',
    "document manifestless env flag",
)
