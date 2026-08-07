#!/usr/bin/env python3
"""Harden the generated DEN-564 product diff before validation."""

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


NO_MANIFEST_RS = r'''//! Explicit, fail-closed installation when no `.zpkg.toml` is present.
//!
//! The normal resolver and materializer remain authoritative. This module only
//! selects a safe install root, collects transient dependencies, obtains user
//! consent, and returns an in-memory consumer manifest to `ops::install`.

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

use crate::cli::Adapter;
use crate::config::{Config, read_manifest};
use crate::registry::registry_for;

const NATIVE_MARKERS: &[(&str, &str)] = &[
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

#[derive(Debug)]
pub struct PreparedInstall {
    pub project: PathBuf,
    pub manifest: Manifest,
    pub adapter: Adapter,
    pub target: Option<String>,
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
    specs: Vec<String>,
    source: String,
}

/// Prepare the exact in-memory manifest consumed by the normal installer.
/// Existing Zed manifests are never rewritten here; positional specs remain
/// transient. A missing manifest requires a reviewed plan and explicit consent.
#[allow(clippy::too_many_arguments)]
pub fn prepare_install(
    requested: &Path,
    cfg: &Config,
    frozen: bool,
    specs: &[String],
    allow_no_manifest: bool,
    requested_adapter: Adapter,
    requested_target: Option<&str>,
) -> Result<PreparedInstall> {
    if let Some(project) = find_existing_manifest(requested) {
        let mut manifest = read_manifest(&project)?;
        merge_specs(&mut manifest.dependencies, specs, cfg)?;
        return Ok(PreparedInstall {
            project,
            manifest,
            adapter: requested_adapter,
            target: requested_target.map(str::to_owned),
        });
    }

    let project = infer_install_root(requested);
    let requested = if specs.is_empty() {
        if !frozen {
            bail!(
                "no {MANIFEST_FILE} and no package specs were supplied; pass one or more `org/name[@requirement]` specs, or use `zed install --frozen --allow-no-manifest` with an existing {LOCKFILE_FILE}"
            );
        }
        RequestedDependencies::Locked(dependencies_from_lock(&project)?)
    } else {
        if frozen {
            bail!(
                "--frozen cannot be combined with package specs when no {MANIFEST_FILE} exists; install the specs first, then use --frozen for a locked reinstall"
            );
        }
        RequestedDependencies::Specs(parse_requested_specs(specs)?)
    };

    let target = requested_target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| detected_target(&project).map(str::to_owned));
    let inferred_adapter = infer_adapter(&project, target.as_deref(), requested_adapter);
    let effective_adapter = match requested_adapter {
        Adapter::Auto if inferred_adapter == Adapter::None => Adapter::Auto,
        Adapter::Auto => inferred_adapter,
        explicit => explicit,
    };
    let plan = ConsentPlan {
        root: fs::canonicalize(&project).unwrap_or_else(|_| project.clone()),
        target: target.clone(),
        adapter: inferred_adapter,
        specs: requested.display_specs(),
        source: requested.source().to_string(),
    };
    confirm_no_manifest(&plan, allow_no_manifest)?;

    // Registry metadata for an unversioned package is intentionally resolved
    // only after consent. Merely showing the prompt never performs network I/O
    // or writes dependency outputs.
    let dependencies = match requested {
        RequestedDependencies::Locked(dependencies) => dependencies,
        RequestedDependencies::Specs(specs) => resolve_requested_specs(&specs, cfg)?,
    };
    let manifest = synthetic_manifest(&project, dependencies);
    Ok(PreparedInstall {
        project,
        manifest,
        adapter: effective_adapter,
        target,
    })
}

fn find_existing_manifest(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(MANIFEST_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }

    let mut descendants = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == MANIFEST_FILE)
        .filter_map(|entry| entry.path().parent().map(Path::to_path_buf));
    let first = descendants.next()?;
    descendants.next().is_none().then_some(first)
}

/// Prefer the requested folder when it already identifies an ecosystem. A
/// repository shell with exactly one best nested native project selects that
/// project. Ambiguous monorepos remain at the requested root so Zed does not
/// silently choose between sibling applications.
fn infer_install_root(start: &Path) -> PathBuf {
    if project_score(start) > 0 {
        return start.to_path_buf();
    }

    let mut candidates: Vec<(usize, usize, PathBuf)> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(3)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir())
        .filter_map(|entry| {
            let score = project_score(entry.path());
            (score > 0).then(|| (entry.depth(), score, entry.path().to_path_buf()))
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let Some((best_depth, best_score, best_path)) = candidates.first().cloned() else {
        return start.to_path_buf();
    };
    let equally_good = candidates
        .iter()
        .filter(|(depth, score, _)| *depth == best_depth && *score == best_score)
        .count();
    if equally_good == 1 {
        best_path
    } else {
        start.to_path_buf()
    }
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

fn project_score(project: &Path) -> usize {
    NATIVE_MARKERS
        .iter()
        .chain(STRUCTURE_MARKERS.iter())
        .filter(|(marker, _)| project.join(marker).exists())
        .count()
}

fn detected_target(project: &Path) -> Option<&'static str> {
    NATIVE_MARKERS
        .iter()
        .chain(STRUCTURE_MARKERS.iter())
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| *target)
}

fn infer_adapter(project: &Path, target: Option<&str>, requested: Adapter) -> Adapter {
    match requested {
        Adapter::Auto => match target.or_else(|| detected_target(project)) {
            Some("node") => Adapter::Node,
            Some("java") => Adapter::Java,
            _ => Adapter::None,
        },
        explicit => explicit,
    }
}

fn adapter_label(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Auto => "auto",
        Adapter::None => "none (universal zed_modules; package-declared adapters still apply)",
        Adapter::Node => "node (also node_modules/@<org>/<name>)",
        Adapter::Java => "java (also .zed/classpath for installed jars)",
    }
}

fn split_spec(spec: &str) -> Result<(String, Option<String>)> {
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
    let mut parts = key.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(org), Some(name)) if !org.is_empty() && !name.is_empty() => {
            Ok((format!("{org}/{name}"), requirement))
        }
        _ => bail!("invalid package spec `{spec}` (expected org/name[@requirement])"),
    }
}

fn parse_requested_specs(specs: &[String]) -> Result<BTreeMap<String, Option<String>>> {
    let mut requested = BTreeMap::new();
    for spec in specs {
        let (key, requirement) = split_spec(spec)?;
        if let Some(previous) = requested.insert(key.clone(), requirement.clone())
            && previous != requirement
        {
            bail!(
                "package `{key}` was requested more than once with conflicting requirements `{}` and `{}`",
                previous.as_deref().unwrap_or("latest"),
                requirement.as_deref().unwrap_or("latest")
            );
        }
    }
    Ok(requested)
}

fn resolve_requested_specs(
    requested: &BTreeMap<String, Option<String>>,
    cfg: &Config,
) -> Result<BTreeMap<String, String>> {
    let registry = if requested.values().any(Option::is_none) {
        Some(registry_for(&cfg.registry)?)
    } else {
        None
    };
    let mut dependencies = BTreeMap::new();
    for (key, requirement) in requested {
        let requirement = match requirement {
            Some(requirement) => requirement.clone(),
            None => {
                let (org, name) = key
                    .split_once('/')
                    .expect("validated dependency key contains a slash");
                let package = registry
                    .as_ref()
                    .expect("registry exists for unversioned package")
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

fn merge_specs(
    dependencies: &mut BTreeMap<String, String>,
    specs: &[String],
    cfg: &Config,
) -> Result<()> {
    let requested = parse_requested_specs(specs)?;
    for (key, requirement) in resolve_requested_specs(&requested, cfg)? {
        if let Some(previous) = dependencies.get(&key)
            && previous != &requirement
        {
            bail!(
                "package `{key}` conflicts with the manifest requirement `{previous}` versus `{requirement}`"
            );
        }
        dependencies.insert(key, requirement);
    }
    Ok(())
}

fn dependencies_from_lock(project: &Path) -> Result<BTreeMap<String, String>> {
    let path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "--frozen manifestless install requires an existing {LOCKFILE_FILE} in {}",
            project.display()
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
        .map(|value| slug(&value.to_string_lossy()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unmanaged-project".to_string());
    Manifest {
        package: PackageSection {
            org: "zed-unmanaged".to_string(),
            name: name.clone(),
            version: "0.0.0".to_string(),
            version_scheme: VersionScheme::Semver,
            description: Some("Transient manifest-free Zed consumer".to_string()),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: format!("https://localhost/zed-unmanaged/{name}"),
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

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut previous_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            result.push(ch);
            previous_dash = false;
        } else if !previous_dash && !result.is_empty() {
            result.push('-');
            previous_dash = true;
        }
    }
    result.trim_matches('-').to_string()
}

fn confirm_no_manifest(plan: &ConsentPlan, allow_no_manifest: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_no_manifest_with(
        plan,
        stdin.is_terminal(),
        allow_no_manifest,
        &mut input,
        &mut output,
    )
}

fn confirm_no_manifest_with<R: BufRead, W: Write>(
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
    for spec in &plan.specs {
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
            "stdin is not interactive; no files were changed. Re-run with --allow-no-manifest or --skip-manifest after reviewing the plan"
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
            "installation cancelled; no files were changed. Re-run and answer `y`, or use --allow-no-manifest/--skip-manifest"
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use tempfile::tempdir;

    fn cfg(root: &Path) -> Config {
        Config {
            registry: format!("file://{}", root.join("registry").display()),
            home: root.join("home"),
            token: None,
            auth_url: "https://registry.example.test/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
        }
    }

    fn plan(root: &Path) -> ConsentPlan {
        ConsentPlan {
            root: root.to_path_buf(),
            target: Some("node".to_string()),
            adapter: Adapter::Node,
            specs: vec!["acme/tool@^1".to_string()],
            source: "the command line".to_string(),
        }
    }

    #[test]
    fn confirmation_accepts_only_explicit_yes_and_prints_the_plan() {
        for accepted in ["y\n", "YES\n", " yes \n"] {
            let mut output = Vec::new();
            confirm_no_manifest_with(
                &plan(Path::new("/tmp/project")),
                true,
                false,
                &mut Cursor::new(accepted.as_bytes()),
                &mut output,
            )
            .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("install root: /tmp/project"));
            assert!(rendered.contains("detected target: node"));
            assert!(rendered.contains("ecosystem adapter: node"));
            assert!(rendered.contains("acme/tool@^1"));
        }
        for rejected in ["\n", "n\n", "true\n", "maybe\n"] {
            assert!(
                confirm_no_manifest_with(
                    &plan(Path::new("/tmp/project")),
                    true,
                    false,
                    &mut Cursor::new(rejected.as_bytes()),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn eof_redirected_input_and_explicit_bypass_are_distinct() {
        let plan = plan(Path::new("/tmp/project"));
        let eof = confirm_no_manifest_with(
            &plan,
            true,
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(eof.contains("input closed"));

        let redirected = confirm_no_manifest_with(
            &plan,
            false,
            false,
            &mut Cursor::new(b"yes\n"),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(redirected.contains("not interactive"));

        confirm_no_manifest_with(
            &plan,
            false,
            true,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn one_nested_native_project_becomes_the_install_root() {
        let temp = tempdir().unwrap();
        let web = temp.path().join("apps/web");
        fs::create_dir_all(&web).unwrap();
        fs::write(web.join("package.json"), "{}").unwrap();
        assert_eq!(infer_install_root(temp.path()), web);
    }

    #[test]
    fn common_source_structure_can_select_a_nested_project() {
        let temp = tempdir().unwrap();
        let service = temp.path().join("services/worker");
        fs::create_dir_all(service.join("src")).unwrap();
        fs::write(service.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(infer_install_root(temp.path()), service);
        assert_eq!(detected_target(&service), Some("rust"));
    }

    #[test]
    fn ambiguous_nested_projects_keep_the_safe_universal_root() {
        let temp = tempdir().unwrap();
        for path in ["apps/web/package.json", "apps/api/Cargo.toml"] {
            let path = temp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        assert_eq!(infer_install_root(temp.path()), temp.path());
    }

    #[test]
    fn allow_no_manifest_builds_a_transient_manifest_without_writing_one() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        let prepared = prepare_install(
            temp.path(),
            &cfg(temp.path()),
            false,
            &["acme/tool@^1".to_string()],
            true,
            Adapter::Auto,
            None,
        )
        .unwrap();
        assert_eq!(prepared.project, temp.path());
        assert_eq!(prepared.manifest.dependencies["acme/tool"], "^1");
        assert!(!temp.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn manifest_free_install_requires_specs_or_an_explicit_frozen_lock() {
        let temp = tempdir().unwrap();
        let error = prepare_install(
            temp.path(),
            &cfg(temp.path()),
            false,
            &[],
            true,
            Adapter::Auto,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no package specs"));

        let frozen_specs = prepare_install(
            temp.path(),
            &cfg(temp.path()),
            true,
            &["acme/tool@^1".to_string()],
            true,
            Adapter::Auto,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(frozen_specs.contains("cannot be combined"));
    }
}
'''

