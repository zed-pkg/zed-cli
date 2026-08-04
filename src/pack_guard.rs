use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{IGNORE_FILE, MANIFEST_FILE};

/// VCS control data is never part of a package payload. The root-directory
/// defaults predate Git worktree pointer files and nested submodules, so the
/// CLI adds these rules in memory for pack/publish without rewriting the
/// author's manifest.
const VCS_METADATA_EXCLUDES: &[&str] = &[
    ".git",
    "**/.git",
    "**/.git/**",
    ".gitmodules",
    "**/.gitmodules",
    ".hg",
    "**/.hg",
    "**/.hg/**",
    ".svn",
    "**/.svn",
    "**/.svn/**",
];

const SUBMODULE_PROBES: &[&str] = &[
    "__zed_pack_probe__",
    MANIFEST_FILE,
    "src/__zed_pack_probe__",
];

pub(crate) fn harden_manifest(mut manifest: Manifest) -> Manifest {
    for pattern in VCS_METADATA_EXCLUDES {
        if !manifest
            .publish
            .exclude
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(pattern))
        {
            manifest.publish.exclude.push((*pattern).to_string());
        }
    }
    manifest
}

/// Verify every Git submodule that can contribute files to at least one
/// package artifact. Submodules excluded from every artifact remain optional;
/// included ones must be initialized, exact, clean, and recursively settled.
pub(crate) fn preflight_submodules(project: &Path, manifest: &Manifest) -> Result<usize> {
    let Some(root) = crate::git_submodules::find_root(project) else {
        return Ok(0);
    };
    let paths = configured_submodule_paths(&root)?;
    if paths.is_empty() {
        return Ok(0);
    }

    let canonical_root = fs::canonicalize(&root)
        .with_context(|| format!("canonicalizing Git superproject {}", root.display()))?;
    let views = artifact_views(project, manifest, &canonical_root)?;
    let included = paths
        .into_iter()
        .filter(|relative| {
            let module = canonical_root.join(relative);
            views.iter().any(|view| view.may_include(&module))
        })
        .collect::<Vec<_>>();

    if included.is_empty() {
        return Ok(0);
    }

    verify_gitmodules_committed(&root)?;
    for relative in &included {
        verify_checkout(&root, &canonical_root, relative)?;
    }
    Ok(included.len())
}

struct ArtifactView {
    source: PathBuf,
    excludes: GlobSet,
    always: GlobSet,
}

impl ArtifactView {
    fn may_include(&self, module: &Path) -> bool {
        let relative = if let Ok(relative) = module.strip_prefix(&self.source) {
            relative
        } else if self.source.starts_with(module) {
            Path::new("")
        } else {
            return false;
        };

        SUBMODULE_PROBES.iter().any(|probe| {
            let candidate = relative.join(probe);
            self.always.is_match(&candidate) || !self.excludes.is_match(&candidate)
        })
    }
}

fn artifact_views(
    project: &Path,
    manifest: &Manifest,
    canonical_root: &Path,
) -> Result<Vec<ArtifactView>> {
    let always = ALWAYS_INCLUDE
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();

    if manifest.is_polyglot() {
        let excludes = effective_excludes(
            &manifest.publish.exclude,
            manifest.publish.include_readme,
        );
        let mut views = Vec::with_capacity(manifest.targets.len());
        for (target, section) in &manifest.targets {
            let source = project.join(&section.dir);
            let source = fs::canonicalize(&source).with_context(|| {
                format!(
                    "canonicalizing target `{target}` source root {}",
                    source.display()
                )
            })?;
            ensure_source_within_root(&source, canonical_root)?;
            views.push(ArtifactView {
                source,
                excludes: glob_set(&excludes)?,
                always: glob_set(&always)?,
            });
        }
        return Ok(views);
    }

    let source = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package source {}", project.display()))?;
    ensure_source_within_root(&source, canonical_root)?;

    let mut extra = manifest.publish.exclude.clone();
    let modules_dir = manifest.modules_dir().trim_matches('/').to_string();
    if !modules_dir.is_empty() {
        extra.push(format!("{modules_dir}/**"));
    }
    extra.push(format!("{}/**", crate::transaction::STAGING_DIR));
    let ignore_file = project.join(IGNORE_FILE);
    if ignore_file.is_file() {
        for line in fs::read_to_string(&ignore_file)?.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                extra.push(line.to_string());
            }
        }
    }

    Ok(vec![ArtifactView {
        source,
        excludes: glob_set(&effective_excludes(
            &extra,
            manifest.publish.include_readme,
        ))?,
        always: glob_set(&always)?,
    }])
}

fn ensure_source_within_root(source: &Path, root: &Path) -> Result<()> {
    if !source.starts_with(root) {
        bail!(
            "package source {} resolves outside Git superproject {}; refusing to package submodule content",
            source.display(),
            root.display()
        );
    }
    Ok(())
}

