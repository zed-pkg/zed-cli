use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use globset::Glob;
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, VersionScheme};

use crate::config::read_manifest;

const GENERATED_ORG: &str = "zed-local";
const GENERATED_VERSION: &str = "0.0.0";
const GENERATED_MARKER: &str = "zed-generated-consumer";
const GENERATED_DESCRIPTION: &str =
    "Local Zed dependency manifest; edit package metadata before publishing";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubmoduleConfig {
    pub(super) name: String,
    pub(super) path: String,
    pub(super) url: String,
    pub(super) branch: Option<String>,
}

#[derive(Debug, Default)]
struct SubmoduleBuilder {
    path: Option<String>,
    url: Option<String>,
    branch: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct WorkspaceMember {
    pub(super) path: String,
    pub(super) root: PathBuf,
    pub(super) manifest: Manifest,
}

pub(super) fn verify_checkout(project: &Path, relative: &str, child: &Path) -> Result<String> {
    let parent_commit = gitlink_commit(project, relative)?;
    let child_commit =
        git_line(child, &["rev-parse", "HEAD"]).context("reading submodule checkout commit")?;
    if parent_commit != child_commit {
        bail!(
            "submodule `{relative}` is checked out at {child_commit}, but the superproject HEAD pins {parent_commit}; run `zed install --git-submodules`"
        );
    }

    let status = checked_git(
        child,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let status = String::from_utf8(status.stdout).context("Git status output is not UTF-8")?;
    if !status.trim().is_empty() {
        let first = status.lines().take(8).collect::<Vec<_>>().join("; ");
        bail!(
            "adopted submodule `{relative}` is dirty ({first}); commit/stash changes before Zed refreshes its immutable lock"
        );
    }

    let nested = checked_git(child, &["submodule", "status", "--recursive"])?;
    let nested =
        String::from_utf8(nested.stdout).context("nested submodule status is not UTF-8")?;
    for line in nested.lines().filter(|line| !line.trim().is_empty()) {
        if matches!(line.as_bytes().first().copied(), Some(b'-' | b'+' | b'U')) {
            bail!(
                "nested submodule drift under `{relative}`: `{line}`; run `zed install --git-submodules`"
            );
        }
    }
    Ok(child_commit)
}

fn gitlink_commit(project: &Path, relative: &str) -> Result<String> {
    validate_relative_path(relative)?;
    let output = checked_git(project, &["ls-tree", "HEAD", "--", relative])?;
    let text = String::from_utf8(output.stdout).context("Git tree output is not UTF-8")?;
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .with_context(|| {
            format!(
                "workspace path `{relative}` is not committed as a Git submodule at superproject HEAD"
            )
        })?;
    let mut fields = line.split_whitespace();
    let mode = fields.next().unwrap_or_default();
    let kind = fields.next().unwrap_or_default();
    let commit = fields.next().unwrap_or_default();
    if mode != "160000" || kind != "commit" || !is_git_object_id(commit) {
        bail!(
            "workspace path `{relative}` is not a committed Git submodule gitlink at HEAD (found `{line}`)"
        );
    }
    Ok(commit.to_string())
}

pub(super) fn origin_url(project: &Path) -> Option<String> {
    git_line(project, &["remote", "get-url", "origin"])
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn warn_on_repository_mismatch(
    module: &SubmoduleConfig,
    child: &Path,
    manifest: &Manifest,
) {
    let declared = &manifest.package.repository.url;
    if normalized_git_url(&module.url) != normalized_git_url(declared) {
        eprintln!(
            "warning: submodule `{}` declares transport `{}`, while {MANIFEST_FILE} declares `{declared}`; the lock records the committed .gitmodules transport",
            module.name, module.url
        );
    }
    if let Some(actual) = origin_url(child)
        && normalized_git_url(&actual) != normalized_git_url(&module.url)
    {
        eprintln!(
            "warning: checkout origin `{actual}` differs from committed submodule transport `{}` for `{}`; `zed install --git-submodules` will resynchronize it",
            module.url, module.name
        );
    }
}

pub(super) fn verify_gitmodules_committed(project: &Path) -> Result<()> {
    let output = checked_git(
        project,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            ".gitmodules",
        ],
    )?;
    let status = String::from_utf8(output.stdout).context("Git status output is not UTF-8")?;
    if !status.trim().is_empty() {
        bail!(
            ".gitmodules has uncommitted changes (`{}`); commit it before Zed records immutable submodule provenance",
            status.lines().take(4).collect::<Vec<_>>().join("; ")
        );
    }
    Ok(())
}

pub(super) fn configured_submodules(project: &Path) -> Result<Vec<SubmoduleConfig>> {
    if !project.join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    let output = git_output(
        project,
        &[
            "config",
            "--null",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.(path|url|branch)$",
        ],
    )?;
    // `git config --get-regexp` returns 1 for no matches.
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return git_failure(
            project,
            &[
                "config",
                "--null",
                "--file",
                ".gitmodules",
                "--get-regexp",
                r"^submodule\..*\.(path|url|branch)$",
            ],
            output,
        );
    }
    let bytes = output.stdout;
    let mut builders: BTreeMap<String, SubmoduleBuilder> = BTreeMap::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(raw).context(".gitmodules contains non-UTF-8 data")?;
        // With --null, Git uses a newline between key and value and NUL between
        // records. Accept a space separator as compatibility with older Git.
        let (key, value) = record
            .split_once('\n')
            .or_else(|| record.split_once(' '))
            .with_context(|| format!("unrecognized git config record `{record}`"))?;
        let Some(rest) = key.strip_prefix("submodule.") else {
            continue;
        };
        let (name, field) = ["path", "url", "branch"]
            .into_iter()
            .find_map(|field| {
                rest.strip_suffix(&format!(".{field}"))
                    .map(|name| (name, field))
            })
            .with_context(|| format!("unsupported .gitmodules key `{key}`"))?;
        let builder = builders.entry(name.to_string()).or_default();
        match field {
            "path" => builder.path = Some(value.to_string()),
            "url" => builder.url = Some(value.to_string()),
            "branch" => builder.branch = Some(value.to_string()),
            _ => unreachable!(),
        }
    }

