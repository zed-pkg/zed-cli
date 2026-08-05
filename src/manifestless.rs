//! Manifestless dependency installation.
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
    allow_ecosystem_mismatch: bool,
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
            allow_ecosystem_mismatch,
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
    let lock_only = matches!(&requested, RequestedDependencies::Locked(_));
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
        if lock_only {
            ops::install_frozen_lock_only(
                &selection.root,
                cfg,
                mode,
                adapter,
                allow_build,
                inferred_target.as_deref(),
                allow_ecosystem_mismatch,
            )
        } else {
            ops::install(
                &selection.root,
                cfg,
                frozen,
                mode,
                adapter,
                allow_build,
                inferred_target.as_deref(),
                allow_ecosystem_mismatch,
            )
        }
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
        .with_context(|| {
            format!("invalid package spec `{spec}` (expected org/name[@requirement])")
        })?;
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
            // A synthesized manifest for a project that has none: no language
            // claim of its own, so nothing here is ecosystem-gated.
            language: Default::default(),
            ecosystem: Default::default(),
        },
        workspace: None,
        dependencies,
        build_dependencies: BTreeMap::new(),
        native_dependencies: Default::default(),
        hooks: Default::default(),
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
    if let Some(root) = structure_ancestor(requested) {
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
        if dir.join(LOCKFILE_FILE).is_file() || ops::detect_native_manifest_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn structure_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if ops::detect_structure_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn nested_candidates(start: &Path, matches: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir() && matches(entry.path()))
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
}

/// Select a nested project only when there is exactly one plausible native
/// root. Authoritative native manifests absorb weaker source-layout candidates
/// below them; unrelated sibling candidates still make the repository
/// ambiguous, so Zed safely stays at the requested root.
fn unique_nested_project(start: &Path) -> Option<PathBuf> {
    let mut authoritative = nested_candidates(start, |path| {
        path.join(LOCKFILE_FILE).is_file() || ops::detect_native_manifest_target(path).is_some()
    });
    let mut heuristic =
        nested_candidates(start, |path| ops::detect_structure_target(path).is_some());
    heuristic.retain(|candidate| !authoritative.iter().any(|root| candidate.starts_with(root)));
    authoritative.extend(heuristic);
    authoritative.sort();
    authoritative.dedup();
    (authoritative.len() == 1).then(|| authoritative.remove(0))
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
        Adapter::Go => "go (also .zed/go.work; use GOWORK=)",
        Adapter::Python => "python (also .zed/pythonpath; use PYTHONPATH=)",
        Adapter::Rust => "rust (also .zed/cargo-paths.toml to merge into .cargo/config.toml)",
        Adapter::Dart => "dart (also .zed/pub-deps.yaml to merge into pubspec.yaml)",
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
    writeln!(
        output,
        "  ecosystem adapter: {}",
        adapter_label(plan.adapter)
    )?;
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
        for accepted in [
            b"y\n".as_slice(),
            b"YES\n".as_slice(),
            b" yes \n".as_slice(),
        ] {
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
    fn native_manifest_ancestor_beats_a_nearer_structure_marker() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("services/api");
        let invocation = project.join("cmd/app/deep");
        fs::create_dir_all(&invocation).unwrap();
        fs::write(
            project.join("go.mod"),
            "module example.com/api
",
        )
        .unwrap();
        fs::write(
            project.join("cmd/app/main.go"),
            "package main
",
        )
        .unwrap();

        assert_eq!(select_project(&invocation).root, project);
    }

    #[test]
    fn one_nested_manifest_absorbs_its_structure_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("services/api");
        fs::create_dir_all(project.join("cmd/app")).unwrap();
        fs::write(
            project.join("go.mod"),
            "module example.com/api
",
        )
        .unwrap();
        fs::write(
            project.join("cmd/app/main.go"),
            "package main
",
        )
        .unwrap();

        assert_eq!(select_project(temp.path()).root, project);
    }

    #[test]
    fn unrelated_structure_sibling_keeps_nested_selection_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let api = temp.path().join("services/api");
        let web = temp.path().join("apps/web");
        fs::create_dir_all(api.join("cmd/app")).unwrap();
        fs::create_dir_all(web.join("src")).unwrap();
        fs::write(
            api.join("go.mod"),
            "module example.com/api
",
        )
        .unwrap();
        fs::write(
            api.join("cmd/app/main.go"),
            "package main
",
        )
        .unwrap();
        fs::write(
            web.join("src/main.ts"),
            "console.log('web')
",
        )
        .unwrap();

        assert_eq!(select_project(temp.path()).root, temp.path());
    }

    #[test]
    fn no_specs_require_an_explicit_frozen_lock_and_frozen_rejects_specs() {
        let temp = tempfile::tempdir().unwrap();
        let missing = requested_dependencies(temp.path(), &[], false)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("no package specs"));
        let incompatible =
            requested_dependencies(temp.path(), &["acme/http-kit@^1".to_string()], true)
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
