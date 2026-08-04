use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

const MAX_REPORTED_INPUTS: usize = 20;

/// Refuse to publish an untracked Git-ignored file unless the package rules
/// independently exclude it from every artifact. Git ignore state is a local
/// developer convenience, not a publication boundary; treating it as one can
/// silently leak credentials and machine-specific build outputs.
///
/// When the `git` executable is unavailable, the fallback intentionally treats
/// every ignore-matched regular file as potentially untracked. That preserves
/// the publication boundary in slim runtime images at the cost of a possible
/// false positive for a tracked file that also matches an ignore rule.
pub(crate) fn preflight_git_ignored(project: &Path, manifest: &Manifest) -> Result<usize> {
    let ignored = git_ignored_inputs(project)?;
    if ignored.paths.is_empty() {
        return Ok(0);
    }

    let views = artifact_views(project, manifest)?;
    let mut unsafe_inputs = Vec::new();

    for relative in &ignored.paths {
        let candidate = project.join(relative);
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading ignored package input {}", candidate.display())
                });
            }
        };
        // The packer includes regular files only. Symlinks, directories, and
        // other filesystem objects cannot enter the current artifact format.
        if !metadata.is_file() {
            continue;
        }

        let candidate = fs::canonicalize(&candidate).with_context(|| {
            format!(
                "canonicalizing ignored package input {}",
                candidate.display()
            )
        })?;
        let artifacts = views
            .iter()
            .filter(|view| view.includes(&candidate))
            .map(|view| view.label.clone())
            .collect::<Vec<_>>();
        if !artifacts.is_empty() {
            unsafe_inputs.push(UnsafeInput {
                path: relative.clone(),
                artifacts,
            });
        }
    }

    if !unsafe_inputs.is_empty() {
        let total = unsafe_inputs.len();
        let classification = if ignored.conservative {
            "Git-ignore-matched"
        } else {
            "untracked Git-ignored"
        };
        let mut details = String::new();
        for input in unsafe_inputs.iter().take(MAX_REPORTED_INPUTS) {
            details.push_str(&format!(
                "\n  - {} ({})",
                input.path.display(),
                input.artifacts.join(", ")
            ));
        }
        if total > MAX_REPORTED_INPUTS {
            details.push_str(&format!(
                "\n  - ... and {} more",
                total - MAX_REPORTED_INPUTS
            ));
        }
        let fallback_note = if ignored.conservative {
            "\nGit was unavailable, so Zed conservatively treated every ignore-matched file as potentially untracked. Install Git to preserve tracked-file exceptions."
        } else {
            ""
        };

        bail!(
            concat!(
                "refusing to pack {} {} file(s) that remain eligible for publication:{}{}\n",
                "Git ignore rules are not publication rules. Add explicit [publish].exclude entries, or a {} rule ",
                "for a whole-tree package, then retry."
            ),
            total,
            classification,
            details,
            fallback_note,
            IGNORE_FILE
        );
    }

    Ok(ignored.paths.len())
}

#[derive(Debug)]
struct UnsafeInput {
    path: PathBuf,
    artifacts: Vec<String>,
}

struct ArtifactView {
    label: String,
    source: PathBuf,
    excludes: GlobSet,
    always: GlobSet,
}

impl ArtifactView {
    fn new(
        label: String,
        source: PathBuf,
        manifest: &Manifest,
        include_source_ignore: bool,
    ) -> Result<Self> {
        let source = fs::canonicalize(&source)
            .with_context(|| format!("canonicalizing artifact source {}", source.display()))?;
        let mut extra = manifest.publish.exclude.clone();

        // Mirror pack_format's dynamic exclusions so the preflight evaluates
        // the exact final payload rather than the intermediate staging tree.
        let modules_dir = manifest.modules_dir().trim_matches('/').to_string();
        if !modules_dir.is_empty() {
            extra.push(format!("{modules_dir}/**"));
        }
        extra.push(format!("{}/**", crate::transaction::STAGING_DIR));
        if include_source_ignore {
            append_ignore_file(&source.join(IGNORE_FILE), &mut extra)?;
        }

        let excludes = effective_excludes(&extra, manifest.publish.include_readme);
        let always = ALWAYS_INCLUDE
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect::<Vec<_>>();

        Ok(Self {
            label,
            source,
            excludes: glob_set(&excludes)?,
            always: glob_set(&always)?,
        })
    }