fn glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid publish exclusion `{pattern}`"))?,
        );
    }
    Ok(builder.build()?)
}

fn configured_submodule_paths(project: &Path) -> Result<Vec<String>> {
    if !project.join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    let args = [
        "config",
        "--null",
        "--file",
        ".gitmodules",
        "--get-regexp",
        r"^submodule\..*\.path$",
    ];
    let output = git_output(project, &args)?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return git_failure(project, &args, output);
    }

    let mut paths = BTreeSet::new();
    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(raw).context(".gitmodules contains non-UTF-8 data")?;
        let (_, value) = record
            .split_once('\n')
            .or_else(|| record.split_once(' '))
            .with_context(|| format!("unrecognized git config record `{record}`"))?;
        let path = normalize_path_text(value);
        validate_relative_path(&path)?;
        if !paths.insert(path.clone()) {
            bail!("duplicate submodule path `{path}` in .gitmodules");
        }
    }
    Ok(paths.into_iter().collect())
}

fn normalize_path_text(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_string()
}

fn validate_relative_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains("//")
        || value.split('/').any(|component| component.is_empty())
    {
        bail!("unsafe Git submodule path `{value}`");
    }
    for component in Path::new(value).components() {
        let Component::Normal(component) = component else {
            bail!("unsafe Git submodule path `{value}`");
        };
        let component = component.to_string_lossy();
        if component.eq_ignore_ascii_case(".git")
            || component == crate::transaction::STAGING_DIR
        {
            bail!("unsafe Git submodule path `{value}`");
        }
    }
    Ok(())
}

fn verify_gitmodules_committed(project: &Path) -> Result<()> {
    checked_git(
        project,
        &["ls-files", "--error-unmatch", "--", ".gitmodules"],
    )
    .context(".gitmodules is not committed at superproject HEAD")?;
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
            ".gitmodules has uncommitted changes (`{}`); commit it before packing",
            status.lines().take(4).collect::<Vec<_>>().join("; ")
        );
    }
    Ok(())
}

