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
    self, EntryStatus, LinkPolicy, LocalEntry, RegisterAction, Selector, parse_selector,
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
    /// Which kind of volume the checkout lives on, as recorded at
    /// registration: `fixed`, `removable`, `network`, `container-mount`.
    volume: String,
    /// The volume root, when one is known — the directory whose presence
    /// answers "is this disk still attached?".
    #[serde(skip_serializing_if = "Option::is_none")]
    mount_point: Option<String>,
    /// This entry's own materialization preference.
    link_policy: String,
    /// The path as the index literally holds it, when a path map rewrote it
    /// for this process. Absent when the two are the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_path: Option<String>,
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
            volume: status.entry.volume.kind.as_str().to_string(),
            mount_point: status.entry.volume.mount_point.clone(),
            link_policy: status.entry.link_policy.as_str().to_string(),
            stored_path: status.entry.stored_path.clone(),
        }
    }
}

/// What `zed local doctor` reports: this machine's whole view, as data, so a
/// test can assert on it without parsing prose.
#[derive(Debug, Serialize)]
struct DoctorReport {
    index: String,
    in_container: bool,
    link_policy: String,
    ephemeral: bool,
    path_map: Vec<PathMapRule>,
    entries: Vec<EntryReport>,
}

#[derive(Debug, Serialize)]
struct PathMapRule {
    from: String,
    to: String,
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
            link,
        } => {
            let target = resolve_path(cwd, path);
            let (action, entry) =
                local_registry::register(cfg, &target, priority, !disabled, link.map(Into::into))?;
            let verb = match action {
                RegisterAction::Added => "registered",
                RegisterAction::Updated => "refreshed",
            };
            println!("{verb} {}@{} -> {}", entry.key(), entry.version, entry.path);
            println!("id: {}", entry.id());
            println!("volume: {}", entry.volume.kind.as_str());
            if !entry.enabled {
                println!("note: entry is disabled and will not be selected");
            }
            if entry.volume.kind.is_ephemeral() && entry.link_policy == LinkPolicy::Auto {
                println!(
                    "note: {} media can go away, so installs copy this checkout instead of \
                     symlinking it (override with --local-link-policy symlink)",
                    entry.volume.kind.as_str()
                );
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
        LocalCmd::Doctor { json } => doctor(cfg, json),
    }
}

/// Everything that determines whether a registration is usable *here*: where
/// the index is, whether this process is inside a container, how paths are
/// translated across that boundary, and what each entry's volume looks like
/// right now. One command to answer "why did my install go to the network?".
fn doctor(cfg: &Config, json: bool) -> Result<()> {
    let map = cfg.local.resolved_path_map()?;
    let report = DoctorReport {
        index: local_registry::index_path(cfg)?.display().to_string(),
        in_container: local_registry::in_container(),
        link_policy: cfg.local.resolved_link_policy()?.as_str().to_string(),
        ephemeral: cfg.local.resolved_ephemeral(),
        path_map: map
            .rules()
            .map(|(from, to)| PathMapRule { from, to })
            .collect(),
        entries: local_registry::status(cfg)?
            .iter()
            .map(EntryReport::new)
            .collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("index          {}", report.index);
    println!(
        "container      {}",
        if report.in_container { "yes" } else { "no" }
    );
    println!("link policy    {}", report.link_policy);
    println!(
        "ephemeral      {}",
        if report.ephemeral {
            "yes (every checkout is copied)"
        } else {
            "no"
        }
    );
    if report.path_map.is_empty() {
        println!("path map       (none)");
    } else {
        for rule in &report.path_map {
            println!("path map       {} -> {}", rule.from, rule.to);
        }
    }
    println!("entries        {}", report.entries.len());
    for entry in &report.entries {
        println!(
            "  {:<9} {}  {}  [{}]",
            if entry.selectable { "ok" } else { "unusable" },
            entry.package,
            entry.path,
            entry.volume
        );
        if let Some(stored) = &entry.stored_path {
            println!("            stored as {stored}");
        }
        if !entry.selectable {
            println!("            {}", entry.health);
        }
    }
    Ok(())
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