    fn includes(&self, candidate: &Path) -> bool {
        let Ok(relative) = candidate.strip_prefix(&self.source) else {
            return false;
        };
        self.always.is_match(relative) || !self.excludes.is_match(relative)
    }
}

fn artifact_views(project: &Path, manifest: &Manifest) -> Result<Vec<ArtifactView>> {
    if !manifest.is_polyglot() {
        return Ok(vec![ArtifactView::new(
            "package artifact".to_string(),
            project.to_path_buf(),
            manifest,
            true,
        )?]);
    }

    let mut views = Vec::with_capacity(manifest.targets.len());
    for (target, section) in &manifest.targets {
        let derived = manifest
            .manifest_for_target(target)
            .with_context(|| format!("target `{target}` disappeared during package preflight"))?;
        // pack_all first copies each target into a staging directory. The
        // source target's .zedignore is intentionally not treated as active
        // here because it is not copied into that staging tree by copy_files.
        views.push(ArtifactView::new(
            format!("target `{target}` artifact"),
            project.join(&section.dir),
            &derived,
            false,
        )?);
    }
    Ok(views)
}

fn append_ignore_file(path: &Path, patterns: &mut Vec<String>) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    for line in fs::read_to_string(path)
        .with_context(|| format!("reading package ignore file {}", path.display()))?
        .lines()
    {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            patterns.push(line.to_string());
        }
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

#[derive(Debug)]
struct IgnoredInputs {
    paths: Vec<PathBuf>,
    conservative: bool,
}

fn git_ignored_command(project: &Path) -> Result<Command> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package worktree {}", project.display()))?;
    let mut command = Command::new("git");
    command
        // Containerized copies can retain host ownership and trigger Git's
        // dubious-ownership protection. Trust only this exact canonical tree
        // for this read-only process; never mutate user or repository config.
        .arg("-c")
        .arg(format!("safe.directory={}", project.display()))
        .arg("-C")
        .arg(&project)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0");
    Ok(command)
}

fn git_ignored_inputs(project: &Path) -> Result<IgnoredInputs> {
    let output = match git_ignored_command(project)?.output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(IgnoredInputs {
                paths: fallback_ignored_paths(project)?,
                conservative: true,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "enumerating ignored package inputs in {}",
                    project.display()
                )
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") || stderr.contains("not inside a work tree") {
            return Ok(IgnoredInputs {
                paths: fallback_ignored_paths(project)?,
                conservative: true,
            });
        }
        bail!(
            "git failed while enumerating ignored package inputs in {}: {}",
            project.display(),
            stderr.trim()
        );
    }

    let mut paths = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(raw).context(
            "git returned a non-UTF-8 ignored path; refusing to guess whether it belongs in the package",
        )?;
        paths.push(PathBuf::from(path));
    }
    paths.sort();
    paths.dedup();
    Ok(IgnoredInputs {
        paths,
        conservative: false,
    })
}

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

