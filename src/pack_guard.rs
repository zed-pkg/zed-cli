use std::fs;
use std::path::{Path, PathBuf};

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
    let Some(submodules) = crate::git_submodules::pack_submodules(project)? else {
        return Ok(0);
    };
    let views = artifact_views(project, manifest, submodules.canonical_root())?;
    let included = submodules
        .paths()
        .filter(|relative| {
            let module = submodules.canonical_root().join(relative);
            views.iter().any(|view| view.may_include(&module))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();

    submodules.verify(&included)?;
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;

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
        fs::create_dir_all(project.path().join("vendor/client")).unwrap();
        fs::write(
            project.path().join("vendor/client/.git"),
            "gitdir: ../../.git/modules/vendor/client\n",
        )
        .unwrap();
        fs::write(project.path().join("vendor/client/lib.txt"), "runtime\n").unwrap();
        fs::create_dir_all(project.path().join("vendor/other/.git/objects")).unwrap();
        fs::write(project.path().join("vendor/other/.git/HEAD"), "secret\n").unwrap();
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
}
