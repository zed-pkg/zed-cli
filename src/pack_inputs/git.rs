use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use globset::GlobBuilder;
use walkdir::WalkDir;

use super::fallback::find_worktree_root;
use super::{IGNORED_INPUT_ALLOW_FILE, MAX_GIT_OUTPUT_BYTES};

#[derive(Debug)]
struct GitContext {
    root: PathBuf,
    scan_from: PathBuf,
}

/// Enumerate ignored, untracked files inside initialized Git work trees nested
/// below the package root. The primary work tree is queried by the caller so it
/// can preserve its Git-less fallback behavior; this function contributes only
/// distinct nested repositories and submodules.
pub(super) fn nested_ignored_paths(project: &Path) -> Result<Vec<PathBuf>> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package root {}", project.display()))?;
    let primary_root = find_worktree_root(&project);
    let mut roots = BTreeSet::from([primary_root]);
    let mut contexts = Vec::new();

    for entry in WalkDir::new(&project)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry = entry.with_context(|| {
            format!(
                "discovering nested Git work trees under {}",
                project.display()
            )
        })?;
        if !entry.file_type().is_dir() || !entry.path().join(".git").exists() {
            continue;
        }

        let root = fs::canonicalize(entry.path()).with_context(|| {
            format!(
                "canonicalizing nested Git work tree {}",
                entry.path().display()
            )
        })?;
        if !roots.insert(root.clone()) {
            continue;
        }

        let output = run_git(&root, &root, &["rev-parse", "--show-toplevel"])?;
        if !output.status.success() {
            // A directory containing a nonfunctional `.git` marker is package
            // data, not an initialized nested work tree.
            continue;
        }
        let resolved = stdout_line(&output, "nested Git work-tree root")?;
        let resolved = fs::canonicalize(&resolved)
            .with_context(|| format!("canonicalizing nested Git root `{resolved}`"))?;
        ensure!(
            resolved == root,
            "nested Git marker at {} resolves to unexpected work tree {}",
            root.display(),
            resolved.display()
        );
        contexts.push(GitContext {
            root: root.clone(),
            scan_from: root,
        });
    }

    let mut paths = Vec::new();
    for context in contexts {
        for absolute in ignored_files(&context)? {
            if let Ok(relative) = absolute.strip_prefix(&project) {
                paths.push(relative.to_path_buf());
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(super) fn ignored_input_patterns(project: &Path) -> Result<Vec<String>> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package root {}", project.display()))?;
    let path = project.join(IGNORED_INPUT_ALLOW_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let metadata =
        fs::symlink_metadata(&path).with_context(|| format!("inspecting {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "`{IGNORED_INPUT_ALLOW_FILE}` must be a regular file, not a symlink or directory"
    );

    let root = find_worktree_root(&project);
    ensure!(
        root.join(".git").exists(),
        "`{IGNORED_INPUT_ALLOW_FILE}` requires a Git work tree so its review state can be verified"
    );
    let root_relative = path.strip_prefix(&root).with_context(|| {
        format!(
            "`{IGNORED_INPUT_ALLOW_FILE}` resolved outside Git work tree {}",
            root.display()
        )
    })?;
    let root_relative = root_relative
        .to_str()
        .with_context(|| format!("`{IGNORED_INPUT_ALLOW_FILE}` has a non-UTF-8 repository path"))?
        .replace(std::path::MAIN_SEPARATOR, "/");

    let tracked = run_git(
        &root,
        &root,
        &["ls-files", "--error-unmatch", "--", &root_relative],
    )?;
    ensure!(
        tracked.status.success(),
        "`{IGNORED_INPUT_ALLOW_FILE}` must be tracked before it can admit ignored publication inputs"
    );
    require_clean(&root, &root_relative, false)?;
    require_clean(&root, &root_relative, true)?;

    let mut patterns = Vec::new();
    for (index, line) in fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .lines()
        .enumerate()
    {
        let pattern = line.trim();
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }
        validate_allow_pattern(pattern).with_context(|| {
            format!(
                "invalid `{IGNORED_INPUT_ALLOW_FILE}` entry on line {}",
                index + 1
            )
        })?;
        patterns.push(pattern.to_string());
    }
    Ok(patterns)
}

pub(super) fn validate_allow_pattern(pattern: &str) -> Result<()> {
    ensure!(!pattern.is_empty(), "patterns may not be empty");
    ensure!(
        pattern == pattern.trim(),
        "patterns may not have leading or trailing whitespace"
    );
    ensure!(
        !pattern.starts_with('!'),
        "negated patterns are not supported"
    );
    ensure!(
        !pattern.starts_with('/'),
        "patterns must be project-relative"
    );
    ensure!(
        !pattern.starts_with("./"),
        "patterns must not start with `./`"
    );
    ensure!(
        !(pattern.len() >= 2
            && pattern.as_bytes()[0].is_ascii_alphabetic()
            && pattern.as_bytes()[1] == b':'),
        "patterns must not use a Windows drive prefix"
    );
    ensure!(!pattern.contains('\\'), "patterns must use `/` separators");
    ensure!(
        !matches!(pattern, "*" | "**" | "**/*"),
        "patterns must identify a bounded file or directory, not the complete project"
    );
    ensure!(
        !pattern
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == ".."),
        "patterns must not contain empty, `.` or `..` path segments"
    );
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("invalid glob `{pattern}`"))?;
    Ok(())
}

fn ignored_files(context: &GitContext) -> Result<Vec<PathBuf>> {
    let output = run_git(
        &context.root,
        &context.scan_from,
        &[
            "ls-files",
            "--full-name",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "listing Git-ignored package inputs in {} failed: {}",
            context.scan_from.display(),
            diagnostic_stderr(&output)
        );
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let relative = std::str::from_utf8(raw)
                .context("Git returned a non-UTF-8 ignored path; rename it before packaging")?;
            Ok(context.root.join(relative))
        })
        .collect()
}

fn require_clean(root: &Path, path: &str, cached: bool) -> Result<()> {
    let mut args = vec!["diff"];
    if cached {
        args.push("--cached");
    }
    args.extend(["--quiet", "--", path]);
    let output = run_git(root, root, &args)?;
    match output.status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!(
            "`{IGNORED_INPUT_ALLOW_FILE}` must be committed and clean before it can admit ignored publication inputs"
        ),
        _ => bail!(
            "checking `{IGNORED_INPUT_ALLOW_FILE}` cleanliness failed: {}",
            diagnostic_stderr(&output)
        ),
    }
}

fn run_git(safe_root: &Path, directory: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .arg("-c")
        .arg(format!("safe.directory={}", safe_root.display()))
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    ensure!(
        output.stdout.len() <= MAX_GIT_OUTPUT_BYTES && output.stderr.len() <= MAX_GIT_OUTPUT_BYTES,
        "Git output exceeded the {}-byte packaging safety limit",
        MAX_GIT_OUTPUT_BYTES
    );
    Ok(output)
}

fn stdout_line(output: &Output, label: &str) -> Result<String> {
    ensure!(
        output.status.success(),
        "resolving {label} failed: {}",
        diagnostic_stderr(output)
    );
    let value = std::str::from_utf8(&output.stdout)
        .with_context(|| format!("Git returned a non-UTF-8 {label}"))?;
    let value = value.trim_end_matches(['\r', '\n']);
    ensure!(!value.is_empty(), "Git returned an empty {label}");
    Ok(value.to_string())
}

fn diagnostic_stderr(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if message.is_empty() {
        format!("Git exited with {}", output.status)
    } else {
        message
    }
}
