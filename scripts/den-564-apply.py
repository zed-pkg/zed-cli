#!/usr/bin/env python3
"""Apply the DEN-564 implementation on its feature branch.

This temporary helper exists only because the connected GitHub API exposes
whole-file writes rather than unified-patch writes.  The branch workflow runs
it once, validates the result, commits the real source changes, and the helper
is removed before review.
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


def replace_once(path: str, old: str, new: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    write(path, content.replace(old, new, 1))


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    content = read(path)
    count = content.count(old)
    if count != expected:
        raise RuntimeError(
            f"{path}: expected {expected} replacement targets, found {count}"
        )
    write(path, content.replace(old, new))


NO_MANIFEST_RS = r'''use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::{DirEntry, WalkDir};
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, VersionScheme};

use crate::config::{Config, read_manifest};
use crate::registry::registry_for;

const NATIVE_MARKERS: &[(&str, &str)] = &[
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

#[derive(Debug)]
pub struct PreparedInstall {
    pub project: PathBuf,
    pub manifest: Manifest,
}

/// Prepare the one manifest consumed by the normal installer. Existing
/// manifests are cloned in memory so positional specs remain transient. When
/// there is no manifest, this creates an in-memory consumer manifest only; it
/// is never written into the checkout.
pub fn prepare_install(
    requested: &Path,
    cfg: &Config,
    frozen: bool,
    specs: &[String],
    allow_no_manifest: bool,
) -> Result<PreparedInstall> {
    if let Some(project) = find_existing_manifest(requested) {
        let mut manifest = read_manifest(&project)?;
        merge_specs(&mut manifest.dependencies, specs, cfg)?;
        return Ok(PreparedInstall { project, manifest });
    }

    let project = infer_install_root(requested);
    let lock_exists = project.join(LOCKFILE_FILE).is_file();
    if specs.is_empty() && !(frozen && lock_exists) {
        bail!(
            "no {MANIFEST_FILE} was found and no package specs were supplied; pass one or more \
             `org/name[@requirement]` specs, or use --frozen with an existing {LOCKFILE_FILE}"
        );
    }

    let mut dependencies = BTreeMap::new();
    merge_specs(&mut dependencies, specs, cfg)?;
    let manifest = synthetic_manifest(&project, dependencies);
    let target = detected_target(&project).unwrap_or("universal");
    let adapter = detected_adapter(&project);

    if allow_no_manifest {
        eprintln!(
            "warning: no {MANIFEST_FILE}; installing without a manifest into {} \
             (target={target}, adapter={adapter})",
            project.display()
        );
    } else {
        let stdin = std::io::stdin();
        let stderr = std::io::stderr();
        confirm_no_manifest(
            &project,
            target,
            adapter,
            &manifest.dependencies,
            frozen && lock_exists,
            &mut stdin.lock(),
            &mut stderr.lock(),
        )?;
    }

    Ok(PreparedInstall { project, manifest })
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
    if descendants.next().is_none() {
        Some(first)
    } else {
        None
    }
}

/// Prefer the requested folder when it already identifies an ecosystem. If it
/// is a repository shell with exactly one clear native project below it, use
/// that project. Ambiguous monorepos stay at the requested root, where the
/// universal zed_modules layout is safe and does not guess between apps.
fn infer_install_root(start: &Path) -> PathBuf {
    if marker_score(start) > 0 {
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
            let score = marker_score(entry.path());
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

fn marker_score(project: &Path) -> usize {
    NATIVE_MARKERS
        .iter()
        .filter(|(marker, _)| project.join(marker).is_file())
        .count()
}

fn detected_target(project: &Path) -> Option<&'static str> {
    NATIVE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).is_file())
        .map(|(_, target)| *target)
}

fn detected_adapter(project: &Path) -> &'static str {
    if project.join("package.json").is_file() {
        "node"
    } else if ["pom.xml", "build.gradle", "build.gradle.kts"]
        .iter()
        .any(|marker| project.join(marker).is_file())
    {
        "java"
    } else {
        "none"
    }
}

fn split_spec(spec: &str) -> Result<(String, String, Option<String>)> {
    let (key, requirement) = match spec.rsplit_once('@') {
        Some((key, requirement)) if key.contains('/') => {
            if requirement.trim().is_empty() {
                bail!("empty requirement in package spec `{spec}`");
            }
            (key, Some(requirement.to_string()))
        }
        _ => (spec, None),
    };
    let mut parts = key.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(org), Some(name)) if !org.is_empty() && !name.is_empty() => {
            Ok((org.to_string(), name.to_string(), requirement))
        }
        _ => bail!("invalid package spec `{spec}` (expected org/name[@requirement])"),
    }
}

fn merge_specs(
    dependencies: &mut BTreeMap<String, String>,
    specs: &[String],
    cfg: &Config,
) -> Result<()> {
    for spec in specs {
        let (org, name, requirement) = split_spec(spec)?;
        let key = format!("{org}/{name}");
        let requirement = match requirement {
            Some(requirement) => requirement,
            None => {
                let registry = registry_for(&cfg.registry)?;
                let package = registry.get_package(&org, &name)?;
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
        if let Some(previous) = dependencies.get(&key)
            && previous != &requirement
        {
            bail!(
                "package `{key}` was requested more than once with conflicting requirements \
                 `{previous}` and `{requirement}`"
            );
        }
        dependencies.insert(key, requirement);
    }
    Ok(())
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

#[allow(clippy::too_many_arguments)]
fn confirm_no_manifest(
    project: &Path,
    target: &str,
    adapter: &str,
    dependencies: &BTreeMap<String, String>,
    lock_only: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<()> {
    writeln!(output, "No {MANIFEST_FILE} was found.")?;
    writeln!(output, "  install root: {}", project.display())?;
    writeln!(output, "  detected target: {target}")?;
    writeln!(output, "  ecosystem adapter: {adapter}")?;
    if lock_only {
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
            "confirmation input closed; installation cancelled (pass --allow-no-manifest or \
             --skip-manifest for intentional non-interactive use)"
        );
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!(
            "installation cancelled (pass --allow-no-manifest or --skip-manifest for intentional \
             non-interactive use)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cfg(root: &Path) -> Config {
        Config {
            registry: format!("file://{}", root.join("registry").display()),
            home: root.join("home"),
            token: None,
            auth_url: None,
            supabase_url: None,
            supabase_key: None,
        }
    }

    #[test]
    fn confirmation_accepts_only_explicit_yes() {
        let deps = BTreeMap::from([("acme/tool".to_string(), "^1".to_string())]);
        for accepted in ["y\n", "YES\n", " yes \n"] {
            let mut input = accepted.as_bytes();
            let mut output = Vec::new();
            confirm_no_manifest(
                Path::new("/tmp/project"),
                "rust",
                "none",
                &deps,
                false,
                &mut input,
                &mut output,
            )
            .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("acme/tool@^1"));
            assert!(rendered.contains("[y/N]"));
        }
        for rejected in ["\n", "n\n", "true\n", "maybe\n"] {
            let mut input = rejected.as_bytes();
            let mut output = Vec::new();
            assert!(
                confirm_no_manifest(
                    Path::new("/tmp/project"),
                    "rust",
                    "none",
                    &deps,
                    false,
                    &mut input,
                    &mut output,
                )
                .is_err()
            );
        }
        let mut input = "".as_bytes();
        let mut output = Vec::new();
        let error = confirm_no_manifest(
            Path::new("/tmp/project"),
            "rust",
            "none",
            &deps,
            false,
            &mut input,
            &mut output,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("input closed"));
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
        )
        .unwrap();
        assert_eq!(prepared.project, temp.path());
        assert_eq!(prepared.manifest.dependencies["acme/tool"], "^1");
        assert!(!temp.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn manifest_free_install_requires_specs_or_a_frozen_lock() {
        let temp = tempdir().unwrap();
        let error = prepare_install(temp.path(), &cfg(temp.path()), false, &[], true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no package specs"));
        fs::write(temp.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();
        let prepared = prepare_install(temp.path(), &cfg(temp.path()), true, &[], true).unwrap();
        assert!(prepared.manifest.dependencies.is_empty());
    }

    #[test]
    fn slug_is_stable_and_safe() {
        assert_eq!(slug("My App__Server"), "my-app-server");
        assert_eq!(slug("---"), "");
    }
}
'''

write("src/no_manifest.rs", NO_MANIFEST_RS)

replace_once(
    "src/lib.rs",
    "pub mod flags;\npub mod ops;",
    "pub mod flags;\npub mod no_manifest;\npub mod ops;",
)

replace_once(
    "src/cli.rs",
    '''    /// Resolve and install dependencies into zed_modules/\n    #[command(alias = "i")]\n    Install {\n        /// Install exactly what .zpkg.lock pins; fail on any drift\n        #[arg(long, env = "ZED_PKG_FROZEN")]\n        frozen: bool,\n        #[arg(\n            long,\n            value_enum,\n            env = "ZED_PKG_INSTALL_MODE",\n            default_value = "symlink"\n        )]\n        install_mode: InstallMode,\n        /// Also link packages where the language ecosystem expects them,\n        /// inferred from the project by default (experimental; python\n        /// site-packages and deeper maven integration are planned)\n        #[arg(long, value_enum, env = "ZED_PKG_ADAPTER", default_value = "auto")]\n        adapter: Adapter,\n        /// Run dependencies' [build] commands (arbitrary code from the\n        /// package author — off by default; builds are cached per\n        /// (artifact, platform, command) under ~/.zed-pkg/builds)\n        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]\n        allow_build: bool,\n        /// Which language subtree to take from polyglot dependencies (a repo\n        /// shipping e.g. node/, python/, go/). Overrides [install].target;\n        /// omitted = infer from the project\n        #[arg(long, env = "ZED_PKG_TARGET")]\n        target: Option<String>,\n    },''',
    '''    /// Resolve and install dependencies into zed_modules/\n    #[command(alias = "i")]\n    Install {\n        /// Transient packages to install without editing .zpkg.toml. Required\n        /// when no manifest exists unless --frozen can use an existing lock.\n        #[arg(value_name = "PACKAGE")]\n        specs: Vec<String>,\n        /// Install exactly what .zpkg.lock pins; fail on any drift\n        #[arg(long, env = "ZED_PKG_FROZEN")]\n        frozen: bool,\n        #[arg(\n            long,\n            value_enum,\n            env = "ZED_PKG_INSTALL_MODE",\n            default_value = "symlink"\n        )]\n        install_mode: InstallMode,\n        /// Also link packages where the language ecosystem expects them,\n        /// inferred from the project by default (experimental; python\n        /// site-packages and deeper maven integration are planned)\n        #[arg(long, value_enum, env = "ZED_PKG_ADAPTER", default_value = "auto")]\n        adapter: Adapter,\n        /// Run dependencies' [build] commands (arbitrary code from the\n        /// package author — off by default; builds are cached per\n        /// (artifact, platform, command) under ~/.zed-pkg/builds)\n        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]\n        allow_build: bool,\n        /// Proceed without prompting when no .zpkg.toml can be found.\n        #[arg(\n            long,\n            visible_alias = "skip-manifest",\n            env = "ZED_PKG_ALLOW_NO_MANIFEST"\n        )]\n        allow_no_manifest: bool,\n        /// Which language subtree to take from polyglot dependencies (a repo\n        /// shipping e.g. node/, python/, go/). Overrides [install].target;\n        /// omitted = infer from the project\n        #[arg(long, env = "ZED_PKG_TARGET")]\n        target: Option<String>,\n    },''',
)

replace_once(
    "src/cli.rs",
    '''    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):\n''',
    '''    #[test]\n    fn install_accepts_transient_specs_and_both_no_manifest_spellings() {\n        for flag in ["--allow-no-manifest", "--skip-manifest"] {\n            let cli = Cli::try_parse_from([\n                "zed",\n                "install",\n                "acme/tool@^1",\n                "acme/other@2",\n                flag,\n            ])\n            .unwrap();\n            match cli.cmd {\n                Cmd::Install {\n                    specs,\n                    allow_no_manifest,\n                    ..\n                } => {\n                    assert_eq!(specs, ["acme/tool@^1", "acme/other@2"]);\n                    assert!(allow_no_manifest);\n                }\n                other => panic!("unexpected command: {other:?}"),\n            }\n        }\n    }\n\n    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):\n''',
)

replace_once(
    "src/main.rs",
    '''        Cmd::Install {\n            frozen,\n            install_mode,\n            adapter,\n            allow_build,\n            target,\n        } => ops::install(\n            &cwd,\n            &cfg,\n            frozen,\n            install_mode,\n            adapter,\n            allow_build,\n            target.as_deref(),\n        )\n''',
    '''        Cmd::Install {\n            specs,\n            frozen,\n            install_mode,\n            adapter,\n            allow_build,\n            allow_no_manifest,\n            target,\n        } => ops::install(\n            &cwd,\n            &cfg,\n            frozen,\n            install_mode,\n            adapter,\n            allow_build,\n            target.as_deref(),\n            &specs,\n            allow_no_manifest,\n        )\n''',
)

replace_once(
    "src/ops.rs",
    "use crate::config::{Config, Credentials, read_manifest, write_manifest};\nuse crate::pack::{self, PackResult};",
    "use crate::config::{Config, Credentials, read_manifest, write_manifest};\nuse crate::no_manifest;\nuse crate::pack::{self, PackResult};",
)

replace_once(
    "src/ops.rs",
    '''#[allow(clippy::too_many_arguments)]\npub fn install(\n    project: &Path,\n    cfg: &Config,\n    frozen: bool,\n    mode: InstallMode,\n    adapter: Adapter,\n    allow_build: bool,\n    target: Option<&str>,\n) -> Result<InstallOutcome> {\n    let store = Store::new(&cfg.home);\n    // Serialize against concurrent `zed install` processes (other terminals,\n    // parallel CI runners) writing the store, refs.json, and lockfile.\n    let _install_lock = store.install_lock()?;\n    install_locked(\n        project,\n        cfg,\n        &store,\n        frozen,\n        mode,\n        adapter,\n        allow_build,\n        target,\n    )\n}\n''',
    '''#[allow(clippy::too_many_arguments)]\npub fn install(\n    project: &Path,\n    cfg: &Config,\n    frozen: bool,\n    mode: InstallMode,\n    adapter: Adapter,\n    allow_build: bool,\n    target: Option<&str>,\n    specs: &[String],\n    allow_no_manifest: bool,\n) -> Result<InstallOutcome> {\n    let prepared =\n        no_manifest::prepare_install(project, cfg, frozen, specs, allow_no_manifest)?;\n    let store = Store::new(&cfg.home);\n    // Serialize against concurrent `zed install` processes (other terminals,\n    // parallel CI runners) writing the store, refs.json, and lockfile.\n    let _install_lock = store.install_lock()?;\n    install_locked(\n        &prepared.project,\n        cfg,\n        &store,\n        frozen,\n        mode,\n        adapter,\n        allow_build,\n        target,\n        Some(&prepared.manifest),\n    )\n}\n''',
)

replace_once(
    "src/ops.rs",
    '''fn install_locked(\n    project: &Path,\n    cfg: &Config,\n    store: &Store,\n    frozen: bool,\n    mode: InstallMode,\n    adapter: Adapter,\n    allow_build: bool,\n    target: Option<&str>,\n) -> Result<InstallOutcome> {\n    let manifest = read_manifest(project)?;\n''',
    '''fn install_locked(\n    project: &Path,\n    cfg: &Config,\n    store: &Store,\n    frozen: bool,\n    mode: InstallMode,\n    adapter: Adapter,\n    allow_build: bool,\n    target: Option<&str>,\n    manifest_override: Option<&Manifest>,\n) -> Result<InstallOutcome> {\n    let manifest = match manifest_override {\n        Some(manifest) => manifest.clone(),\n        None => read_manifest(project)?,\n    };\n''',
)

replace_once(
    "src/ops.rs",
    '''            // Build dependencies are toolchain, not the consumer's language:\n            // take them whole rather than slicing them to a target.\n            None,\n        )?;\n''',
    '''            // Build dependencies are toolchain, not the consumer's language:\n            // take them whole rather than slicing them to a target.\n            None,\n            None,\n        )?;\n''',
)

replace_count(
    "src/ops.rs",
    '''        // Re-install after the manifest edit; the target comes from\n        // [install].target or project inference, same as a bare `zed install`.\n        None,\n    )?;\n''',
    '''        // Re-install after the manifest edit; the target comes from\n        // [install].target or project inference, same as a bare `zed install`.\n        None,\n        &[],\n        false,\n    )?;\n''',
    2,
)

replace_once(
    "src/r2g.rs",
    '''        Adapter::None,\n        true,\n        None,\n    )?;\n''',
    '''        Adapter::None,\n        true,\n        None,\n        &[],\n        false,\n    )?;\n''',
)

replace_once(
    ".cli-flags.toml",
    '''[flags.target]\nenv = "ZED_PKG_TARGET"\naliases = ["target"]\ntype = "string"\nhelp = "Polyglot package target."\n''',
    '''[flags.target]\nenv = "ZED_PKG_TARGET"\naliases = ["target"]\ntype = "string"\nhelp = "Polyglot package target."\n\n[flags.allow_no_manifest]\nenv = "ZED_PKG_ALLOW_NO_MANIFEST"\naliases = ["allow-no-manifest", "skip-manifest"]\ntype = "bool"\ndefault = "false"\nhelp = "Proceed non-interactively when no .zpkg.toml is present."\n''',
)

replace_once(
    "README.md",
    '''# consume packages\nzed add acme/http-kit@^1\nzed install\nzed find http\n''',
    '''# consume packages from a manifest\nzed add acme/http-kit@^1\nzed install\n\n# or install transiently in a folder with no .zpkg.toml\nzed install acme/http-kit@^1       # confirms interactively\nzed install acme/http-kit@^1 --skip-manifest  # intentional automation\nzed find http\n''',
)

replace_once(
    "README.md",
    '''Every package is `<org>/<name>`, declared in a `.zpkg.toml` manifest at the\nrepo root (TOML only). See `zed init` output for the annotated template.\n''',
    '''Every authored package is `<org>/<name>`, declared in a `.zpkg.toml` manifest\nat the repo root (TOML only). Consumers may install positional package specs\nwithout a manifest; Zed asks for confirmation and keeps that manifest transient.\nSee `zed init` output for the annotated authoring template.\n''',
)

replace_once(
    "README.md",
    '''| `zed install` (`zed i`) | Resolve, download once into the store, symlink into `zed_modules/` |\n| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers) |\n''',
    '''| `zed install [<org>/<name>[@req] ...]` (`zed i`) | Resolve, download once into the store, and install manifest or transient dependencies |\n| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers; also works without a manifest) |\n''',
)

replace_once(
    "README.md",
    '''### Where dependencies land (`[install].dir`)\n''',
    '''### Installing without a Zed manifest\n\n`zed install` accepts transient package specs even when the current repository\nor folder has no `.zpkg.toml`:\n\n```sh\nzed install oresoftware/flags-2-env@^0.1\n```\n\nZed searches for a nearby Zed manifest first. When none exists, it inspects\nnative manifests such as `package.json`, `Cargo.toml`, `go.mod`, and\n`pyproject.toml`. A folder that contains one clear nested app (for example\n`apps/web/package.json`) becomes the install root; ambiguous monorepos stay at\nthe requested root and use the safe universal `zed_modules/` layout. The\nconsole prints the chosen root, target, adapter, and transient dependencies,\nthen requires `y` or `yes`. EOF and every other answer cancel before files are\nwritten.\n\nAutomation must opt in explicitly with `--allow-no-manifest` or its visible\nalias `--skip-manifest` (`ZED_PKG_ALLOW_NO_MANIFEST=1`). Zed never creates a\nsynthetic `.zpkg.toml`; it still writes `.zpkg.lock`, `zed_modules/`, hoisted\nbinaries, and language adapter outputs. With an existing lockfile,\n`zed install --frozen --skip-manifest` needs no package specs.\n\n### Where dependencies land (`[install].dir`)\n''',
)

replace_once(
    "README.md",
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |\n| `--force` (build) | `ZED_PKG_FORCE` | off |\n''',
    '''| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |\n| `--allow-no-manifest` / `--skip-manifest` (install) | `ZED_PKG_ALLOW_NO_MANIFEST` | off; otherwise confirm interactively |\n| `--force` (build) | `ZED_PKG_FORCE` | off |\n''',
)

ci = read(".github/workflows/ci.yml")
marker = "      - name: Manifest-free installs require consent and infer placement\n"
if marker in ci:
    raise RuntimeError(".github/workflows/ci.yml: DEN-564 steps already present")
ci += r'''

      - name: Prepare manifest-free Node consumers
        shell: bash
        run: |
          set -euo pipefail
          for name in interactive allow alias reject eof empty frozen; do
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
          nested="$RUNNER_TEMP/no-manifest-nested/apps/web"
          mkdir -p "$nested"
          cp "$RUNNER_TEMP/no-manifest-interactive/package.json" "$nested/package.json"
          cp "$RUNNER_TEMP/no-manifest-interactive/app.js" "$nested/app.js"

      - name: Manifest-free installs require consent and infer placement
        shell: bash
        run: |
          set -euo pipefail
          common=(
            --volume "$RUNNER_TEMP/registry:/registry:ro"
            --volume "$RUNNER_TEMP/zed-home:/zed-home"
            zed-pkg/install-test
          )

          interactive="$RUNNER_TEMP/no-manifest-interactive"
          printf 'y\n' | docker run --rm -i \
            --volume "$interactive:/work" \
            --workdir /work \
            "${common[@]}" \
            sh -euc '
              zed install zed-pkg/docker-node-lib \
                --registry file:///registry \
                --home /zed-home/interactive \
                --install-mode copy
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              test -d zed_modules/zed-pkg/docker-node-lib
              test -d node_modules/@zed-pkg/docker-node-lib
              node app.js
            '

          rejected="$RUNNER_TEMP/no-manifest-reject"
          if printf 'n\n' | docker run --rm -i \
            --volume "$rejected:/work" \
            --workdir /work \
            "${common[@]}" \
            zed install zed-pkg/docker-node-lib \
              --registry file:///registry \
              --home /zed-home/reject \
              --install-mode copy
          then
            echo 'negative confirmation unexpectedly installed' >&2
            exit 1
          fi
          test ! -e "$rejected/.zpkg.lock"
          test ! -e "$rejected/zed_modules"

          eof="$RUNNER_TEMP/no-manifest-eof"
          if docker run --rm \
            --volume "$eof:/work" \
            --workdir /work \
            "${common[@]}" \
            zed install zed-pkg/docker-node-lib \
              --registry file:///registry \
              --home /zed-home/eof \
              --install-mode copy </dev/null
          then
            echo 'closed stdin unexpectedly installed' >&2
            exit 1
          fi
          test ! -e "$eof/.zpkg.lock"

          allow="$RUNNER_TEMP/no-manifest-allow"
          docker run --rm \
            --volume "$allow:/work" \
            --workdir /work \
            "${common[@]}" \
            sh -euc '
              zed install zed-pkg/docker-node-lib \
                --allow-no-manifest \
                --registry file:///registry \
                --home /zed-home/allow \
                --install-mode copy
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              node app.js
            '

          alias="$RUNNER_TEMP/no-manifest-alias"
          docker run --rm \
            --volume "$alias:/work" \
            --workdir /work \
            "${common[@]}" \
            sh -euc '
              zed install zed-pkg/docker-node-lib \
                --skip-manifest \
                --registry file:///registry \
                --home /zed-home/alias \
                --install-mode copy
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              node app.js
            '

          empty="$RUNNER_TEMP/no-manifest-empty"
          if docker run --rm \
            --volume "$empty:/work" \
            --workdir /work \
            "${common[@]}" \
            zed install --skip-manifest \
              --registry file:///registry \
              --home /zed-home/empty \
              --install-mode copy
          then
            echo 'manifest-free install without specs or lock unexpectedly succeeded' >&2
            exit 1
          fi

          frozen="$RUNNER_TEMP/no-manifest-frozen"
          docker run --rm \
            --volume "$frozen:/work" \
            --workdir /work \
            "${common[@]}" \
            zed install zed-pkg/docker-node-lib \
              --skip-manifest \
              --registry file:///registry \
              --home /zed-home/frozen \
              --install-mode copy
          rm -rf "$frozen/zed_modules" "$frozen/node_modules" "$frozen/.zed"
          docker run --rm \
            --volume "$frozen:/work" \
            --workdir /work \
            "${common[@]}" \
            sh -euc '
              zed install --frozen --skip-manifest \
                --registry file:///registry \
                --home /zed-home/frozen \
                --install-mode copy
              test ! -e .zpkg.toml
              test -f .zpkg.lock
              node app.js
            '

          nested_root="$RUNNER_TEMP/no-manifest-nested"
          docker run --rm \
            --volume "$nested_root:/work" \
            --workdir /work \
            "${common[@]}" \
            sh -euc '
              zed install zed-pkg/docker-node-lib \
                --skip-manifest \
                --registry file:///registry \
                --home /zed-home/nested \
                --install-mode copy
              test ! -e /work/.zpkg.toml
              test ! -e /work/.zpkg.lock
              test -f /work/apps/web/.zpkg.lock
              test -d /work/apps/web/node_modules/@zed-pkg/docker-node-lib
              node /work/apps/web/app.js
            '
'''
write(".github/workflows/ci.yml", ci)

print("DEN-564 patch applied")
