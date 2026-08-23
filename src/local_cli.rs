//! Presentation layer for `zed local`.
//!
//! [`crate::local_registry`] owns the index and every decision it encodes.
//! This module only turns the typed command model into calls against it and
//! renders the result, so the same operations stay usable from library code
//! and integration tests without going through stdout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use zed_interfaces::version::Requirement;

use crate::cli::LocalCmd;
use crate::config::Config;
use crate::local_registry::{
    self, EntryStatus, LocalEntry, RegisterAction, Selector, parse_selector,
};

/// Machine-readable shape of one registration. Kept separate from the on-disk
/// [`LocalEntry`] so the index format and the reporting format can evolve
/// independently.
#[derive(Debug, Serialize)]
struct EntryReport {
    id: String,
    package: String,
    path: String,
    registered_version: String,
    priority: i64,
    enabled: bool,
    health: String,
    selectable: bool,
}

impl EntryReport {
    fn new(status: &EntryStatus) -> Self {
        Self {
            id: status.entry.id(),
            package: status.entry.key(),
            path: status.entry.path.clone(),
            registered_version: status.entry.version.clone(),
            priority: status.entry.priority,
            enabled: status.entry.enabled,
            health: status.health.label(),
            selectable: status.health.is_selectable(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ResolveReport {
    package: String,
    requirement: String,
    resolved: Option<ResolvedReport>,
    skipped: Vec<SkippedReport>,
}

#[derive(Debug, Serialize)]
struct ResolvedReport {
    path: String,
    version: String,
    priority: i64,
    id: String,
}

#[derive(Debug, Serialize)]
struct SkippedReport {
    path: String,
    reason: String,
}

pub fn dispatch(cwd: &Path, cfg: &Config, cmd: LocalCmd) -> Result<()> {
    match cmd {
        LocalCmd::Register {
            path,
            priority,
            disabled,
        } => {
            let target = resolve_path(cwd, path);
            let (action, entry) = local_registry::register(cfg, &target, priority, !disabled)?;
            let verb = match action {
                RegisterAction::Added => "registered",
                RegisterAction::Updated => "refreshed",
            };
            println!("{verb} {}@{} -> {}", entry.key(), entry.version, entry.path);
            println!("id: {}", entry.id());
            if !entry.enabled {
                println!("note: entry is disabled and will not be selected");
            }
            Ok(())
        }
        LocalCmd::Unregister { selector, all } => {
            let selector = parse_selector(&selector)?;
            let removed = local_registry::unregister(cfg, &selector, all)?;
            for entry in &removed {
                println!("unregistered {} ({})", entry.key(), entry.path);
            }
            Ok(())
        }
        LocalCmd::List { json } => {
            let statuses = local_registry::status(cfg)?;
            if json {
                let reports: Vec<EntryReport> = statuses.iter().map(EntryReport::new).collect();
                println!("{}", serde_json::to_string_pretty(&reports)?);
                return Ok(());
            }
            if statuses.is_empty() {
                println!(
                    "no local projects registered (index: {})",
                    local_registry::index_path(cfg)?.display()
                );
                return Ok(());
            }
            for status in &statuses {
                println!(
                    "{}  {}  priority {}  [{}]",
                    status.entry.key(),
                    status.entry.path,
                    status.entry.priority,
                    status.health.label()
                );
            }
            Ok(())
        }
        LocalCmd::Enable { selector, all } => set_enabled(cfg, &selector, true, all),
        LocalCmd::Disable { selector, all } => set_enabled(cfg, &selector, false, all),
        LocalCmd::Prune { dry_run } => {
            let removed = local_registry::prune(cfg, dry_run)?;
            if removed.is_empty() {
                println!("nothing to prune");
                return Ok(());
            }
            for status in &removed {
                let verb = if dry_run { "would drop" } else { "dropped" };
                println!(
                    "{verb} {} ({}) — {}",
                    status.entry.key(),
                    status.entry.path,
                    status.health.label()
                );
            }
            Ok(())
        }
        LocalCmd::Scan {
            path,
            max_depth,
            priority,
            dry_run,
        } => {
            let root = resolve_path(cwd, path);
            let hits = local_registry::scan(cfg, &root, max_depth, priority, dry_run)?;
            if hits.is_empty() {
                println!("no Zed projects found below {}", root.display());
                return Ok(());
            }
            for hit in &hits {
                let verb = match hit.action {
                    Some(RegisterAction::Added) => "registered",
                    Some(RegisterAction::Updated) => "refreshed",
                    None => "found",
                };
                println!(
                    "{verb} {}@{} -> {}",
                    hit.key,
                    hit.version,
                    hit.dir.display()
                );
            }
            println!("{} project(s)", hits.len());
            Ok(())
        }
        LocalCmd::Resolve {
            package,
            require,
            json,
        } => {
            let key = normalize_key(&package)?;
            let requirement = Requirement::parse(&require);
            let index = local_registry::load(cfg)?;
            let (selected, skipped) = local_registry::select(&index, &key, &requirement)?;
            if json {
                let report = ResolveReport {
                    package: key.clone(),
                    requirement: require.clone(),
                    resolved: selected.as_ref().map(|selection| ResolvedReport {
                        path: selection.entry.path.clone(),
                        version: selection.manifest.package.version.clone(),
                        priority: selection.entry.priority,
                        id: selection.entry.id(),
                    }),
                    skipped: skipped
                        .iter()
                        .map(|(entry, health)| SkippedReport {
                            path: entry.path.clone(),
                            reason: health.label(),
                        })
                        .collect(),
                };
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            match selected {
                Some(selection) => println!(
                    "{key} {} satisfied by {} ({})",
                    require, selection.entry.path, selection.manifest.package.version
                ),
                None => println!("no registered local project satisfies {key} {require}"),
            }
            for (entry, health) in &skipped {
                println!("skipped {} — {}", entry.path, health.label());
            }
            Ok(())
        }
        LocalCmd::Path => {
            println!("{}", local_registry::index_path(cfg)?.display());
            Ok(())
        }
    }
}

fn set_enabled(cfg: &Config, selector: &str, enabled: bool, all: bool) -> Result<()> {
    let selector: Selector = parse_selector(selector)?;
    let changed = local_registry::set_enabled(cfg, &selector, enabled, all)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    for entry in &changed {
        print_entry(verb, entry);
    }
    Ok(())
}

fn print_entry(verb: &str, entry: &LocalEntry) {
    println!("{verb} {} ({})", entry.key(), entry.path);
}

/// Resolve an optional path operand against the invocation directory so a
/// relative `zed local register ../sibling` means what the shell means.
fn resolve_path(cwd: &Path, path: Option<PathBuf>) -> PathBuf {
    match path {
        Some(path) if path.is_absolute() => path,
        Some(path) => cwd.join(path),
        None => cwd.to_path_buf(),
    }
}

fn normalize_key(raw: &str) -> Result<String> {
    let (org, name) = crate::ops::split_key(raw.trim())
        .with_context(|| format!("`{raw}` is not an `org/name` package key"))?;
    Ok(format!("{org}/{name}"))
}