write("src/no_manifest.rs", NO_MANIFEST_RS)

replace_once(
    "src/ops.rs",
    '''    let prepared =
        no_manifest::prepare_install(project, cfg, frozen, specs, allow_no_manifest)?;
    let store = Store::new(&cfg.home);''',
    '''    let prepared = no_manifest::prepare_install(
        project,
        cfg,
        frozen,
        specs,
        allow_no_manifest,
        adapter,
        target,
    )?;
    let store = Store::new(&cfg.home);''',
    "pass actual CLI layout inputs to the consent plan",
)
replace_once(
    "src/ops.rs",
    '''        adapter,
        allow_build,
        target,
        Some(&prepared.manifest),''',
    '''        prepared.adapter,
        allow_build,
        prepared.target.as_deref(),
        Some(&prepared.manifest),''',
    "run the installer with the exact reviewed layout",
)

PTY = r'''#!/usr/bin/env python3
"""Run a command under a real pseudo-terminal and answer Zed's prompt."""

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

    deadline = time.monotonic() + 45
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
        raise SystemExit("command exited before showing the consent prompt")
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    raise SystemExit(main())
'''
write("tests/manifestless_pty.py", PTY)

ci = read(".github/workflows/ci.yml")
marker = "      - name: Prepare manifest-free Node consumers\n"
if marker not in ci:
    raise RuntimeError("DEN-564 Docker section was not generated")