fn fallback_ignored_paths(project: &Path) -> Result<Vec<PathBuf>> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package worktree {}", project.display()))?;
    let rules = fallback_ignore_rules(&project)?;
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
        let relative = entry.path().strip_prefix(&project).with_context(|| {
            format!(
                "resolving package input {} relative to {}",
                entry.path().display(),
                project.display()
            )
        })?;
        if path_is_ignored(relative, &rules)? {
            paths.push(relative.to_path_buf());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn fallback_ignore_rules(project: &Path) -> Result<Vec<IgnoreRule>> {
    let mut rules = Vec::new();

    if let Some(global) = default_global_ignore_file() {
        if global.is_file() {
            append_git_ignore_rules(&global, Path::new(""), &mut rules)?;
        }
    }

    if let Some(git_dir) = git_dir(project)? {
        let info_exclude = git_dir.join("info/exclude");
        if info_exclude.is_file() {
            append_git_ignore_rules(&info_exclude, Path::new(""), &mut rules)?;
        }
    }

    let mut ignore_files = Vec::new();
    for entry in WalkDir::new(project)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry =
            entry.with_context(|| format!("discovering .gitignore files in {}", project.display()))?;
        if entry.file_type().is_file() && entry.file_name() == OsStr::new(".gitignore") {
            ignore_files.push(entry.path().to_path_buf());
        }
    }
    ignore_files.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });

    for path in ignore_files {
        let parent = path.parent().unwrap_or(project);
        let base = parent.strip_prefix(project).with_context(|| {
            format!(
                "resolving ignore file {} relative to {}",
                path.display(),
                project.display()
            )
        })?;
        append_git_ignore_rules(&path, base, &mut rules)?;
    }
    Ok(rules)
}

fn default_global_ignore_file() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("git/ignore"));
    }
    dirs::home_dir().map(|home| home.join(".config/git/ignore"))
}

fn git_dir(project: &Path) -> Result<Option<PathBuf>> {
    let marker = project.join(".git");
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
        project.join(path)
    }))
}

