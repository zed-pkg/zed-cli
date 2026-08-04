use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

const MAX_REPORTED_INPUTS: usize = 20;

/// Refuse to publish an untracked Git-ignored file unless the package rules
/// independently exclude it from every artifact. Git ignore state is a local
/// developer convenience, not a publication boundary; treating it as one can
/// silently leak credentials and machine-specific build outputs.
pub(crate) fn preflight_git_ignored(project: &Path, manifest: &Manifest) -> Result<usize> {
    let ignored = git_ignored_untracked(project)?;
    if ignored.is_empty() {
        return Ok(0);
    }

    let views = artifact_views(project, manifest)?;
    let mut unsafe_inputs = Vec::new();

    for relative in &ignored {
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

        bail!(
            concat!(
                "refusing to pack {} untracked Git-ignored file(s) that remain eligible for publication:{}\n",
                "Git ignore rules are not publication rules. Add explicit [publish].exclude entries, or a {} rule ",
                "for a whole-tree package, then retry."
            ),
            total,
            details,
            IGNORE_FILE
        );
    }

    Ok(ignored.len())
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

fn git_ignored_untracked(project: &Path) -> Result<Vec<PathBuf>> {
    let output = match git_ignored_command(project)?.output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if looks_like_git_worktree(project) {
                bail!(
                    "git is required to verify ignored package inputs for {}; install git or run pack from a source tree without Git metadata",
                    project.display()
                );
            }
            return Ok(Vec::new());
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
            return Ok(Vec::new());
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
    Ok(paths)
}

fn looks_like_git_worktree(project: &Path) -> bool {
    project
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
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
