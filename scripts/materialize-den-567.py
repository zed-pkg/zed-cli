#!/usr/bin/env python3
"""Materialize the consolidated DEN-567 product diff.

The connected GitHub surface supports whole-file writes but not unified patch
application. This one-shot helper applies reviewed replacements on the feature
branch; its workflow formats, lints, and tests the result, then removes both
bootstrap files before committing the product changes.
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match in {path}, found {count}")
    write(path, content.replace(old, new, 1))


MANIFESTLESS_RS = r'''//! Manifestless and transient dependency installation.
//!
//! A missing `.zpkg.toml` is a consent boundary, not a second installer. This
//! module selects a conservative project root, builds an in-memory consumer
//! manifest, and delegates to the normal resolver/materializer. Store locking,
//! integrity checks, target slicing, build consent, bin hoisting, and ecosystem
//! adapters therefore remain identical to manifest-backed installs.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::{DirEntry, WalkDir};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, VersionScheme};

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};
use crate::ops;
use crate::registry::registry_for;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectSelection {
    root: PathBuf,
    has_manifest: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    requested: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_no_manifest: bool,
) -> Result<ops::InstallOutcome> {
    let selection = select_project(requested);

    if selection.has_manifest {
        if specs.is_empty() {
            return ops::install(
                &selection.root,
                cfg,
                frozen,
                mode,
                adapter,
                allow_build,
                target,
            );
        }
        if frozen {
            bail!(
                "--frozen cannot be combined with package specs; use `zed add` to persist a \
                 dependency or install the existing lockfile without operands"
            );
        }

        let mut manifest = config::read_manifest(&selection.root)?;
        for (key, requirement) in resolve_specs(cfg, specs)? {
            manifest.dependencies.insert(key.clone(), requirement.clone());
            eprintln!(
                "transient dependency: {key}@{requirement} (the existing {MANIFEST_FILE} is unchanged)"
            );
        }
        let manifest_text = manifest.to_toml_string()?;
        return config::with_manifest_override(&selection.root, manifest_text, || {
            ops::install(
                &selection.root,
                cfg,
                false,
                mode,
                adapter,
                allow_build,
                target,
            )
        });
    }

    let dependencies = if specs.is_empty() {
        if !frozen {
            bail!(
                "no {MANIFEST_FILE} was found and no package specs were supplied; pass one or \
                 more `org/name[@requirement]` operands, or use --frozen with an existing \
                 {LOCKFILE_FILE}"
            );
        }
        dependencies_from_lock(&selection.root)?
    } else {
        if frozen {
            bail!(
                "--frozen cannot be combined with package specs when no {MANIFEST_FILE} exists"
            );
        }
        resolve_specs(cfg, specs)?
    };

    let effective_adapter = match adapter {
        Adapter::Auto => ops::detect_adapter(&selection.root),
        other => other,
    };
    let effective_target = target
        .map(str::to_owned)
        .or_else(|| ops::detect_target(&selection.root))
        .unwrap_or_else(|| "universal".to_string());

    confirm_manifestless(
        &selection.root,
        &effective_target,
        adapter_name(effective_adapter),
        &dependencies,
        frozen,
        allow_no_manifest,
    )?;

    if allow_no_manifest {
        eprintln!(
            "warning: proceeding without {MANIFEST_FILE}; install root={}, target={}, adapter={}",
            selection.root.display(),
            effective_target,
            adapter_name(effective_adapter)
        );
    }

    let manifest = synthetic_manifest(&selection.root, dependencies);
    let manifest_text = manifest.to_toml_string()?;
    config::with_manifest_override(&selection.root, manifest_text, || {
        ops::install(
            &selection.root,
            cfg,
            frozen,
            mode,
            adapter,
            allow_build,
            target,
        )
    })
}

fn select_project(requested: &Path) -> ProjectSelection {
    if let Some(root) = manifest_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: true,
        };
    }
    if let Some(root) = native_or_lock_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    ProjectSelection {
        root: unique_nested_project(requested).unwrap_or_else(|| requested.to_path_buf()),
        has_manifest: false,
    }
}

fn manifest_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(MANIFEST_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn native_or_lock_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(LOCKFILE_FILE).is_file() || ops::detect_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Select a nested project only when there is exactly one plausible native
/// project below the requested folder. Multiple candidates are a monorepo, and
/// guessing between them is less safe than the universal layout at the
/// requested root.
fn unique_nested_project(start: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir())
        .filter(|entry| {
            entry.path().join(LOCKFILE_FILE).is_file()
                || ops::detect_target(entry.path()).is_some()
        })
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort();
    candidates.dedup();
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !name.starts_with('.')
        && !matches!(
            name.as_ref(),
            "node_modules" | "zed_modules" | "target" | "vendor" | "dist" | "build"
        )
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
                bail!("empty requirement in package spec `{spec}`");
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
            "--frozen manifestless install requires an existing {}",
            path.display()
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

fn synthetic_manifest(project: &Path, dependencies: BTreeMap<String, String>) -> Manifest {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    Manifest {
        package: PackageSection {
            org: "zed-manifestless".to_string(),
            name: name.clone(),
            version: "0.0.0".to_string(),
            version_scheme: VersionScheme::Semver,
            description: Some("Transient manifestless Zed consumer".to_string()),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: format!("https://localhost/zed-manifestless/{name}"),
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
    }
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

fn adapter_name(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Auto => "auto",
        Adapter::None => "none",
        Adapter::Node => "node",
        Adapter::Java => "java",
    }
}

fn confirm_manifestless(
    project: &Path,
    target: &str,
    adapter: &str,
    dependencies: &BTreeMap<String, String>,
    frozen: bool,
    allow_no_manifest: bool,
) -> Result<()> {
    if allow_no_manifest {
        return Ok(());
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_manifestless_with(
        project,
        target,
        adapter,
        dependencies,
        frozen,
        stdin.is_terminal(),
        &mut input,
        &mut output,
    )
}

#[allow(clippy::too_many_arguments)]
fn confirm_manifestless_with<R: BufRead, W: Write>(
    project: &Path,
    target: &str,
    adapter: &str,
    dependencies: &BTreeMap<String, String>,
    frozen: bool,
    interactive: bool,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    if !interactive {
        bail!(
            "no {MANIFEST_FILE} found for {} and stdin is not an interactive terminal; re-run \
             with --allow-no-manifest or --skip-manifest",
            project.display()
        );
    }

    writeln!(output, "No {MANIFEST_FILE} was found.")?;
    writeln!(output, "  install root: {}", project.display())?;
    writeln!(output, "  detected target: {target}")?;
    writeln!(output, "  ecosystem adapter: {adapter}")?;
    if frozen {
        writeln!(output, "  dependencies: existing {LOCKFILE_FILE} (--frozen)")?;
    } else {
        writeln!(output, "  transient dependencies:")?;
        for (name, requirement) in dependencies {
            writeln!(output, "    {name}@{requirement}")?;
        }
    }
    write!(
        output,
        "Proceed without creating {MANIFEST_FILE}? [y/N] "
    )?;
    output.flush()?;

    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        bail!(
            "confirmation input closed; installation cancelled (use --allow-no-manifest or \
             --skip-manifest for intentional non-interactive use)"
        );
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("manifestless installation cancelled")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn dependency_specs_support_explicit_and_latest_requirements() {
        assert_eq!(
            split_dependency_spec("acme/http-kit@^1.2").unwrap(),
            ("acme/http-kit".to_string(), Some("^1.2".to_string()))
        );
        assert_eq!(
            split_dependency_spec("acme/http-kit").unwrap(),
            ("acme/http-kit".to_string(), None)
        );
        assert!(split_dependency_spec("not-a-package").is_err());
        assert!(split_dependency_spec("acme/http-kit@").is_err());
    }

    #[test]
    fn explicit_yes_is_required_and_eof_is_distinct() {
        let dependencies = BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]);
        for accepted in [b"y\n".as_slice(), b"YES\n".as_slice()] {
            let mut output = Vec::new();
            confirm_manifestless_with(
                Path::new("/tmp/project"),
                "node",
                "node",
                &dependencies,
                false,
                true,
                &mut Cursor::new(accepted),
                &mut output,
            )
            .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("install root"));
            assert!(rendered.contains("acme/http-kit@^1"));
            assert!(rendered.contains("[y/N]"));
        }

        for rejected in [b"\n".as_slice(), b"n\n".as_slice(), b"maybe\n".as_slice()] {
            assert!(
                confirm_manifestless_with(
                    Path::new("/tmp/project"),
                    "node",
                    "node",
                    &dependencies,
                    false,
                    true,
                    &mut Cursor::new(rejected),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }

        let error = confirm_manifestless_with(
            Path::new("/tmp/project"),
            "node",
            "node",
            &dependencies,
            false,
            true,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("input closed"));
    }

    #[test]
    fn noninteractive_input_requires_an_explicit_bypass() {
        let error = confirm_manifestless_with(
            Path::new("/tmp/project"),
            "rust",
            "none",
            &BTreeMap::from([("acme/tool".to_string(), "^1".to_string())]),
            false,
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--allow-no-manifest"));
        assert!(error.contains("--skip-manifest"));
    }

    #[test]
    fn one_nested_native_project_is_selected() {
        let temp = tempfile::tempdir().unwrap();
        let web = temp.path().join("apps/web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), "{}").unwrap();
        assert_eq!(select_project(temp.path()).root, web);
    }

    #[test]
    fn ambiguous_monorepo_keeps_the_requested_root() {
        let temp = tempfile::tempdir().unwrap();
        for path in ["apps/web/package.json", "apps/api/Cargo.toml"] {
            let path = temp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        assert_eq!(select_project(temp.path()).root, temp.path());
    }

    #[test]
    fn nearest_native_ancestor_wins_from_a_source_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("apps/web/src/components");
        fs::create_dir_all(&source).unwrap();
        fs::write(temp.path().join("apps/web/package.json"), "{}").unwrap();
        assert_eq!(select_project(&source).root, temp.path().join("apps/web"));
    }

    #[test]
    fn synthetic_manifest_never_touches_disk() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = synthetic_manifest(
            temp.path(),
            BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]),
        );
        assert_eq!(
            manifest
                .dependencies
                .get("acme/http-kit")
                .map(String::as_str),
            Some("^1")
        );
        assert!(!temp.path().join(MANIFEST_FILE).exists());
    }
}
'''

COMPLETION_RS = r'''//! Shell-completion generation from the same clap command model used at runtime.

use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;

/// Write a static completion script for `zed` to stdout.
pub fn print(shell: Shell) {
    let mut command = Cli::command();
    generate(shell, &mut command, "zed", &mut io::stdout());
}

#[cfg(test)]
fn render(shell: Shell) -> String {
    let mut command = Cli::command();
    let mut output = Vec::new();
    generate(shell, &mut command, "zed", &mut output);
    String::from_utf8(output).expect("completion output must be UTF-8")
}

#[cfg(test)]
mod tests {
    use clap_complete::Shell;

    use super::render;

    #[test]
    fn bash_completion_contains_commands_aliases_and_manifestless_flags() {
        let script = render(Shell::Bash);
        assert!(script.contains("_zed"), "missing generated completion function");
        assert!(script.contains("complete"), "missing Bash completion registration");
        for command in ["install", "init", "completions", "self-update", "r2g"] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in ["--allow-no-manifest", "--skip-manifest", "--install-mode"] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }

    #[test]
    fn zsh_completion_contains_registration_commands_and_manifestless_flags() {
        let script = render(Shell::Zsh);
        assert!(script.contains("#compdef zed"), "missing zsh compdef header");
        assert!(script.contains("_zed"), "missing zsh completion function");
        for command in ["install", "init", "completions", "self-update", "r2g"] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in ["--allow-no-manifest", "--skip-manifest", "--install-mode"] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }
}
'''

write("src/manifestless.rs", MANIFESTLESS_RS)
write("src/completion.rs", COMPLETION_RS)

replace_once(
    "Cargo.toml",
    'flags2env = { git = "https://github.com/ORESoftware/flags-2-env.git", rev = "450031f54468d4fd054131effb6b5f300d3d1310" }',
    'flags2env = { git = "https://github.com/ORESoftware/flags-2-env.git", rev = "9483b92c1fb259f598858fdd2bef66417d87fb2c" }',
    "pin merged flags2env revision",
)

replace_once(
    "src/lib.rs",
    "pub mod flags;\npub mod ops;",
    "pub mod flags;\npub mod manifestless;\npub mod ops;",
    "export manifestless module",
)

replace_once(
    "src/main.rs",
    "use zed_cli::config::Config;\nuse zed_cli::ops;",
    "use zed_cli::completion;\nuse zed_cli::config::Config;\nuse zed_cli::manifestless;\nuse zed_cli::ops;",
    "import completion and manifestless dispatchers",
)
replace_once(
    "src/main.rs",
    '''fn main() {
    zed_cli::flags::apply_cli_flags();
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}''',
    '''fn main() {
    if let Err(error) = zed_cli::flags::apply_cli_flags() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    let cli = Cli::parse();
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}''',
    "fail closed when flags2env startup fails",
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
        .map(|_| ()),
        Cmd::Completions { shell } => {
            completion::print(shell.into());
            Ok(())
        }''',
    "route install and completion commands",
)

replace_once(
    "src/cli.rs",
    '''impl ContainerRuntime {
    pub fn program(self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}
''',
    '''impl ContainerRuntime {
    pub fn program(self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => clap_complete::Shell::Bash,
            CompletionShell::Zsh => clap_complete::Shell::Zsh,
        }
    }
}
''',
    "declare supported completion shells",
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
    },
    /// Run (or warm the build cache for) the [build] steps the locked''',
    '''    /// Resolve and install dependencies into the selected project
    #[command(alias = "i")]
    Install {
        /// Transient package specs (`org/name[@requirement]`). With no manifest
        /// these form the in-memory consumer plan; an existing manifest is
        /// never edited by this command (`zed add` persists dependencies).
        #[arg(value_name = "PACKAGE")]
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
        /// Proceed without prompting when no .zpkg.toml can be found.
        #[arg(
            long,
            visible_alias = "skip-manifest",
            env = "ZED_PKG_ALLOW_NO_MANIFEST"
        )]
        allow_no_manifest: bool,
    },
    /// Generate a completion script from the same typed command model used at runtime
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Run (or warm the build cache for) the [build] steps the locked''',
    "extend install CLI and add completions",
)
replace_once(
    "src/cli.rs",
    '''    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
    /// every user-facing option must be settable via a ZED_PKG_* env var.
    #[test]
    fn flags_2_env_convention_holds() {''',
    '''    #[test]
    fn install_accepts_specs_and_both_manifestless_bypass_spellings() {
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
                    assert_eq!(specs, ["acme/http-kit@^1"]);
                    assert!(allow_no_manifest);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn completion_shells_are_typed_positionals() {
        for shell in ["bash", "zsh"] {
            let cli = Cli::try_parse_from(["zed", "completions", shell]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Completions { .. }));
        }
    }

    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
    /// every user-facing option must be settable via a ZED_PKG_* env var.
    #[test]
    fn flags_2_env_convention_holds() {''',
    "test new CLI surfaces",
)

replace_once(
    "src/config.rs",
    "use std::collections::BTreeMap;\n",
    "use std::cell::RefCell;\nuse std::collections::BTreeMap;\n",
    "import manifest override storage",
)
replace_once(
    "src/config.rs",
    "use anyhow::{Context, Result};",
    "use anyhow::{Context, Result, anyhow};",
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
url = "https://localhost/manifestless/consumer"

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
url = "https://localhost/manifestless/consumer"
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
    "src/ops.rs",
    "fn detect_target(project: &Path) -> Option<String> {",
    "pub(crate) fn detect_target(project: &Path) -> Option<String> {",
    "share target inference",
)
replace_once(
    "src/ops.rs",
    "fn detect_adapter(project: &Path) -> Adapter {",
    "pub(crate) fn detect_adapter(project: &Path) -> Adapter {",
    "share adapter inference",
)

replace_once(
    ".cli-flags.toml",
    '''[parse]
command_env = "ZED_PKG_COMMAND"
allow_unknown = true''',
    '''[parse]
command_env = "ZED_PKG_COMMAND"
unknown_options_env = "ZED_PKG_UNKNOWN_OPTIONS"
errors_env = "ZED_PKG_PARSE_ERRORS"
allow_unknown = false''',
    "fail closed for unknown options",
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
help = "Proceed non-interactively when no .zpkg.toml is present."

[flags.force]''',
    "declare manifestless flags",
)
replace_once(
    ".cli-flags.toml",
    '''[commands.install]
aliases = ["i"]
help = "Install dependencies."

[commands.build]''',
    '''[commands.install]
aliases = ["i"]
help = "Install dependencies."

[commands.completions]
help = "Generate Bash or Zsh completion from the typed CLI model."

[commands.build]''',
    "declare completions command",
)

replace_once(
    "README.md",
    '''# consume packages
zed add acme/http-kit@^1
zed install
zed find http''',
    '''# consume packages from a manifest
zed add acme/http-kit@^1
zed install

# or install transiently in a folder with no .zpkg.toml
zed install acme/http-kit@^1            # confirms in an interactive terminal
zed install acme/http-kit@^1 --skip-manifest  # intentional automation
zed find http''',
    "document manifestless quickstart",
)
replace_once(
    "README.md",
    '''Every package is `<org>/<name>`, declared in a `.zpkg.toml` manifest at the
repo root (TOML only). See `zed init` output for the annotated template.''',
    '''Every authored package is `<org>/<name>`, declared in a `.zpkg.toml` manifest
at the repo root (TOML only). Consumers may also install positional package specs
through a transient in-memory manifest. See `zed init` output for the annotated
authoring template.''',
    "clarify author and consumer manifests",
)
replace_once(
    "README.md",
    '''| `zed install` (`zed i`) | Resolve, download once into the store, symlink into `zed_modules/` |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers) |''',
    '''| `zed install [<org>/<name>[@req] ...]` (`zed i`) | Resolve, download once into the store, and install manifest or transient dependencies |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers, including manifestless locked reinstalls) |''',
    "update install command table",
)
replace_once(
    "README.md",
    '''| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |

### Where dependencies land (`[install].dir`)''',
    '''| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |
| `zed completions bash\|zsh` | Generate shell completion from the same Clap model used by the executable |

### Shell completion

```sh
# Bash for the current shell
source <(zed completions bash)

# Zsh (persistent user completion)
mkdir -p ~/.zfunc
zed completions zsh > ~/.zfunc/_zed
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```

The generated scripts include aliases, subcommands, and install flags directly
from the typed parser. GitHub Actions syntax-checks and registers them in real
Bash and Zsh processes.

### Installing without `.zpkg.toml`

`zed install` accepts transient package specs in an existing repository or
folder:

```sh
zed install oresoftware/flags-2-env@^0.1
```

Zed first searches upward for a Zed manifest. Without one, it looks for the
nearest native project marker (`package.json`, `Cargo.toml`, `go.mod`,
`pyproject.toml`, and other supported ecosystems). When invoked at a repository
shell containing exactly one clear nested app such as `apps/web/package.json`,
that app becomes the install root. Ambiguous monorepos stay at the requested
root and use the safe universal `zed_modules/` layout rather than guessing.

A real interactive terminal prints the selected root, inferred target, adapter,
and dependencies, then accepts only `y` or `yes`. EOF and every other answer
cancel before files are written. Automation must opt in with
`--allow-no-manifest`, its visible alias `--skip-manifest`, or
`ZED_PKG_ALLOW_NO_MANIFEST=1`.

No synthetic `.zpkg.toml` is written. The normal installer still writes
`.zpkg.lock`, `zed_modules/`, hoisted bins, and supported ecosystem adapter
outputs. `zed install --frozen --skip-manifest` can reconstruct a no-manifest
install from an existing lockfile without package operands. Positional specs in
a project that already has a manifest are transient; use `zed add` to persist
them.

### Where dependencies land (`[install].dir`)''',
    "document completion and manifestless behavior",
)
replace_once(
    "README.md",
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--force` (build) | `ZED_PKG_FORCE` | off |''',
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--allow-no-manifest` / `--skip-manifest` (install) | `ZED_PKG_ALLOW_NO_MANIFEST` | off; otherwise a real-terminal confirmation is required |
| `--force` (build) | `ZED_PKG_FORCE` | off |''',
    "document manifestless environment flag",
)

ci = read(".github/workflows/ci.yml")
ci = ci.replace(
    "      - uses: dtolnay/rust-toolchain@stable\n",
    "      - uses: dtolnay/rust-toolchain@stable\n        with:\n          components: rustfmt, clippy\n",
    1,
)
if ci.count("components: rustfmt, clippy") != 1:
    raise RuntimeError("install Rust components: expected exactly one replacement")
ci = ci.replace(
    '''      - name: Doctests
        run: cargo test --doc
        working-directory: zed-cli
''',
    '''      - name: Doctests
        run: cargo test --doc
        working-directory: zed-cli
      - name: Clippy
        if: runner.os == 'Linux'
        run: cargo clippy --all-targets -- -D warnings
        working-directory: zed-cli
''',
    1,
)
if "name: Clippy" not in ci:
    raise RuntimeError("add clippy: replacement did not apply")
ci += r'''

      - name: Prepare manifestless consumer fixtures
        run: |
          set -euo pipefail
          for name in interactive reject allow alias frozen; do
            root="$RUNNER_TEMP/manifestless-$name"
            mkdir -p "$root/src"
            cp "$RUNNER_TEMP/fixtures/node-app/package.json" "$root/package.json"
            cp "$RUNNER_TEMP/fixtures/node-app/src/main.js" "$root/src/main.js"
          done
          nested="$RUNNER_TEMP/manifestless-nested/apps/web"
          mkdir -p "$nested/src"
          cp "$RUNNER_TEMP/fixtures/node-app/package.json" "$nested/package.json"
          cp "$RUNNER_TEMP/fixtures/node-app/src/main.js" "$nested/src/main.js"

      - name: Manifestless install fails closed without a terminal or bypass
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-reject"
          if docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --registry file:///registry \
              --home /zed-home/reject \
              --install-mode copy </dev/null
          then
            echo "non-interactive manifestless install unexpectedly succeeded" >&2
            exit 1
          fi
          test ! -e "$root/.zpkg.toml"
          test ! -e "$root/.zpkg.lock"
          test ! -e "$root/zed_modules"

      - name: Interactive manifestless install confirms on a real pseudo-terminal
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-interactive"
          printf 'y\n' | script -qfec "docker run --rm -it \
            --volume '$root:/work' \
            --volume '$RUNNER_TEMP/registry:/registry:ro' \
            --volume '$RUNNER_TEMP/zed-home:/zed-home' \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --registry file:///registry \
              --home /zed-home/interactive \
              --install-mode copy" /dev/null
          test ! -e "$root/.zpkg.toml"
          test -f "$root/.zpkg.lock"
          test -d "$root/zed_modules/zed-pkg/docker-node-lib"
          test -d "$root/node_modules/@zed-pkg/docker-node-lib"
          docker run --rm --volume "$root:/work:ro" --workdir /work node:22-bookworm-slim node src/main.js

      - name: Both non-interactive bypass spellings install without a manifest
        run: |
          set -euo pipefail
          for pair in "allow --allow-no-manifest" "alias --skip-manifest"; do
            set -- $pair
            name="$1"
            flag="$2"
            root="$RUNNER_TEMP/manifestless-$name"
            docker run --rm \
              --volume "$root:/work" \
              --volume "$RUNNER_TEMP/registry:/registry:ro" \
              --volume "$RUNNER_TEMP/zed-home:/zed-home" \
              --workdir /work \
              zed-pkg/install-test \
              sh -euc "
                zed install zed-pkg/docker-node-lib@^1 \\
                  $flag \\
                  --registry file:///registry \\
                  --home /zed-home/$name \\
                  --install-mode copy
                test ! -e .zpkg.toml
                test -f .zpkg.lock
                test -d zed_modules/zed-pkg/docker-node-lib
                test -d node_modules/@zed-pkg/docker-node-lib
                node src/main.js
              "
          done

      - name: Frozen lock-only manifestless reinstall remains reproducible
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-frozen"
          docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --skip-manifest \
              --registry file:///registry \
              --home /zed-home/frozen \
              --install-mode copy
          rm -rf "$root/zed_modules" "$root/node_modules" "$root/.zed"
          docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            sh -euc '
              zed install --frozen --skip-manifest \
                --registry file:///registry \
                --home /zed-home/frozen \
                --install-mode copy
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              node src/main.js
            '

      - name: One clear nested native project becomes the install root
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-nested"
          docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            sh -euc '
              zed install zed-pkg/docker-node-lib@^1 \
                --skip-manifest \
                --registry file:///registry \
                --home /zed-home/nested \
                --install-mode copy
              test ! -e /work/.zpkg.toml
              test ! -e /work/.zpkg.lock
              test -f /work/apps/web/.zpkg.lock
              test -d /work/apps/web/node_modules/@zed-pkg/docker-node-lib
              node /work/apps/web/src/main.js
            '

  shell-contract:
    name: Bash, Zsh, and terminal help contracts
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          path: zed-cli
      - uses: actions/checkout@v4
        with:
          repository: zed-pkg/zed-interfaces
          path: zed-interfaces
      - uses: dtolnay/rust-toolchain@stable
      - name: Install Zsh
        run: sudo apt-get update && sudo apt-get install -y zsh
      - name: Build the real executable
        working-directory: zed-cli
        run: cargo build --locked
      - name: Terminal help exposes the synchronized install contract
        shell: bash
        run: |
          set -euo pipefail
          bin="$GITHUB_WORKSPACE/zed-cli/target/debug/zed"
          script -qec "$bin --help" "$RUNNER_TEMP/zed-help.txt" >/dev/null
          script -qec "$bin install --help" "$RUNNER_TEMP/install-help.txt" >/dev/null
          grep -F -- "completions" "$RUNNER_TEMP/zed-help.txt"
          grep -F -- "--allow-no-manifest" "$RUNNER_TEMP/install-help.txt"
          grep -F -- "--skip-manifest" "$RUNNER_TEMP/install-help.txt"
          grep -F -- "Proceed without prompting" "$RUNNER_TEMP/install-help.txt"
      - name: Bash completion parses, registers, and offers real candidates
        shell: bash
        run: |
          set -euo pipefail
          bin="$GITHUB_WORKSPACE/zed-cli/target/debug/zed"
          completion="$RUNNER_TEMP/zed.bash"
          "$bin" completions bash > "$completion"
          bash -n "$completion"
          grep -F -- "--allow-no-manifest" "$completion"
          grep -F -- "--skip-manifest" "$completion"
          bash --noprofile --norc -c '
            set -euo pipefail
            source "$1"
            complete -p zed | grep -F _zed
            COMP_WORDS=(zed "")
            COMP_CWORD=1
            COMP_LINE="zed "
            COMP_POINT=${#COMP_LINE}
            _zed
            printf "%s\n" "${COMPREPLY[@]}" | grep -Fx install
          ' bash "$completion"
      - name: Zsh completion parses, registers, and autoloads
        shell: bash
        run: |
          set -euo pipefail
          bin="$GITHUB_WORKSPACE/zed-cli/target/debug/zed"
          dir="$RUNNER_TEMP/zfunc"
          mkdir -p "$dir"
          "$bin" completions zsh > "$dir/_zed"
          zsh -n "$dir/_zed"
          grep -F -- "--allow-no-manifest" "$dir/_zed"
          grep -F -- "--skip-manifest" "$dir/_zed"
          zsh -f -c '
            set -eu
            fpath=("$1" $fpath)
            autoload -Uz compinit
            compinit -i -D
            [[ ${_comps[zed]-} == _zed ]]
            autoload +X _zed
            whence -w _zed | grep -F function
          ' zsh "$dir"
'''
write(".github/workflows/ci.yml", ci)

print("DEN-567 product patch materialized")
