//! Public installer facade.
//!
//! Graph solving and artifact acquisition happen before the implementation's
//! project transaction. Non-frozen installs expose the solver's exact registry
//! selections only to the root consumer manifest, so the established installer
//! writes the lockfile, adapters, and materialization from that one graph rather
//! than independently making greedy choices. Frozen replay remains lock-driven.
//!
//! Adopted Git submodules are verified before mutation and recorded through an
//! additive lock extension after the ordinary install transaction. Older lock
//! readers ignore that extension; this facade keeps it exact across install,
//! add, remove, and frozen replay.
//!
//! Dart's provisional adapter fragment is finalized here after materialization,
//! replacing Zed directory-derived keys with the native package identities from
//! each dependency's `pubspec.yaml`. The hook is a no-op for every other
//! adapter and is shared by normal and manifestless frozen installs.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};

const MAX_REPORTED_PUBLISH_IGNORE_CONFLICTS: usize = 20;

#[derive(Debug)]
pub(crate) struct GitLockFinalizeError;

impl std::fmt::Display for GitLockFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("finalizing adopted Git submodule lock metadata")
    }
}

impl std::error::Error for GitLockFinalizeError {}

#[path = "ops.rs"]
mod implementation;

pub use implementation::{
    InstallOutcome, WorkspaceInfo, build_cmd, build_publish_meta, cache_clean, find, gc, login,
    org_audit, org_claim, run, split_key, store_prune, store_status, yank,
};

pub(crate) use implementation::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

