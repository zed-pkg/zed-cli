use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

mod git;

#[cfg(test)]
mod tests;

/// Explicit opt-in for Git-ignored, untracked publication inputs. Patterns are
/// project-relative Zed globs and are accepted only from a tracked, clean file.
pub(crate) const IGNORED_INPUT_ALLOW_FILE: &str = ".zedinclude";

pub(super) const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const DIAGNOSTIC_PATH_LIMIT: usize = 20;

/// Keep publication-control metadata out of package payloads. `.zedignore` is
/// already a shared default exclusion; `.zedinclude` is added in memory so old
/// manifests do not need to change.
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

/// Refuse to package Git-ignored, untracked regular files unless they are
/// excluded or deliberately admitted by a tracked, clean `.zedinclude`.
/// Initialized nested Git work trees are scanned too, including submodules.
///
/// The returned count is the number of ignored files intentionally admitted by
/// the allowlist. Non-Git package directories retain manifest/`.zedignore`
/// behavior and return zero.
pub(crate) fn preflight_ignored_inputs(project: &Path, manifest: &Manifest) -> Result<usize> {
    let project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package root {}", project.display()))?;
    let contexts = git::discover(&project)?;
    let Some(primary) = contexts.first() else {
        return Ok(0);
    };
    ensure!(
        project.starts_with(&primary.root),
        "package root {} is outside Git work tree {}",
        project.display(),
        primary.root.display()
    );

    let views = artifact_views(&project, manifest)?;
    let allow_patterns = git::ignored_input_patterns(&project, primary)?;
    let allowed = compile_globs(&allow_patterns)?;
    let mut admitted = BTreeSet::new();
    let mut rejected = BTreeSet::new();

    for context in &contexts {
        for absolute in git::ignored_files(context)? {
            let Ok(project_relative) = absolute.strip_prefix(&project) else {
                continue;
            };
            if project_relative.as_os_str().is_empty() {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&absolute) else {
                // A concurrent deletion cannot enter the later WalkDir payload.
                continue;
            };
            if !metadata.file_type().is_file() {
                // The packer does not follow or publish symlinks/directories.
                continue;
            }
            if !views.iter().any(|view| view.includes(&absolute))
                && !(manifest.is_polyglot() && is_root_legal_file(&project, &absolute))
            {
                continue;
            }

            let normalized = normalize_path(project_relative);
            if allowed.is_match(project_relative) {
                admitted.insert(normalized);
            } else {
                rejected.insert(normalized);
            }
        }
    }

    if !rejected.is_empty() {
        let shown = rejected
            .iter()
            .take(DIAGNOSTIC_PATH_LIMIT)
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let remainder = rejected.len().saturating_sub(DIAGNOSTIC_PATH_LIMIT);
        let suffix = if remainder == 0 {
            String::new()
        } else {
            format!(" and {remainder} more")
        };
        bail!(
            "refusing to package {} Git-ignored, untracked file(s) eligible for publication: {shown}{suffix}. Git-ignored files commonly contain secrets or machine-local build output. Exclude them with `{IGNORE_FILE}`, intentionally admit narrow globs in a tracked and clean `{IGNORED_INPUT_ALLOW_FILE}`, or force-track exact release inputs with `git add -f`",
            rejected.len()
        );
    }

    Ok(admitted.len())
}

struct ArtifactView {
    source: PathBuf,
    excludes: GlobSet,
    always: GlobSet,
}

impl ArtifactView {
    fn includes(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.source) else {
            return false;
        };
        !relative.as_os_str().is_empty()
            && (self.always.is_match(relative) || !self.excludes.is_match(relative))
    }
}

fn artifact_views(project: &Path, manifest: &Manifest) -> Result<Vec<ArtifactView>> {
    if manifest.is_polyglot() {
        let excludes =
            effective_excludes(&manifest.publish.exclude, manifest.publish.include_readme);
        let mut views = Vec::with_capacity(manifest.targets.len());
        for (target, section) in &manifest.targets {
            let source = project.join(&section.dir);
            let source = fs::canonicalize(&source).with_context(|| {
                format!(
                    "canonicalizing target `{target}` source root {}",
                    source.display()
                )
            })?;
            ensure!(
                source.starts_with(project),
                "target `{target}` source root {} resolves outside package root {}",
                source.display(),
                project.display()
            );
            views.push(build_view(source, &excludes)?);
        }
        return Ok(views);
    }

    let mut extra = manifest.publish.exclude.clone();
    let modules_dir = manifest.modules_dir().trim_matches('/').to_string();
    if !modules_dir.is_empty() {
        extra.push(format!("{modules_dir}/**"));
    }
    extra.push(format!("{}/**", crate::transaction::STAGING_DIR));
    let ignore_file = project.join(IGNORE_FILE);
    if ignore_file.is_file() {
        for line in fs::read_to_string(&ignore_file)
            .with_context(|| format!("reading {}", ignore_file.display()))?
            .lines()
        {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                extra.push(line.to_string());
            }
        }
    }
    let excludes = effective_excludes(&extra, manifest.publish.include_readme);
    Ok(vec![build_view(project.to_path_buf(), &excludes)?])
}

fn build_view(source: PathBuf, excludes: &[String]) -> Result<ArtifactView> {
    let always = ALWAYS_INCLUDE
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect::<Vec<_>>();
    Ok(ArtifactView {
        source,
        excludes: compile_globs(excludes)?,
        always: compile_globs(&always)?,
    })
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid publication glob `{pattern}`"))?,
        );
    }
    Ok(builder.build()?)
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

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
