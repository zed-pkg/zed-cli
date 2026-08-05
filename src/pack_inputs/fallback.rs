use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

struct IgnoreRule {
    base: PathBuf,
    matchers: GlobSet,
    negated: bool,
    directory_only: bool,
}

impl IgnoreRule {
    fn matches(&self, relative: &Path, is_dir: bool) -> Result<bool> {
        if self.directory_only && !is_dir {
            return Ok(false);
        }
        let Ok(subject) = relative.strip_prefix(&self.base) else {
            return Ok(false);
        };
        if subject.as_os_str().is_empty() {
            return Ok(false);
        }
        let subject = slash_path(subject)?;
        Ok(self.matchers.is_match(Path::new(&subject)))
    }
}

pub(super) fn fallback_ignored_paths(project: &Path) -> Result<Vec<PathBuf>> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package worktree {}", project.display()))?;
    let worktree = find_worktree_root(&project);
    let rules = fallback_ignore_rules(&worktree, &project)?;
    if rules.is_empty() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in WalkDir::new(&project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry =
            entry.with_context(|| format!("walking package worktree {}", project.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let worktree_relative = entry.path().strip_prefix(&worktree).with_context(|| {
            format!(
                "resolving package input {} relative to {}",
                entry.path().display(),
                worktree.display()
            )
        })?;
        if path_is_ignored(worktree_relative, &rules)? {
            paths.push(
                entry
                    .path()
                    .strip_prefix(&project)
                    .with_context(|| format!("resolving package path {}", entry.path().display()))?
                    .to_path_buf(),
            );
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn find_worktree_root(project: &Path) -> PathBuf {
    project
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(project)
        .to_path_buf()
}

fn fallback_ignore_rules(worktree: &Path, project: &Path) -> Result<Vec<IgnoreRule>> {
    let mut rules = Vec::new();

    if let Some(global) = default_global_ignore_file().filter(|path| path.is_file()) {
        append_git_ignore_rules(&global, Path::new(""), &mut rules)?;
    }

    if let Some(info_exclude) = git_info_exclude(worktree)? {
        if info_exclude.is_file() {
            append_git_ignore_rules(&info_exclude, Path::new(""), &mut rules)?;
        }
    }

    let mut ignore_files = ancestor_ignore_files(worktree, project)?;
    for entry in WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry = entry
            .with_context(|| format!("discovering .gitignore files in {}", project.display()))?;
        if entry.file_type().is_file() && entry.file_name() == OsStr::new(".gitignore") {
            ignore_files.push(entry.path().to_path_buf());
        }
    }
    dedupe_paths_in_order(&mut ignore_files);

    for path in ignore_files {
        let parent = path.parent().unwrap_or(worktree);
        let base = parent.strip_prefix(worktree).with_context(|| {
            format!(
                "resolving ignore file {} relative to {}",
                path.display(),
                worktree.display()
            )
        })?;
        append_git_ignore_rules(&path, base, &mut rules)?;
    }
    Ok(rules)
}

fn ancestor_ignore_files(worktree: &Path, project: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let mut current = Some(project);
    while let Some(directory) = current {
        directories.push(directory.to_path_buf());
        if directory == worktree {
            break;
        }
        current = directory.parent();
    }
    directories.reverse();

    let mut paths = Vec::new();
    for directory in directories {
        let ignore = directory.join(".gitignore");
        if ignore.is_file() {
            paths.push(ignore);
        }
    }
    Ok(paths)
}

fn dedupe_paths_in_order(paths: &mut Vec<PathBuf>) {
    let mut unique = Vec::capacity(paths.len());
    for path in paths.drain(..) {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    *paths = unique;
}

fn default_global_ignore_file() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("git/ignore"));
    }
    dirs::home_dir().map(|home| home.join(".config/git/ignore"))
}

fn git_info_exclude(worktree: &Path) -> Result<Option<PathBuf>> {
    let Some(git_dir) = git_dir(worktree)? else {
        return Ok(None);
    };
    let commondir = git_dir.join("commondir");
    let common_git_dir = if commondir.is_file() {
        let value = fs::read_to_string(&commondir)
            .with_context(|| format!("reading linked worktree commondir {}", commondir.display()))?;
        let path = PathBuf::from(value.trim());
        if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        }
    } else {
        git_dir
    };
    Ok(Some(common_git_dir.join("info/exclude")))
}

