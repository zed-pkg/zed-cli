use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use globset::GlobBuilder;
use walkdir::WalkDir;

use super::{IGNORED_INPUT_ALLOW_FILE, MAX_GIT_OUTPUT_BYTES};

pub(super) struct GitContext {
    pub(super) root: PathBuf,
    scan_from: PathBuf,
}

pub(super) fn discover(project: &Path) -> Result<Vec<GitContext>> {
    let Some(primary) = GitContext::at(project, project)? else {
        return Ok(Vec::new());
    };
    let primary_root = primary.root.clone();
    let primary_scan = primary.scan_from.clone();
    let mut roots = BTreeSet::from([primary.root.clone()]);
    let mut contexts = vec![primary];

    for entry in WalkDir::new(project)
        .min_depth(0)
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
        let Some(context) = GitContext::at(entry.path(), entry.path())? else {
            continue;
        };
        if context.root.starts_with(project) && roots.insert(context.root.clone()) {
            contexts.push(context);
        }
    }

    contexts.sort_by(|left, right| left.root.cmp(&right.root));
    let primary_index = contexts
        .iter()
        .position(|context| context.root == primary_root && context.scan_from == primary_scan)
        .context("primary Git work tree disappeared during discovery")?;
    contexts.swap(0, primary_index);
    Ok(contexts)
}

pub(super) fn ignored_files(context: &GitContext) -> Result<Vec<PathBuf>> {
    let output = run_git(
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

pub(super) fn ignored_input_patterns(project: &Path, context: &GitContext) -> Result<Vec<String>> {
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

    let root_relative = path.strip_prefix(&context.root).with_context(|| {
        format!(
            "`{IGNORED_INPUT_ALLOW_FILE}` resolved outside Git work tree {}",
            context.root.display()
        )
    })?;
    let root_relative = root_relative
        .to_str()
        .with_context(|| format!("`{IGNORED_INPUT_ALLOW_FILE}` has a non-UTF-8 repository path"))?;

    let tracked = run_git(
        &context.root,
        &["ls-files", "--error-unmatch", "--", root_relative],
    )?;
    ensure!(
        tracked.status.success(),
        "`{IGNORED_INPUT_ALLOW_FILE}` must be tracked before it can admit ignored publication inputs"
    );
    require_clean(&context.root, root_relative, false)?;
    require_clean(&context.root, root_relative, true)?;

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

impl GitContext {
    fn at(directory: &Path, scan_from: &Path) -> Result<Option<Self>> {
        if !has_git_marker(directory) {
            return Ok(None);
        }
        let root = run_git(directory, &["rev-parse", "--show-toplevel"])?;
        if !root.status.success() {
            return Ok(None);
        }
        let root = stdout_line(&root, "Git work-tree root")?;
        let root = fs::canonicalize(&root)
            .with_context(|| format!("canonicalizing Git work-tree root `{root}`"))?;
        let scan_from = fs::canonicalize(scan_from)
            .with_context(|| format!("canonicalizing Git scan root {}", scan_from.display()))?;
        ensure!(
            scan_from.starts_with(&root),
            "Git scan root {} is outside work tree {}",
            scan_from.display(),
            root.display()
        );
        Ok(Some(Self { root, scan_from }))
    }
}

fn require_clean(root: &Path, path: &str, cached: bool) -> Result<()> {
    let mut args = vec!["diff"];
    if cached {
        args.push("--cached");
    }
    args.extend(["--quiet", "--", path]);
    let output = run_git(root, &args)?;
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

fn has_git_marker(directory: &Path) -> bool {
    let mut current = Some(directory);
    while let Some(path) = current {
        if path.join(".git").exists() {
            return true;
        }
        current = path.parent();
    }
    false
}

fn run_git(directory: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
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
        diagnostic_stderr(&output)
    );
    let value = std::str::from_utf8(&output.stdout)
        .with_context(|| format!("Git returned a non-UTF-8 {label}"))?;
    let value = value.trim_end_matches(|character| character == '\r' || character == '\n');
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