#[cfg(test)]
pub(crate) use implementation::legacy_ensure_artifact_for_test;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishIgnoreConflict {
    path_family: String,
    manifest_rule: String,
    ignore_rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishIgnoreAnalysis {
    manifest_rule_count: usize,
    ignore_rule_count: usize,
    ordered_rules: Vec<String>,
    conflicts: Vec<PublishIgnoreConflict>,
}

fn parse_zedignore_rules(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn read_zedignore_rules(project: &Path) -> Result<Vec<String>> {
    let path = project.join(IGNORE_FILE);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(parse_zedignore_rules(&contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error)
            .with_context(|| format!("reading package ignore file {}", path.display())),
    }
}

fn normalize_publish_ignore_rule(rule: &str) -> Option<(bool, String)> {
    let rule = rule.trim();
    let (excludes, body) = match rule.strip_prefix('!') {
        Some(body) => (false, body),
        None => (true, rule),
    };
    let mut path_family = body.trim().replace('\\', "/");
    path_family = path_family
        .strip_prefix("**/")
        .unwrap_or(&path_family)
        .to_owned();
    while let Some(stripped) = path_family.strip_suffix("/**") {
        path_family = stripped.to_owned();
    }
    let path_family = path_family.trim_matches('/').to_ascii_lowercase();
    (!path_family.is_empty()).then_some((excludes, path_family))
}

fn analyze_publish_ignore_rules(
    manifest_rules: &[String],
    ignore_rules: &[String],
) -> PublishIgnoreAnalysis {
    let ordered_rules = manifest_rules
        .iter()
        .chain(ignore_rules)
        .cloned()
        .collect::<Vec<_>>();
    let mut conflicts = Vec::new();

    for manifest_rule in manifest_rules {
        let Some((manifest_excludes, manifest_family)) =
            normalize_publish_ignore_rule(manifest_rule)
        else {
            continue;
        };
        for ignore_rule in ignore_rules {
            let Some((ignore_excludes, ignore_family)) = normalize_publish_ignore_rule(ignore_rule)
            else {
                continue;
            };
            if manifest_family == ignore_family && manifest_excludes != ignore_excludes {
                conflicts.push(PublishIgnoreConflict {
                    path_family: manifest_family.clone(),
                    manifest_rule: manifest_rule.clone(),
                    ignore_rule: ignore_rule.clone(),
                });
            }
        }
    }

    conflicts.sort_by(|left, right| {
        (
            &left.path_family,
            &left.manifest_rule,
            &left.ignore_rule,
        )
            .cmp(&(
                &right.path_family,
                &right.manifest_rule,
                &right.ignore_rule,
            ))
    });
    conflicts.dedup();

    PublishIgnoreAnalysis {
        manifest_rule_count: manifest_rules.len(),
        ignore_rule_count: ignore_rules.len(),
        ordered_rules,
        conflicts,
    }
}

fn warn_publish_ignore_sources(project: &Path, manifest: &Manifest) -> Result<()> {
    let ignore_rules = read_zedignore_rules(project)?;
    if manifest.publish.exclude.is_empty() || ignore_rules.is_empty() {
        return Ok(());
    }

    let analysis = analyze_publish_ignore_rules(&manifest.publish.exclude, &ignore_rules);
    eprintln!(
        "warning: both .zpkg.toml [publish].exclude ({} rule(s)) and {} ({} rule(s)) are defined; zed-pkg applies their ordered union (manifest first, {} second; {} total rule(s))",
        analysis.manifest_rule_count,
        IGNORE_FILE,
        analysis.ignore_rule_count,
        IGNORE_FILE,
        analysis.ordered_rules.len()
    );

    for conflict in analysis
        .conflicts
        .iter()
        .take(MAX_REPORTED_PUBLISH_IGNORE_CONFLICTS)
    {
        eprintln!(
            "warning: conflicting publish-ignore rules for `{}`: .zpkg.toml has `{}` and {} has `{}`; the later {} rule wins for this path family",
            conflict.path_family,
            conflict.manifest_rule,
            IGNORE_FILE,
            conflict.ignore_rule,
            IGNORE_FILE
        );
    }
    if analysis.conflicts.len() > MAX_REPORTED_PUBLISH_IGNORE_CONFLICTS {
        eprintln!(
            "warning: ... and {} more publish-ignore conflict(s)",
            analysis.conflicts.len() - MAX_REPORTED_PUBLISH_IGNORE_CONFLICTS
        );
    }
    Ok(())
}

fn with_pack_guard<T>(project: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let manifest = config::read_manifest(project)?;
    warn_publish_ignore_sources(project, &manifest)?;
    let manifest = crate::pack_guard::harden_manifest(manifest);
    let manifest = crate::pack_inputs::harden_manifest(manifest);
    crate::pack_guard::preflight_submodules(project, &manifest)?;
    crate::pack_inputs::preflight_git_ignored(project, &manifest)?;
    let manifest_text = manifest.to_toml_string()?;
    config::with_manifest_override(project, manifest_text, action)
}

pub fn init(
    project: &Path,
    org: Option<String>,
    name: Option<String>,
    interactive_mode: bool,
) -> Result<()> {
    crate::project_lock::with_lock(project, "initialize Zed package", || {
        implementation::init(project, org, name, interactive_mode)
    })
}

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<Vec<crate::pack::PackagedTarget>> {
    with_pack_guard(project, || implementation::pack_cmd(project, out))
}

pub fn publish(
    project: &Path,
    cfg: &Config,
    dry_run: bool,
    allow_dirty: bool,
    skip_vcs_checks: bool,
) -> Result<()> {
    with_pack_guard(project, || {
        implementation::publish(project, cfg, dry_run, allow_dirty, skip_vcs_checks)
    })
}

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    crate::project_lock::with_lock(project, "add Zed dependency", || {
        crate::git_submodules::preflight_gitmodules_metadata(project)?;
        crate::git_submodules::preflight_mutation(project)?;
        crate::config::with_install_prefetch(cfg, || implementation::add(project, cfg, spec))?;
        crate::git_submodules::refresh_lock_extensions(project)
    })
}

pub fn remove(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    crate::project_lock::with_lock(project, "remove Zed dependency", || {
        crate::git_submodules::preflight_gitmodules_metadata(project)?;
        crate::git_submodules::preflight_mutation(project)?;
        crate::config::with_install_prefetch(cfg, || implementation::remove(project, cfg, spec))?;
        crate::git_submodules::refresh_lock_extensions(project)
    })
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    let operation = if frozen {
        "restore frozen Zed dependency graph"
    } else {
        "install recursive Zed dependency graph"
    };
    crate::project_lock::with_lock(project, operation, || {
        crate::git_submodules::preflight_gitmodules_metadata(project)?;
        let git_lock = crate::git_submodules::prepare_install(project, frozen)?;
        let outcome = if frozen {
            crate::install_graph::prefetch(project, cfg, true)?;
            implementation::install(
                project,
                cfg,
                true,
                mode,
                adapter,
                allow_build,
                target,
                allow_ecosystem_mismatch,
            )?
        } else {
            let prepared = crate::install_graph::prepare(project, cfg)?;
            config::with_resolved_requirements(project, prepared.exact_requirements(), || {
                implementation::install(
                    project,
                    cfg,
                    false,
                    mode,
                    adapter,
                    allow_build,
                    target,
                    allow_ecosystem_mismatch,
                )
            })?
        };
        crate::dart_wiring::rewrite_if_present(project)
            .context("finalizing Dart package-manager wiring")?;
        git_lock.finish(project).context(GitLockFinalizeError)?;
        Ok(outcome)
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_frozen_lock_only(
    project: &Path,
    cfg: &Config,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    crate::project_lock::with_lock(project, "restore manifestless frozen Zed graph", || {
        crate::git_submodules::preflight_gitmodules_metadata(project)?;
        let git_lock = crate::git_submodules::prepare_install(project, true)?;
        crate::install_graph::prefetch(project, cfg, true)?;
        let outcome = implementation::install_frozen_lock_only(
            project,
            cfg,
            mode,
            adapter,
            allow_build,
            target,
            allow_ecosystem_mismatch,
        )?;
        crate::dart_wiring::rewrite_if_present(project)
            .context("finalizing Dart package-manager wiring")?;
        git_lock.finish(project).context(GitLockFinalizeError)?;
        Ok(outcome)
    })
}

pub fn uninstall(project: &Path, cfg: &Config, specs: &[String]) -> Result<()> {
    crate::project_lock::with_lock(project, "uninstall Zed dependency graph", || {
        implementation::uninstall(project, cfg, specs)
    })
}

#[cfg(test)]
mod publish_ignore_tests {
    use zed_interfaces::excludes::effective_excludes;

    use super::*;

    fn rules(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn ordered_union_keeps_manifest_rules_before_zedignore_rules() {
        let analysis = analyze_publish_ignore_rules(
            &rules(&[".env*", ".idea/**"]),
            &rules(&[".cache/**", ".local/**"]),
        );

        assert_eq!(
            analysis.ordered_rules,
            rules(&[".env*", ".idea/**", ".cache/**", ".local/**"])
        );
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn later_zedignore_negation_is_reported_and_wins() {
        let analysis =
            analyze_publish_ignore_rules(&rules(&["target/**"]), &rules(&["!target"]));

        assert_eq!(
            analysis.conflicts,
            vec![PublishIgnoreConflict {
                path_family: "target".to_owned(),
                manifest_rule: "target/**".to_owned(),
                ignore_rule: "!target".to_owned(),
            }]
        );
        let effective = effective_excludes(&analysis.ordered_rules, false);
        assert!(!effective.iter().any(|pattern| pattern == "target/**"));
        assert!(!effective.iter().any(|pattern| pattern == "**/target/**"));
    }

    #[test]
    fn later_zedignore_exclusion_can_reapply_after_manifest_negation() {
        let analysis =
            analyze_publish_ignore_rules(&rules(&["!target"]), &rules(&["target/**"]));

        assert_eq!(analysis.conflicts.len(), 1);
        let effective = effective_excludes(&analysis.ordered_rules, false);
        assert!(effective.iter().any(|pattern| pattern == "target/**"));
    }

    #[test]
    fn same_polarity_duplicates_are_not_contradictions() {
        let analysis = analyze_publish_ignore_rules(
            &rules(&[".cache/**", ".idea/**"]),
            &rules(&["**/.CACHE/**", ".idea/"]),
        );

        assert!(analysis.conflicts.is_empty());
        assert_eq!(analysis.ordered_rules.len(), 4);
    }

    #[test]
    fn parser_ignores_comments_blank_lines_and_surrounding_whitespace() {
        assert_eq!(
            parse_zedignore_rules("\n# local-only files\n  .env*  \n\r\n.idea/**\r\n"),
            rules(&[".env*", ".idea/**"])
        );
    }
}