    let mut modules = Vec::with_capacity(builders.len());
    let mut paths = BTreeSet::new();
    for (name, builder) in builders {
        let path = builder
            .path
            .with_context(|| format!("submodule `{name}` is missing `path`"))?;
        let path = normalize_path_text(&path);
        validate_relative_path(&path)?;
        if !paths.insert(path.clone()) {
            bail!("duplicate submodule path `{path}` in .gitmodules");
        }
        let url = builder
            .url
            .filter(|url| !url.trim().is_empty())
            .with_context(|| format!("submodule `{name}` is missing `url`"))?;
        modules.push(SubmoduleConfig {
            name,
            path,
            url,
            branch: builder.branch.filter(|branch| !branch.trim().is_empty()),
        });
    }
    modules.sort_by(|left, right| (&left.path, &left.name).cmp(&(&right.path, &right.name)));
    Ok(modules)
}

pub(super) fn collect_workspace_members(
    project: &Path,
    manifest: &Manifest,
) -> Result<BTreeMap<String, WorkspaceMember>> {
    let mut members = BTreeMap::new();
    let Some(workspace) = manifest.workspace.as_ref() else {
        return Ok(members);
    };
    let canonical_project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing project {}", project.display()))?;

    for pattern in &workspace.members {
        let mut candidates = vec![project.to_path_buf()];
        for segment in pattern.split('/') {
            let mut next = Vec::new();
            for base in &candidates {
                if segment.contains('*') {
                    let matcher = Glob::new(segment)
                        .with_context(|| format!("invalid workspace glob segment `{segment}`"))?
                        .compile_matcher();
                    if let Ok(entries) = fs::read_dir(base) {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            if entry.path().is_dir()
                                && matcher.is_match(Path::new(&name))
                                && !name.to_string_lossy().starts_with('.')
                            {
                                next.push(entry.path());
                            }
                        }
                    }
                } else {
                    let candidate = base.join(segment);
                    if candidate.is_dir() {
                        next.push(candidate);
                    }
                }
            }
            candidates = next;
        }

        for root in candidates {
            let canonical = fs::canonicalize(&root)
                .with_context(|| format!("canonicalizing workspace member {}", root.display()))?;
            if !canonical.starts_with(&canonical_project) {
                bail!(
                    "workspace member {} resolves outside project {}",
                    root.display(),
                    project.display()
                );
            }
            if !root.join(MANIFEST_FILE).is_file() {
                continue;
            }
            let member_manifest = read_manifest(&root)
                .with_context(|| format!("reading workspace member manifest {}", root.display()))?;
            let package = member_manifest.full_name();
            let relative = root
                .strip_prefix(project)
                .with_context(|| format!("workspace member {} escaped project", root.display()))?;
            let path = path_text(relative)?;
            if let Some(previous) = members.insert(
                package.clone(),
                WorkspaceMember {
                    path: path.clone(),
                    root,
                    manifest: member_manifest,
                },
            ) && previous.path != path
            {
                bail!(
                    "workspace package `{package}` is provided by both `{}` and `{path}`",
                    previous.path
                );
            }
        }
    }
    Ok(members)
}