fn verify_checkout(project: &Path, canonical_root: &Path, relative: &str) -> Result<()> {
    let child = canonical_root.join(relative);
    let marker = child.join(".git");
    let marker_metadata = fs::symlink_metadata(&marker).with_context(|| {
        format!(
            "included submodule `{relative}` is not initialized; run `zed install --git-submodules` before packing"
        )
    })?;
    if marker_metadata.file_type().is_symlink() {
        bail!(
            "included submodule `{relative}` has a symlinked .git control path; refusing to package it"
        );
    }

    let canonical_child = fs::canonicalize(&child)
        .with_context(|| format!("canonicalizing included submodule `{relative}`"))?;
    if !canonical_child.starts_with(canonical_root) {
        bail!(
            "included submodule `{relative}` resolves outside superproject {}; refusing",
            project.display()
        );
    }

    let parent_commit = gitlink_commit(project, relative)?;
    let child_commit = git_line(&canonical_child, &["rev-parse", "HEAD"])
        .context("reading included submodule checkout commit")?;
    if parent_commit != child_commit {
        bail!(
            "included submodule `{relative}` is checked out at {child_commit}, but superproject HEAD pins {parent_commit}; run `zed install --git-submodules` before packing"
        );
    }

    let status = checked_git(
        &canonical_child,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let status = String::from_utf8(status.stdout).context("Git status output is not UTF-8")?;
    if !status.trim().is_empty() {
        bail!(
            "included submodule `{relative}` is dirty ({}); commit or stash changes before packing",
            status.lines().take(8).collect::<Vec<_>>().join("; ")
        );
    }

    let nested = checked_git(&canonical_child, &["submodule", "status", "--recursive"])?;
    let nested =
        String::from_utf8(nested.stdout).context("nested submodule status is not UTF-8")?;
    for line in nested.lines().filter(|line| !line.trim().is_empty()) {
        if matches!(line.as_bytes().first().copied(), Some(b'-' | b'+' | b'U')) {
            bail!(
                "nested submodule drift under `{relative}`: `{line}`; run `zed install --git-submodules` before packing"
            );
        }
    }
    Ok(())
}

fn gitlink_commit(project: &Path, relative: &str) -> Result<String> {
    let output = checked_git(project, &["ls-tree", "HEAD", "--", relative])?;
    let text = String::from_utf8(output.stdout).context("Git tree output is not UTF-8")?;
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .with_context(|| {
            format!(
                "included path `{relative}` is not committed as a Git submodule at superproject HEAD"
            )
        })?;
    let mut fields = line.split_whitespace();
    let mode = fields.next().unwrap_or_default();
    let kind = fields.next().unwrap_or_default();
    let commit = fields.next().unwrap_or_default();
    if mode != "160000" || kind != "commit" || !is_git_object_id(commit) {
        bail!(
            "included path `{relative}` is not a committed Git submodule gitlink at HEAD (found `{line}`)"
        );
    }
    Ok(commit.to_string())
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_line(project: &Path, args: &[&str]) -> Result<String> {
    let output = checked_git(project, args)?;
    Ok(String::from_utf8(output.stdout)
        .context("Git output is not UTF-8")?
        .trim()
        .to_string())
}

fn checked_git(project: &Path, args: &[&str]) -> Result<Output> {
    let output = git_output(project, args)?;
    if output.status.success() {
        return Ok(output);
    }
    git_failure(project, args, output)
}

fn git_output(project: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .output()
        .with_context(|| format!("running git {} in {}", args.join(" "), project.display()))
}

fn git_failure<T>(project: &Path, args: &[&str], output: Output) -> Result<T> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    bail!(
        "git {} failed in {} ({}): {}{}",
        args.join(" "),
        project.display(),
        output.status,
        stderr.trim(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!("; {}", stdout.trim())
        }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::Read;

    use flate2::read::GzDecoder;

    use super::*;

    fn manifest_text(extra: &str) -> String {
        format!(
            r#"[package]
org = "acme"
name = "pack-guard"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/pack-guard.git"

{extra}
"#
        )
    }

    fn git(project: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Zed Pack Guard")
            .env("GIT_AUTHOR_EMAIL", "zed-pack-guard@example.invalid")
            .env("GIT_COMMITTER_NAME", "Zed Pack Guard")
            .env("GIT_COMMITTER_EMAIL", "zed-pack-guard@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn archive_files(path: &Path) -> BTreeSet<String> {
        let file = fs::File::open(path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn hardened_manifest_strips_nested_vcs_control_files() {
        let project = tempfile::tempdir().unwrap();
        let manifest_text = manifest_text("");
        fs::write(project.path().join(MANIFEST_FILE), &manifest_text).unwrap();
        fs::create_dir_all(project.path().join("vendor/client/.git/objects")).unwrap();
        fs::write(project.path().join("vendor/client/.git/HEAD"), "secret\n").unwrap();
        fs::write(project.path().join("vendor/client/lib.txt"), "runtime\n").unwrap();
        fs::write(project.path().join(".gitmodules"), "transport metadata\n").unwrap();

        let manifest = harden_manifest(Manifest::parse(&manifest_text).unwrap());
        let packed = crate::pack::pack(project.path(), &manifest, None).unwrap();
        let files = archive_files(&packed.path);

        assert!(files.contains("pkg/vendor/client/lib.txt"));
        assert!(!files.iter().any(|path| path.contains("/.git")));
        assert!(!files.iter().any(|path| path.ends_with("/.gitmodules")));
    }

    #[cfg(unix)]
    fn submodule_fixture() -> (tempfile::TempDir, tempfile::TempDir, Manifest) {
        let child = tempfile::tempdir().unwrap();
        git(child.path(), &["init"]);
        fs::write(child.path().join("lib.txt"), "runtime\n").unwrap();
        git(child.path(), &["add", "."]);
        git(child.path(), &["commit", "-m", "child"]);

        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init"]);
        let manifest_text = manifest_text("");
        fs::write(root.path().join(MANIFEST_FILE), &manifest_text).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args([
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                child.path().to_str().unwrap(),
                "vendor/client",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .unwrap();
        assert!(status.success());
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-m", "root"]);

        (
            child,
            root,
            harden_manifest(Manifest::parse(&manifest_text).unwrap()),
        )
    }

    #[cfg(unix)]
    #[test]
    fn included_uninitialized_submodule_fails_but_excluded_one_is_optional() {
        let (_child, root, manifest) = submodule_fixture();
        git(
            root.path(),
            &["submodule", "deinit", "--force", "--", "vendor/client"],
        );

        let error = preflight_submodules(root.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not initialized"), "{error}");

        let mut excluded = manifest;
        excluded
            .publish
            .exclude
            .push("vendor/client/**".to_string());
        assert_eq!(preflight_submodules(root.path(), &excluded).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn dirty_included_submodule_is_rejected() {
        let (_child, root, manifest) = submodule_fixture();
        fs::write(root.path().join("vendor/client/lib.txt"), "dirty\n").unwrap();

        let error = preflight_submodules(root.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("dirty"), "{error}");
    }

    #[test]
    fn archive_reader_helper_reads_runtime_file() {
        let project = tempfile::tempdir().unwrap();
        let manifest_text = manifest_text("");
        fs::write(project.path().join(MANIFEST_FILE), &manifest_text).unwrap();
        fs::write(project.path().join("runtime.txt"), "runtime\n").unwrap();
        let manifest = harden_manifest(Manifest::parse(&manifest_text).unwrap());
        let packed = crate::pack::pack(project.path(), &manifest, None).unwrap();
        let file = fs::File::open(packed.path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        let mut found = false;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap() == Path::new("pkg/runtime.txt") {
                let mut body = String::new();
                entry.read_to_string(&mut body).unwrap();
                assert_eq!(body, "runtime\n");
                found = true;
            }
        }
        assert!(found);
    }
}
