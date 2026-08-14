use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSetBuilder};
use zed_interfaces::manifest::Manifest;

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
            views.iter().any(|view| view.includes_submodule(&module))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();

    submodules.verify(&included)?;
    Ok(included.len())
}

struct ArtifactView {
    source: PathBuf,
    excludes: Vec<String>,
}

impl ArtifactView {
    fn includes_submodule(&self, module: &Path) -> bool {
        if self.source.starts_with(module) {
            // The artifact source root itself is inside this submodule. It
            // cannot be produced from an uninitialized checkout, even when a
            // broad source exclusion happens to match every current file.
            return true;
        }
        let Ok(relative) = module.strip_prefix(&self.source) else {
            return false;
        };
        !explicitly_excludes_tree(relative, &self.excludes)
    }
}

/// Return true only when one authored rule conclusively excludes the complete
/// submodule subtree under the exact glob syntax used by the packer. Arbitrary
/// probing or normalizing a non-canonical pattern is unsafe: either can claim a
/// subtree is absent while an unsampled runtime file remains eligible.
fn explicitly_excludes_tree(relative: &Path, patterns: &[String]) -> bool {
    let relative = relative
        .to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_ascii_lowercase();
    patterns.iter().any(|pattern| {
        let Some(prefix) = canonical_recursive_prefix(pattern) else {
            return false;
        };
        relative == prefix
            || relative
                .strip_prefix(&prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn canonical_recursive_prefix(pattern: &str) -> Option<String> {
    if pattern.is_empty()
        || pattern != pattern.trim()
        || pattern.contains('\\')
        || pattern.starts_with('/')
        || pattern.starts_with("./")
        || pattern.ends_with('/')
    {
        return None;
    }
    let prefix = pattern.strip_suffix("/**")?;
    if prefix.is_empty()
        || prefix.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment
                    .bytes()
                    .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
        })
    {
        return None;
    }
    Some(prefix.to_ascii_lowercase())
}

fn artifact_views(
    project: &Path,
    manifest: &Manifest,
    canonical_root: &Path,
) -> Result<Vec<ArtifactView>> {
    if manifest.is_polyglot() {
        let mut views = Vec::with_capacity(manifest.targets.len());
        for (target, section) in &manifest.targets {
            let derived = manifest.manifest_for_target(target).with_context(|| {
                format!("target `{target}` disappeared during package preflight")
            })?;
            let source = project.join(&section.dir);
            let ignore_rules = crate::publish_ignore::read_rules(&source)?;
            let excludes =
                crate::publish_ignore::effective_artifact_excludes(&derived, &ignore_rules);
            validate_globs(&excludes)?;
            let source = fs::canonicalize(&source).with_context(|| {
                format!(
                    "canonicalizing target `{target}` source root {}",
                    source.display()
                )
            })?;
            ensure_source_within_root(&source, canonical_root)?;
            views.push(ArtifactView { source, excludes });
        }
        return Ok(views);
    }

    let source = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing package source {}", project.display()))?;
    ensure_source_within_root(&source, canonical_root)?;

    let ignore_rules = crate::publish_ignore::read_rules(project)?;
    let excludes = crate::publish_ignore::effective_artifact_excludes(manifest, &ignore_rules);
    validate_globs(&excludes)?;

    Ok(vec![ArtifactView { source, excludes }])
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

fn validate_globs(patterns: &[String]) -> Result<()> {
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
    builder.build()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    #[cfg(unix)]
    use std::process::Command;

    use flate2::read::GzDecoder;
    use zed_interfaces::paths::MANIFEST_FILE;

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

    #[cfg(unix)]
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
    fn complete_tree_exclusion_requires_an_exact_recursive_rule() {
        let path = Path::new("vendor/client");
        assert!(explicitly_excludes_tree(
            path,
            &["vendor/client/**".to_string()]
        ));
        assert!(explicitly_excludes_tree(path, &["VENDOR/**".to_string()]));

        for pattern in [
            "vendor/client/*",
            "vendor/client/src/**",
            "/vendor/client/**",
            "./vendor/client/**",
            "vendor\\client/**",
            " vendor/client/**",
            "vendor/client/** ",
            "vendor/*/**",
            "vendor/**/client/**",
            "vendor/client/**/",
        ] {
            assert!(
                !explicitly_excludes_tree(path, &[pattern.to_string()]),
                "non-canonical or partial pattern unexpectedly excluded the tree: {pattern}"
            );
        }
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

        let error = preflight_submodules(root.path(), &manifest).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not initialized"), "{message}");

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

        let error = preflight_submodules(root.path(), &manifest).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("dirty"), "{message}");
    }
}