fn git_dir(worktree: &Path) -> Result<Option<PathBuf>> {
    let marker = worktree.join(".git");
    if marker.is_dir() {
        return Ok(Some(marker));
    }
    if !marker.is_file() {
        return Ok(None);
    }

    let text = fs::read_to_string(&marker)
        .with_context(|| format!("reading Git directory pointer {}", marker.display()))?;
    let value = text
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .context("invalid .git file: expected `gitdir: <path>`")?;
    let path = PathBuf::from(value);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    }))
}

fn append_git_ignore_rules(path: &Path, base: &Path, rules: &mut Vec<IgnoreRule>) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading Git ignore rules {}", path.display()))?;
    for (index, raw) in text.lines().enumerate() {
        let Some(parsed) = parse_git_ignore_rule(raw) else {
            continue;
        };
        let mut builder = GlobSetBuilder::new();
        for pattern in &parsed.patterns {
            builder.add(
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .with_context(||  {
                        format!(
                            "invalid Git ignore pattern `{}` in {}:{}",
                            pattern,
                            path.display(),
                            index + 1
                        )
                    })?,
            );
        }
        rules.push(IgnoreRule {
            base: base.to_path_buf(),
            matchers: builder.build().with_context(|| {
                format!(
                    "compiling Git ignore pattern in {}:{}",
                    path.display(),
                    index + 1
                )
            })?,
            negated: parsed.negated,
            directory_only: parsed.directory_only,
        });
    }
    Ok(())
}

struct ParsedIgnoreRule {
    patterns: Vec<String>,
    negated: bool,
    directory_only: bool,
}

fn parse_git_ignore_rule(raw: &str) -> Option<ParsedIgnoreRule> {
    let mut line = trim_unescaped_trailing_spaces(raw.trim_end_matches('\r'));
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let mut negated = false;
    if line.starts_with("\\#") || line.starts_with("\\!") {
        line = &line[1..];
    } else if let Some(rest) = line.strip_prefix('!') {
        negated = true;
        line = rest;
    }

    let directory_only = line.ends_with('/');
    if directory_only {
        line = line.trim_end_matches('/');
    }
    let anchored = line.starts_with('/');
    if anchored {
        line = line.trim_start_matches('/');
    }
    if line.is_empty() {
        return None;
    }

    let mut patterns = vec![line.to_string()];
    if !anchored && !line.contains('/') {
        patterns.push(format!("**/{line}"));
    }
    patterns.sort();
    patterns.dedup();
    Some(ParsedIgnoreRule {
        patterns,
        negated,
        directory_only,
    })
}

fn trim_unescaped_trailing_spaces(mut line: &str) -> &str {
    while line.ends_with(' ') {
        let bytes = line.as_bytes();
        let mut slash_count = 0;
        let mut cursor = bytes.len().saturating_sub(1);
        while cursor > 0 && bytes[cursor - 1] == b'\\' {
            slash_count += 1;
            cursor -= 1;
        }
        if slash_count % 2 == 1 {
            break;
        }
        line = &line[..line.len() - 1];
    }
    line
}

fn path_is_ignored(relative: &Path, rules: &[IgnoreRule]) -> Result<bool> {
    let components = relative.components().collect::<Vec<_>>();
    let mut parent = PathBuf::new();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        parent.push(component.as_os_str());
        if ignored_at_path(&parent, true, rules)? {
            return Ok(true);
        }
    }
    ignored_at_path(relative, false, rules)
}

fn ignored_at_path(relative: &Path, is_dir: bool, rules: &[IgnoreRule]) -> Result<bool> {
    let mut ignored = false;
    for rule in rules {
        if rule.matches(relative, is_dir)? {
            ignored = !rule.negated;
        }
    }
    Ok(ignored)
}

fn slash_path(path: &Path) -> Result<String> {
    let path = path.to_str().with_context(|| {
        format!(
            "non-UTF-8 package path {}; refusing to approximate Git ignore semantics",
            path.display()
        )
    })?;
    Ok(path.replace(std::path::MAIN_SEPARATOR, "/"))
}