ci = ci[: ci.index(marker)] + r'''      - name: Prepare manifest-free Node consumers
        shell: bash
        run: |
          set -euo pipefail
          for name in interactive redirected reject eof allow alias frozen nested; do
            root="$RUNNER_TEMP/no-manifest-$name"
            mkdir -p "$root"
            cat >"$root/package.json" <<'JSON'
          {"name":"zed-no-manifest-consumer","private":true,"type":"commonjs"}
          JSON
            cat >"$root/app.js" <<'JS'
          const {containerGreeting} = require("@zed-pkg/docker-node-lib");
          if (containerGreeting() !== "hello from @zed-pkg/docker-node-lib") {
            throw new Error("unexpected installed package result");
          }
          console.log("manifest-free node install passed");
          JS
          done
          mkdir -p "$RUNNER_TEMP/no-manifest-nested/apps/web"
          mv "$RUNNER_TEMP/no-manifest-nested/package.json" "$RUNNER_TEMP/no-manifest-nested/apps/web/package.json"
          mv "$RUNNER_TEMP/no-manifest-nested/app.js" "$RUNNER_TEMP/no-manifest-nested/apps/web/app.js"

      - name: Redirected stdin fails closed even when it contains yes
        shell: bash
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/no-manifest-redirected"
          if printf 'yes\n' | docker run --rm -i \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --registry file:///registry --home /zed-home/redirected \
              --install-mode copy
          then
            echo 'redirected stdin unexpectedly bypassed the TTY consent boundary' >&2
            exit 1
          fi
          test ! -e "$root/.zpkg.toml"
          test ! -e "$root/.zpkg.lock"
          test ! -e "$root/zed_modules"

      - name: Interactive yes, no, and EOF use a real pseudo-terminal
        shell: bash
        run: |
          set -euo pipefail
          yes_root="$RUNNER_TEMP/no-manifest-interactive"
          python3 zed-cli/tests/manifestless_pty.py yes -- \
            docker run --rm -it \
              --volume "$yes_root:/work" \
              --volume "$RUNNER_TEMP/registry:/registry:ro" \
              --volume "$RUNNER_TEMP/zed-home:/zed-home" \
              --workdir /work \
              zed-pkg/install-test \
              zed install zed-pkg/docker-node-lib@^1 \
                --registry file:///registry --home /zed-home/interactive \
                --install-mode copy
          test ! -e "$yes_root/.zpkg.toml"
          test -f "$yes_root/.zpkg.lock"
          test -d "$yes_root/zed_modules/zed-pkg/docker-node-lib"
          test -d "$yes_root/node_modules/@zed-pkg/docker-node-lib"
          test -z "$(find "$yes_root/zed_modules" "$yes_root/node_modules" -type l -print -quit)"
          docker run --rm --volume "$yes_root:/work:ro" --workdir /work \
            node:22-bookworm-slim node app.js

          for mode in no eof; do
            case "$mode" in
              no) root="$RUNNER_TEMP/no-manifest-reject" ;;
              eof) root="$RUNNER_TEMP/no-manifest-eof" ;;
            esac
            if python3 zed-cli/tests/manifestless_pty.py "$mode" -- \
              docker run --rm -it \
                --volume "$root:/work" \
                --volume "$RUNNER_TEMP/registry:/registry:ro" \
                --volume "$RUNNER_TEMP/zed-home:/zed-home" \
                --workdir /work \
                zed-pkg/install-test \
                zed install zed-pkg/docker-node-lib@^1 \
                  --registry file:///registry --home "/zed-home/$mode" \
                  --install-mode copy
            then
              echo "$mode unexpectedly accepted manifest-free installation" >&2
              exit 1
            fi
            test ! -e "$root/.zpkg.toml"
            test ! -e "$root/.zpkg.lock"
            test ! -e "$root/zed_modules"
          done

      - name: Both automation spellings and explicit frozen reinstall work
        shell: bash
        run: |
          set -euo pipefail
          allow="$RUNNER_TEMP/no-manifest-allow"
          docker run --rm \
            --volume "$allow:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --allow-no-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/allow
          test ! -e "$allow/.zpkg.toml"
          test -f "$allow/.zpkg.lock"
          test -d "$allow/node_modules/@zed-pkg/docker-node-lib"

          alias="$RUNNER_TEMP/no-manifest-alias"
          docker run --rm \
            --volume "$alias:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --skip-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/alias
          test ! -e "$alias/.zpkg.toml"
          test -f "$alias/.zpkg.lock"

          frozen="$RUNNER_TEMP/no-manifest-frozen"
          docker run --rm \
            --volume "$frozen:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --skip-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/frozen
          if docker run --rm \
            --volume "$frozen:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install --skip-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/frozen
          then
            echo 'lock-only manifestless reinstall without --frozen unexpectedly succeeded' >&2
            exit 1
          fi
          rm -rf "$frozen/zed_modules" "$frozen/node_modules" "$frozen/.zed"
          docker run --rm \
            --volume "$frozen:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install --frozen --skip-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/frozen
          test ! -e "$frozen/.zpkg.toml"
          test -f "$frozen/.zpkg.lock"
          test -z "$(find "$frozen/zed_modules" "$frozen/node_modules" -type l -print -quit)"
          docker run --rm --volume "$frozen:/work:ro" --workdir /work \
            node:22-bookworm-slim node app.js

      - name: A single nested native app becomes the selected install root
        shell: bash
        run: |
          set -euo pipefail
          root="$RUNNER_TEMP/no-manifest-nested"
          docker run --rm \
            --volume "$root:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --skip-manifest --install-mode copy \
              --registry file:///registry --home /zed-home/nested
          test ! -e "$root/.zpkg.toml"
          test ! -e "$root/.zpkg.lock"
          test -f "$root/apps/web/.zpkg.lock"
          test -d "$root/apps/web/node_modules/@zed-pkg/docker-node-lib"
          docker run --rm --volume "$root:/work:ro" --workdir /work/apps/web \
            node:22-bookworm-slim node app.js
'''
write(".github/workflows/ci.yml", ci)

replace_once(
    "README.md",
    "console prints the chosen root, target, adapter, and transient dependencies,\nthen requires `y` or `yes`. EOF and every other answer cancel before files are\nwritten.",
    "console prints the chosen root, target, adapter, and transient dependencies,\nthen requires `y` or `yes` from a real terminal. Redirected stdin, EOF, and every\nother answer cancel before dependency outputs are written.",
    "document the TTY consent boundary",
)

print("DEN-564 hardening applied")
