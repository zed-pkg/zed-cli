#!/usr/bin/env python3
"""Apply the final DEN-567 safety and integration invariants.

Runs after `materialize-den-567.py` has produced the broad product diff. This
pass keeps confirmation ahead of registry/network resolution, adds bounded
folder-structure inference, exercises the interaction on a real PTY, and
removes surprising manifest-backed positional-install behavior.
"""

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
        raise RuntimeError(f"{path}: {label}: expected one target, found {count}")
    write(path, content.replace(old, new, 1))


MANIFESTLESS_RS = r'''//! Manifestless dependency installation.
//!
//! A missing `.zpkg.toml` is an explicit consent boundary, not a second
//! resolver. This module selects a conservative project root, parses a
//! transient install plan, obtains consent, and then delegates to the normal
//! installer through a scoped in-memory consumer manifest. Store locking,
//! integrity checks, target slicing, build consent, bin hoisting, and adapter
//! behavior therefore cannot drift from manifest-backed installation.

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
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
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

#[derive(Debug)]
enum RequestedDependencies {
    Locked(BTreeMap<String, String>),
    Specs(BTreeMap<String, Option<String>>),
}

impl RequestedDependencies {
    fn source(&self) -> &'static str {
        match self {
            Self::Locked(_) => LOCKFILE_FILE,
            Self::Specs(_) => "the command line",
        }
    }

    fn display_specs(&self) -> Vec<String> {
        match self {
            Self::Locked(dependencies) => dependencies
                .iter()
                .map(|(key, requirement)| format!("{key}@{requirement}"))
                .collect(),
            Self::Specs(specs) => specs
                .iter()
                .map(|(key, requirement)| match requirement {
                    Some(requirement) => format!("{key}@{requirement}"),
                    None => format!("{key} (latest compatible release)"),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
struct ConsentPlan {
    root: PathBuf,
    target: Option<String>,
    adapter: Adapter,
    package_specs: Vec<String>,
    source: String,
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    requested_root: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_no_manifest: bool,
) -> Result<ops::InstallOutcome> {
    let selection = select_project(requested_root);

    if selection.has_manifest {
        if !specs.is_empty() {
            bail!(
                "package operands on `zed install` are only supported when no {MANIFEST_FILE} exists; use `zed add <org>/<name>[@requirement]` to persist a dependency in this project"
            );
        }
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

    let requested = requested_dependencies(&selection.root, specs, frozen)?;
    let inferred_target = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| ops::detect_target(&selection.root));
    let inferred_adapter = match adapter {
        Adapter::Auto => ops::detect_adapter(&selection.root),
        explicit => explicit,
    };
    let plan = ConsentPlan {
        root: fs::canonicalize(&selection.root).unwrap_or_else(|_| selection.root.clone()),
        target: inferred_target.clone(),
        adapter: inferred_adapter,
        package_specs: requested.display_specs(),
        source: requested.source().to_string(),
    };

    // Consent intentionally precedes registry access. An unversioned package
    // can be shown as “latest compatible release” without contacting a server.
    confirm_manifestless(&plan, allow_no_manifest)?;
    let dependencies = match requested {
        RequestedDependencies::Locked(dependencies) => dependencies,
        RequestedDependencies::Specs(specs) => resolve_requested_specs(cfg, &specs)?,
    };

    let manifest = synthetic_manifest(&selection.root, dependencies);
    let manifest_text = manifest.to_toml_string()?;
    eprintln!(
        "manifestless install: {MANIFEST_FILE} will not be created; the normal installer will write {LOCKFILE_FILE} and the inferred dependency outputs"
    );
    config::with_manifest_override(&selection.root, manifest_text, || {
        ops::install(
            &selection.root,
            cfg,
            frozen,
            mode,
            adapter,
            allow_build,
            inferred_target.as_deref(),
        )
    })
}

fn requested_dependencies(
    project: &Path,
    specs: &[String],
    frozen: bool,
) -> Result<RequestedDependencies> {
    if specs.is_empty() {
        if !frozen {
            bail!(
                "no {MANIFEST_FILE} and no package specs were supplied; pass one or more `org/name[@requirement]` operands, or use `zed install --frozen --allow-no-manifest` with an existing {LOCKFILE_FILE}"
            );
        }
        return Ok(RequestedDependencies::Locked(dependencies_from_lock(
            project,
        )?));
    }
    if frozen {
        bail!(
            "--frozen cannot be combined with package specs when no {MANIFEST_FILE} exists; install the specs first, then use --frozen for a locked reinstall"
        );
    }
    Ok(RequestedDependencies::Specs(parse_requested_specs(specs)?))
}

fn parse_requested_specs(specs: &[String]) -> Result<BTreeMap<String, Option<String>>> {
    let mut requested = BTreeMap::new();
    for spec in specs {
        let (key, requirement) = split_dependency_spec(spec)?;
        if let Some(previous) = requested.insert(key.clone(), requirement.clone())
            && previous != requirement
        {
            bail!(
                "conflicting requirements for {key}: `{}` and `{}`",
                display_requirement(previous.as_deref()),
                display_requirement(requirement.as_deref())
            );
        }
    }
    Ok(requested)
}

fn resolve_requested_specs(
    cfg: &Config,
    specs: &BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let registry = if specs.values().any(Option::is_none) {
        Some(registry_for(&cfg.registry)?)
    } else {
        None
    };
    let mut dependencies = BTreeMap::new();
    for (key, explicit) in specs {
        let requirement = match explicit {
            Some(requirement) => requirement.clone(),
            None => {
                let (org, name) = key
                    .split_once('/')
                    .expect("validated dependency key contains a slash");
                let package = registry
                    .as_ref()
                    .expect("registry exists when an unversioned spec exists")
                    .get_package(org, name)?;
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
        dependencies.insert(key.clone(), requirement);
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
    let (org, name) = key
        .split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty())
        .with_context(|| format!("invalid package spec `{spec}` (expected org/name[@requirement])"))?;
    if name.contains('/') {
        bail!("invalid package spec `{spec}` (expected exactly org/name[@requirement])");
    }
    Ok((format!("{org}/{name}"), requirement))
}

fn display_requirement(requirement: Option<&str>) -> &str {
    requirement.unwrap_or("latest")
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
        .map(|package| {
            (
                format!("{}/{}", package.org, package.name),
                package.version.clone(),
            )
        })
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
/// root. Multiple candidates are a monorepo; staying at the requested root is
/// safer than silently choosing between sibling applications.
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

fn adapter_label(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Auto => "auto",
        Adapter::None => "none (universal zed_modules; package-declared adapters may still apply)",
        Adapter::Node => "node (also node_modules/@<org>/<name>)",
        Adapter::Java => "java (also .zed/classpath for installed jars)",
    }
}

fn confirm_manifestless(plan: &ConsentPlan, allow_no_manifest: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_manifestless_with(
        plan,
        stdin.is_terminal(),
        allow_no_manifest,
        &mut input,
        &mut output,
    )
}

fn confirm_manifestless_with<R: BufRead, W: Write>(
    plan: &ConsentPlan,
    interactive: bool,
    allow_no_manifest: bool,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    writeln!(output, "No {MANIFEST_FILE} was found.")?;
    writeln!(output, "  install root: {}", plan.root.display())?;
    writeln!(
        output,
        "  detected target: {}",
        plan.target.as_deref().unwrap_or("universal")
    )?;
    writeln!(output, "  ecosystem adapter: {}", adapter_label(plan.adapter))?;
    writeln!(
        output,
        "  universal dependency tree: {}",
        plan.root.join(MODULES_DIR).display()
    )?;
    writeln!(output, "  dependencies from {}:", plan.source)?;
    for spec in &plan.package_specs {
        writeln!(output, "    {spec}")?;
    }
    writeln!(
        output,
        "Zed will write {LOCKFILE_FILE} and dependency outputs but will not create {MANIFEST_FILE}."
    )?;

    if allow_no_manifest {
        writeln!(
            output,
            "Proceeding non-interactively because --allow-no-manifest/--skip-manifest was supplied."
        )?;
        return Ok(());
    }
    if !interactive {
        bail!(
            "stdin is not an interactive terminal; no files were changed. Re-run with --allow-no-manifest or --skip-manifest after reviewing the plan"
        );
    }

    write!(output, "Proceed without creating {MANIFEST_FILE}? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        bail!(
            "confirmation input closed before `y`/`yes`; no files were changed. Re-run with --allow-no-manifest or --skip-manifest after reviewing the plan"
        );
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!(
            "manifestless installation cancelled; no files were changed. Re-run and answer `y`, or use --allow-no-manifest/--skip-manifest"
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn plan(root: &Path) -> ConsentPlan {
        ConsentPlan {
            root: root.to_path_buf(),
            target: Some("node".to_string()),
            adapter: Adapter::Node,
            package_specs: vec!["acme/http-kit@^1".to_string()],
            source: "the command line".to_string(),
        }
    }

    #[test]
    fn dependency_specs_are_parsed_without_registry_access() {
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
        assert!(split_dependency_spec("a/b/c").is_err());
    }

    #[test]
    fn explicit_yes_is_required_and_the_complete_plan_is_printed() {
        for accepted in [b"y\n".as_slice(), b"YES\n".as_slice(), b" yes \n".as_slice()] {
            let mut output = Vec::new();
            confirm_manifestless_with(
                &plan(Path::new("/tmp/project")),
                true,
                false,
                &mut Cursor::new(accepted),
                &mut output,
            )
            .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("install root: /tmp/project"));
            assert!(rendered.contains("detected target: node"));
            assert!(rendered.contains("ecosystem adapter: node"));
            assert!(rendered.contains("acme/http-kit@^1"));
            assert!(rendered.contains("[y/N]"));
        }

        for rejected in [b"\n".as_slice(), b"n\n".as_slice(), b"maybe\n".as_slice()] {
            assert!(
                confirm_manifestless_with(
                    &plan(Path::new("/tmp/project")),
                    true,
                    false,
                    &mut Cursor::new(rejected),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn eof_and_redirected_input_fail_closed_but_explicit_bypass_does_not_read() {
        let plan = plan(Path::new("/tmp/project"));
        let eof = confirm_manifestless_with(
            &plan,
            true,
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(eof.contains("input closed"));

        let redirected = confirm_manifestless_with(
            &plan,
            false,
            false,
            &mut Cursor::new(b"yes\n"),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(redirected.contains("not an interactive terminal"));
        assert!(redirected.contains("--allow-no-manifest"));

        confirm_manifestless_with(
            &plan,
            false,
            true,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap();
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
    fn common_source_structure_can_select_a_nested_project() {
        let temp = tempfile::tempdir().unwrap();
        let worker = temp.path().join("services/worker");
        fs::create_dir_all(worker.join("src")).unwrap();
        fs::write(worker.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(select_project(temp.path()).root, worker);
        assert_eq!(ops::detect_target(&worker).as_deref(), Some("rust"));
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
    fn no_specs_require_an_explicit_frozen_lock_and_frozen_rejects_specs() {
        let temp = tempfile::tempdir().unwrap();
        let missing = requested_dependencies(temp.path(), &[], false)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("no package specs"));
        let incompatible = requested_dependencies(
            temp.path(),
            &["acme/http-kit@^1".to_string()],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(incompatible.contains("cannot be combined"));
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

write("src/manifestless.rs", MANIFESTLESS_RS)

replace_once(
    "src/ops.rs",
    '''pub(crate) fn detect_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("pubspec.yaml", "dart"),
        ("mix.exs", "elixir"),
        ("rebar.config", "erlang"),
        ("gleam.toml", "gleam"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("CMakeLists.txt", "cpp"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Pick the ecosystem adapter from what the project looks like: Node
/// projects resolve from node_modules/, JVM projects need a classpath,
/// Rust/others use zed_modules/ directly.
pub(crate) fn detect_adapter(project: &Path) -> Adapter {
    if project.join("package.json").exists() {
        Adapter::Node
    } else if project.join("pom.xml").exists()
        || project.join("build.gradle").exists()
        || project.join("build.gradle.kts").exists()
    {
        Adapter::Java
    } else {
        Adapter::None
    }
}''',
    '''pub(crate) fn detect_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
        ("tsconfig.json", "node"),
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("pubspec.yaml", "dart"),
        ("mix.exs", "elixir"),
        ("rebar.config", "erlang"),
        ("gleam.toml", "gleam"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("CMakeLists.txt", "cpp"),
    ];
    if let Some((_, target)) = MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
    {
        return Some((*target).to_string());
    }

    // Some consumer folders are intentionally pre-manifest (for example,
    // generated app skeletons). Keep this bounded and shallow so Zed never
    // recursively classifies an unrelated large checkout.
    const STRUCTURE_MARKERS: &[(&str, &str)] = &[
        ("src/main.rs", "rust"),
        ("src/lib.rs", "rust"),
        ("src/index.ts", "node"),
        ("src/main.ts", "node"),
        ("src/index.js", "node"),
        ("src/main.js", "node"),
        ("main.go", "go"),
        ("cmd/main.go", "go"),
        ("main.py", "python"),
        ("app.py", "python"),
        ("src/main.py", "python"),
        ("lib/main.dart", "dart"),
        ("src/main.gleam", "gleam"),
        ("src/main/java", "java"),
        ("src/main/kotlin", "java"),
    ];
    STRUCTURE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Pick the ecosystem adapter from the same language inference used for target
/// slicing so marker-only and structure-only consumer folders agree.
pub(crate) fn detect_adapter(project: &Path) -> Adapter {
    match detect_target(project).as_deref() {
        Some("node") => Adapter::Node,
        Some("java") => Adapter::Java,
        _ => Adapter::None,
    }
}''',
    "extend bounded language and adapter inference",
)

replace_once(
    "README.md",
    '''outputs. `zed install --frozen --skip-manifest` can reconstruct a no-manifest
install from an existing lockfile without package operands. Positional specs in
a project that already has a manifest are transient; use `zed add` to persist
them.''',
    '''outputs. `zed install --frozen --skip-manifest` can reconstruct a no-manifest
install from an existing lockfile without package operands. In a project that
already has a manifest, use `zed add` to persist a dependency; positional
package operands on `zed install` are rejected rather than silently creating a
non-persistent manifest override.''',
    "document the manifest-backed positional boundary",
)

PTY_HELPER = r'''#!/usr/bin/env python3
"""Run a command under a real PTY and answer Zed's confirmation prompt."""

import os
import select
import signal
import sys
import time


def main() -> int:
    if len(sys.argv) < 4 or "--" not in sys.argv:
        raise SystemExit("usage: manifestless_pty.py <yes|no|eof> -- <command> [args...]")
    mode = sys.argv[1]
    if mode not in {"yes", "no", "eof"}:
        raise SystemExit(f"unsupported mode: {mode}")
    split = sys.argv.index("--")
    command = sys.argv[split + 1 :]
    if not command:
        raise SystemExit("missing command")

    pid, fd = os.forkpty()
    if pid == 0:
        os.execvp(command[0], command)

    deadline = time.monotonic() + 60
    output = bytearray()
    answered = False
    status = None
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    chunk = b""
                if chunk:
                    output.extend(chunk)
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                    if not answered and b"[y/N]" in output:
                        os.write(fd, {"yes": b"yes\n", "no": b"no\n", "eof": b"\x04"}[mode])
                        answered = True
            waited, raw = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = raw
                break
        if status is None:
            os.kill(pid, signal.SIGKILL)
            _, status = os.waitpid(pid, 0)
            raise SystemExit("pseudo-terminal command timed out")
    finally:
        try:
            os.close(fd)
        except OSError:
            pass

    if not answered:
        raise SystemExit("command exited before showing the manifestless consent prompt")
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    raise SystemExit(main())
'''
write("tests/manifestless_pty.py", PTY_HELPER)

replace_once(
    ".github/workflows/ci.yml",
    '''          for name in interactive reject allow alias frozen; do''',
    '''          for name in interactive redirected negative eof no-network allow alias frozen; do''',
    "prepare all manifestless interaction fixtures",
)
replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Manifestless install fails closed without a terminal or bypass
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
''',
    '''      - name: Redirected stdin fails closed even when it contains yes
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-redirected"
          if printf 'yes\n' | docker run --rm -i \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --registry file:///registry \
              --home /zed-home/redirected \
              --install-mode copy
          then
            echo "redirected stdin unexpectedly bypassed the TTY boundary" >&2
            exit 1
          fi
          test ! -e "$root/.zpkg.toml"
          test ! -e "$root/.zpkg.lock"
          test ! -e "$root/zed_modules"

      - name: Interactive yes, no, and EOF use a real pseudo-terminal
        run: |
          set -euo pipefail
          yes_root="$RUNNER_TEMP/manifestless-interactive"
          python3 zed-cli/tests/manifestless_pty.py yes -- \
            docker run --rm -it \
              --volume "$yes_root:/work" \
              --volume "$RUNNER_TEMP/registry:/registry:ro" \
              --volume "$RUNNER_TEMP/zed-home:/zed-home" \
              --workdir /work \
              zed-pkg/install-test \
              zed install zed-pkg/docker-node-lib@^1 \
                --registry file:///registry \
                --home /zed-home/interactive \
                --install-mode copy
          test ! -e "$yes_root/.zpkg.toml"
          test -f "$yes_root/.zpkg.lock"
          test -d "$yes_root/zed_modules/zed-pkg/docker-node-lib"
          test -d "$yes_root/node_modules/@zed-pkg/docker-node-lib"
          test -z "$(find "$yes_root/zed_modules" "$yes_root/node_modules" -type l -print -quit)"
          docker run --rm --volume "$yes_root:/work:ro" --workdir /work \
            node:22-bookworm-slim node src/main.js

          for mode in no eof; do
            case "$mode" in
              no) root="$RUNNER_TEMP/manifestless-negative" ;;
              eof) root="$RUNNER_TEMP/manifestless-eof" ;;
            esac
            if python3 zed-cli/tests/manifestless_pty.py "$mode" -- \
              docker run --rm -it \
                --volume "$root:/work" \
                --volume "$RUNNER_TEMP/registry:/registry:ro" \
                --volume "$RUNNER_TEMP/zed-home:/zed-home" \
                --workdir /work \
                zed-pkg/install-test \
                zed install zed-pkg/docker-node-lib@^1 \
                  --registry file:///registry \
                  --home "/zed-home/$mode" \
                  --install-mode copy
            then
              echo "$mode unexpectedly accepted manifestless installation" >&2
              exit 1
            fi
            test ! -e "$root/.zpkg.toml"
            test ! -e "$root/.zpkg.lock"
            test ! -e "$root/zed_modules"
          done

      - name: Consent is requested before unversioned registry resolution
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/manifestless-no-network"
          if python3 zed-cli/tests/manifestless_pty.py no -- \
            docker run --rm -it \
              --volume "$root:/work" \
              --workdir /work \
              zed-pkg/install-test \
              zed install zed-pkg/docker-node-lib \
                --registry http://127.0.0.1:9 \
                --home /tmp/zed-home \
                --install-mode copy
          then
            echo "negative confirmation unexpectedly installed" >&2
            exit 1
          fi
          test ! -e "$root/.zpkg.toml"
          test ! -e "$root/.zpkg.lock"
          test ! -e "$root/zed_modules"
''',
    "exercise real TTY and pre-resolution consent boundaries",
)
replace_once(
    ".github/workflows/ci.yml",
    '''          rm -rf "$root/zed_modules" "$root/node_modules" "$root/.zed"
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
            ' ''',
    '''          if docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install --skip-manifest \
              --registry file:///registry \
              --home /zed-home/frozen \
              --install-mode copy
          then
            echo "lock-only manifestless reinstall without --frozen unexpectedly succeeded" >&2
            exit 1
          fi
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
              test -z "$(find zed_modules node_modules -type l -print -quit)"
              node src/main.js
            ' ''',
    "require explicit frozen mode for lock-only reinstall",
)

print("DEN-567 safety hardening applied")