pub(super) fn generated_consumer_manifest(project: &Path) -> Manifest {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    Manifest {
        package: PackageSection {
            org: GENERATED_ORG.to_string(),
            name: name.clone(),
            version: GENERATED_VERSION.to_string(),
            version_scheme: VersionScheme::Semver,
            description: Some(GENERATED_DESCRIPTION.to_string()),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: format!("https://localhost/{GENERATED_ORG}/{name}"),
            },
            keywords: vec![GENERATED_MARKER.to_string()],
            language: Default::default(),
            ecosystem: Default::default(),
            artifacts: Default::default(),
        },
        workspace: None,
        dependencies: BTreeMap::new(),
        build_dependencies: BTreeMap::new(),
        native_dependencies: Default::default(),
        hooks: Default::default(),
        lifecycle: Default::default(),
        build: None,
        overrides: Default::default(),
        bin: BTreeMap::new(),
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
        install: Default::default(),
        interop: Default::default(),
        targets: Default::default(),
    }
}

pub(super) fn exact_requirement(scheme: VersionScheme, raw: &str) -> String {
    match scheme {
        VersionScheme::Semver => format!("={raw}"),
        VersionScheme::Calver => version::normalize_calver(raw)
            .map(|normalized| format!("={normalized}"))
            .unwrap_or_else(|| raw.to_string()),
        VersionScheme::Opaque => raw.to_string(),
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

pub(super) fn validate_relative_path(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value != value.trim()
        || value.contains('\0')
        || normalize_path_text(value) != value
        || value.split('/').any(str::is_empty)
    {
        bail!("unsafe or non-canonical Git submodule path `{value}`");
    }
    let unsafe_segment = value.split('/').any(|segment| {
        segment == "."
            || segment == ".."
            || segment.eq_ignore_ascii_case(".git")
            || segment.eq_ignore_ascii_case(crate::transaction::STAGING_DIR)
    });
    let path = Path::new(value);
    if unsafe_segment
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe Git submodule path `{value}`");
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("Git submodule/workspace paths must be UTF-8")?;
    let text = normalize_path_text(text);
    validate_relative_path(&text)?;
    Ok(text)
}

fn normalize_path_text(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_string()
}

fn normalized_git_url(value: &str) -> String {
    let mut value = value
        .trim()
        .strip_prefix("git+")
        .unwrap_or(value.trim())
        .to_string();
    if !value.contains("://")
        && let Some((authority, path)) = value.split_once(':')
        && authority.contains('@')
    {
        value = format!("ssh://{authority}/{path}");
    }
    while value.ends_with('/') {
        value.pop();
    }
    if value.ends_with(".git") {
        value.truncate(value.len() - 4);
    }
    value
}

pub(super) fn is_git_object_id(value: &str) -> bool {
    (7..=128).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !value.bytes().all(|byte| byte == b'0')
}

fn git_line(project: &Path, args: &[&str]) -> Result<String> {
    let output = checked_git(project, args)?;
    let mut text = String::from_utf8(output.stdout).context("Git output is not UTF-8")?;
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    Ok(text)
}

pub(super) fn checked_git(project: &Path, args: &[&str]) -> Result<Output> {
    let output = git_output(project, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        git_failure(project, args, output)
    }
}

fn git_output(project: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("running git in {}", project.display()))
}

fn git_failure<T>(project: &Path, args: &[&str], output: Output) -> Result<T> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    bail!(
        "git -C {} {} failed with {}{}",
        project.display(),
        args.join(" "),
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}
