use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

#[cfg(test)]
mod extended_tests;
mod fallback;
mod git;
#[cfg(test)]
mod tests;

use fallback::{fallback_ignored_paths, find_worktree_root};

pub(crate) const IGNORED_INPUT_ALLOW_FILE: &str = ".zedinclude";
pub(super) const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPORTED_INPUTS: usize = 20;

/// Keep publication-control metadata out of package payloads. The allowlist is
/// interpreted only after Git proves it is tracked and clean.
pub(crate) fn harden_manifest(mut manifest: Manifest) -> Manifest {
    for pattern in [IGNORED_INPUT_ALLOW_FILE, "**/.zedinclude"] {
        if !manifest
            .publish
            .exclude
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(pattern))
        {
            manifest.publish.exclude.push(pattern.to_string());
        }
    }
    manifest
}

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
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package root {}", project.display()))?;
    let ignored = git_ignored_inputs(&project)?;
    let allowed = allow_glob_set(&ignored.allow_patterns)?;
    if ignored.paths.is_empty() {
        return Ok(0);
    }

    let views = artifact_views(&project, manifest)?;
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
        let artifacts = artifact_labels(&project, &candidate, manifest, &views);
        if artifacts.is_empty() || allowed.is_match(relative) {
            continue;
        }
        unsafe_inputs.push(UnsafeInput {
            path: relative.clone(),
            artifacts,
        });
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
            "\nGit was unavailable, so Zed conservatively treated every ignore-matched file as potentially untracked. Install Git to preserve tracked-file exceptions and use a reviewed .zedinclude allowlist."
        } else {
            ""
        };

        bail!(
            concat!(
                "refusing to pack {} {} file(s) that remain eligible for publication:{}{}\n",
                "Git ignore rules are not publication rules. Add explicit [publish].exclude entries, add a {} rule for a whole-tree package, force-track exact release inputs, or narrowly admit generated inputs with a tracked and clean {} file."
            ),
            total,
            classification,
            details,
            fallback_note,
            IGNORE_FILE,
            IGNORED_INPUT_ALLOW_FILE
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
        self.includes_relative(relative)
    }

    fn includes_relative(&self, relative: &Path) -> bool {
        !relative.as_os_str().is_empty()
            && (self.always.is_match(relative) || !self.excludes.is_match(relative))
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

fn artifact_labels(
    project: &Path,
    candidate: &Path,
    manifest: &Manifest,
    views: &[ArtifactView],
) -> Vec<String> {
    let mut labels = views
        .iter()
        .filter(|view| view.includes(candidate))
        .map(|view| view.label.clone())
        .collect::<Vec<_>>();

    if !manifest.is_polyglot() || !is_root_legal_file(project, candidate) {
        return labels;
    }
    let Some(name) = candidate.file_name() else {
        return labels;
    };
    let relative = Path::new(name);
    for view in views {
        // A root target already includes the candidate through the ordinary
        // source walk. Other targets receive it through copy_root_legal_files,
        // unless their source already supplies a file with the same name.
        if view.source == project
            || view.source.join(name).exists()
            || !view.includes_relative(relative)
        {
            continue;
        }
        labels.push(format!("{} via root legal-file copy", view.label));
    }
    labels.sort();
    labels.dedup();
    labels
}

fn is_root_legal_file(project: &Path, path: &Path) -> bool {
    if path.parent() != Some(project) {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let upper = name.to_string_lossy().to_ascii_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "NOTICE"]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
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

fn allow_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(true)
                .build()
                .with_context(|| {
                    format!("invalid `{IGNORED_INPUT_ALLOW_FILE}` pattern `{pattern}`")
                })?,
        );
    }
    Ok(builder.build()?)
}

#[derive(Debug)]
struct IgnoredInputs {
    paths: Vec<PathBuf>,
    allow_patterns: Vec<String>,
    conservative: bool,
}

fn git_ignored_command(project: &Path) -> Result<Command> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package worktree {}", project.display()))?;
    let worktree = find_worktree_root(&project);
    let mut command = Command::new("git");
    command
        // Containerized copies can retain host ownership and trigger Git's
        // dubious-ownership protection. Trust only the exact canonical
        // worktree that owns this package for this read-only process; never
        // mutate user or repository config.
        .arg("-c")
        .arg(format!("safe.directory={}", worktree.display()))
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
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(command)
}

fn git_ignored_inputs(project: &Path) -> Result<IgnoredInputs> {
    let output = match git_ignored_command(project)?.output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(IgnoredInputs {
                paths: fallback_ignored_paths(project)?,
                allow_patterns: Vec::new(),
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

    ensure!(
        output.stdout.len() <= MAX_GIT_OUTPUT_BYTES && output.stderr.len() <= MAX_GIT_OUTPUT_BYTES,
        "Git output exceeded the {}-byte packaging safety limit",
        MAX_GIT_OUTPUT_BYTES
    );
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") || stderr.contains("not inside a work tree") {
            return Ok(IgnoredInputs {
                paths: fallback_ignored_paths(project)?,
                allow_patterns: Vec::new(),
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
    paths.extend(git::nested_ignored_paths(project)?);
    paths.sort();
    paths.dedup();
    Ok(IgnoredInputs {
        paths,
        allow_patterns: git::ignored_input_patterns(project)?,
        conservative: false,
    })
}