fn append_git_ignore_rules(
    path: &Path,
    base: &Path,
    rules: &mut Vec<IgnoreRule>,
) -> Result<()> {
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
                    .with_context(|| {
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
    for component in components
        .iter()
        .take(components.len().saturating_sub(1))
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"[package]
org = "acme"
name = "pack-inputs"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/pack-inputs.git"

{extra}
"#
        ))
        .unwrap()
    }

    #[cfg(unix)]
    fn git(project: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Zed Pack Inputs")
            .env("GIT_AUTHOR_EMAIL", "zed-pack-inputs@example.invalid")
            .env("GIT_COMMITTER_NAME", "Zed Pack Inputs")
            .env("GIT_COMMITTER_EMAIL", "zed-pack-inputs@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_query_trusts_only_the_canonical_project_path() {
        let project = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(project.path()).unwrap();
        let command = git_ignored_command(project.path()).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args.first().map(String::as_str), Some("-c"));
        assert_eq!(
            args.get(1),
            Some(&format!("safe.directory={}", canonical.display()))
        );
        assert_eq!(args.get(2).map(String::as_str), Some("-C"));
        assert_eq!(args.get(3), Some(&canonical.to_string_lossy().into_owned()));
        assert!(!args.iter().any(|arg| arg == "safe.directory=*"));
    }

    #[test]
    fn gitless_fallback_honors_nested_rules_and_negation() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("nested/cache")).unwrap();
        fs::write(
            project.path().join(".gitignore"),
            "*.env\n!important.env\n",
        )
        .unwrap();
        fs::write(
            project.path().join("nested/.gitignore"),
            "cache/\nvisible.tmp\n",
        )
        .unwrap();
        fs::write(project.path().join("secret.env"), "secret\n").unwrap();
        fs::write(project.path().join("important.env"), "tracked intent\n").unwrap();
        fs::write(project.path().join("nested/cache/value.txt"), "cache\n").unwrap();
        fs::write(project.path().join("nested/visible.tmp"), "ignored\n").unwrap();

        let paths = fallback_ignored_paths(project.path()).unwrap();
        assert!(paths.contains(&PathBuf::from("secret.env")));
        assert!(paths.contains(&PathBuf::from("nested/cache/value.txt")));
        assert!(paths.contains(&PathBuf::from("nested/visible.tmp")));
        assert!(!paths.contains(&PathBuf::from("important.env")));
    }

    #[test]
    fn gitless_fallback_does_not_reinclude_a_child_of_ignored_parent() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("cache")).unwrap();
        fs::write(
            project.path().join(".gitignore"),
            "cache/\n!cache/value.txt\n",
        )
        .unwrap();
        fs::write(project.path().join("cache/value.txt"), "still ignored\n").unwrap();

        let paths = fallback_ignored_paths(project.path()).unwrap();
        assert_eq!(paths, vec![PathBuf::from("cache/value.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn ignored_untracked_input_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
        fs::write(project.path().join("secret.env"), "TOKEN=do-not-publish\n").unwrap();
        fs::write(project.path().join("public.txt"), "safe\n").unwrap();

        let error = preflight_git_ignored(project.path(), &manifest("")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("secret.env"), "{message}");
        assert!(message.contains("package artifact"), "{message}");
        assert!(message.contains("Git ignore rules are not publication rules"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_package_ignore_allows_ignored_input() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
        fs::write(project.path().join(IGNORE_FILE), "secret.env\n").unwrap();
        fs::write(project.path().join("secret.env"), "TOKEN=local-only\n").unwrap();

        assert_eq!(
            preflight_git_ignored(project.path(), &manifest("")).unwrap(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn publish_exclusion_allows_ignored_input() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::write(project.path().join(".gitignore"), "secret.env\n").unwrap();
        fs::write(project.path().join("secret.env"), "TOKEN=local-only\n").unwrap();

        let manifest = manifest(
            r#"[publish]
exclude = ["secret.env"]
"#,
        );
        assert_eq!(preflight_git_ignored(project.path(), &manifest).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn polyglot_target_checks_paths_relative_to_its_source_root() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::create_dir_all(project.path().join("clients/ts")).unwrap();
        fs::write(
            project.path().join(".gitignore"),
            "clients/ts/private.key\n",
        )
        .unwrap();
        fs::write(project.path().join("clients/ts/private.key"), "private\n").unwrap();

        let error = preflight_git_ignored(
            project.path(),
            &manifest(
                r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
            ),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("clients/ts/private.key"), "{message}");
        assert!(message.contains("target `nodejs` artifact"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn polyglot_source_ignore_is_not_mistaken_for_pack_exclusion() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::create_dir_all(project.path().join("clients/ts")).unwrap();
        fs::write(
            project.path().join(".gitignore"),
            "clients/ts/private.key\n",
        )
        .unwrap();
        fs::write(
            project.path().join("clients/ts/.zedignore"),
            "private.key\n",
        )
        .unwrap();
        fs::write(project.path().join("clients/ts/private.key"), "private\n").unwrap();

        let error = preflight_git_ignored(
            project.path(),
            &manifest(
                r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
            ),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("clients/ts/private.key"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn ignored_input_outside_all_polyglot_targets_is_safe() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::create_dir_all(project.path().join("clients/ts")).unwrap();
        fs::create_dir_all(project.path().join("scratch")).unwrap();
        fs::write(project.path().join(".gitignore"), "scratch/private.key\n").unwrap();
        fs::write(project.path().join("scratch/private.key"), "private\n").unwrap();

        let manifest = manifest(
            r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        );
        assert_eq!(preflight_git_ignored(project.path(), &manifest).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn tracked_file_matching_gitignore_is_not_treated_as_local_input() {
        let project = tempfile::tempdir().unwrap();
        git(project.path(), &["init"]);
        fs::write(project.path().join("tracked.env"), "published=true\n").unwrap();
        git(project.path(), &["add", "--", "tracked.env"]);
        fs::write(project.path().join(".gitignore"), "tracked.env\n").unwrap();

        assert_eq!(
            preflight_git_ignored(project.path(), &manifest("")).unwrap(),
            0
        );
    }

    #[test]
    fn non_git_source_tree_is_not_forced_to_have_git() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("payload.txt"), "runtime\n").unwrap();
        assert_eq!(
            preflight_git_ignored(project.path(), &manifest("")).unwrap(),
            0
        );
    }
}
