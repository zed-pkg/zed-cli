//! Universal, read-only project/target selection for `zed add`.
//!
//! `.zpkg.toml` owns Zed dependency intent; a native manifest is only a
//! language context hint. No native manager or compiler is invoked here.
//! Ambiguous targets must be selected explicitly before registry I/O.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::language::Language;
use zed_interfaces::paths::MANIFEST_FILE;

use crate::config;
use crate::ops;

#[derive(Debug, PartialEq, Eq)]
pub struct AddSelection {
    pub root: PathBuf,
    /// Target for this Zed consumer manifest, not a compiler invocation.
    pub target: Option<String>,
}

/// Choose the nearest Zed package authority, then the most local *unique*
/// native consumer target. An explicit --project always names an exact Zed
/// root; it is not expanded into another ancestor's dependency graph.
pub pub fn select(
    cwd: &Path,
    project: Option<&Path>,
    explicit_target: Option<&str>,
) -> Result<AddSelection> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("invalid current directory {}", cwd.display()))?;
    let root = match project {
        Some(project) => {
            let candidate = if project.is_absolute() {
                project.to_path_buf()
            } else {
                cwd.join(project)
            };
            candidate
                .canonicalize()
                .with_context(|| format!("cannot resolve --project {}", candidate.display()))?
        }
        None => {
            let mut nearest = None;
            for ancestor in cwd.ancestors() {
                if regular_marker(ancestor, MANIFEST_FILE)? {
                    nearest = Some(ancestor.to_path_buf());
                    break;
                }
            }
            nearest.with_context(|| format!(
                "no {MANIFEST_FILE} found at or above {}; use --project PATH or zed init / zed install PACKAGE first",
                cwd.display()
            ))?
        }
    };
    if !root.is_dir() || !regular_marker(&root, MANIFEST_FILE)? {
        bail!(
            "selected Zed project {} has no {MANIFEST_FILE}; use zed init or zed install PACKAGE",
            root.display()
        );
    }

    // An explicit --target must never launder dual native package markers.
    let mut directory = Some(if cwd.starts_with(&root) {
        cwd.as_path()
    } else {
        root.as_path()
    });
    while let Some(path) = directory {
        native_targets(path)?;
        if path == root {
            break;
        }
        directory = path.parent();
    }

    let manifest = config::read_manifest(&root)?;
    let configured = manifest.requested_target();
    let explicit = explicit_target.map(str::trim);
    if explicit.is_some_and(|value| {
        value.is_empty()
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    }) {
        bail!("--target must be a nonempty language/target token (e.g. attolang, nodejs, rust)");
    }
    // A CLI target is authoritative, but `zed add` writes a durable
    // dependency declaration. Changing the consumer-wide default while
    // existing direct requirements remain would reinterpret them on the
    // next frozen install. Allow an explicit switch only for an empty
    // consumer; otherwise use separate target-specific Zed manifests.
    if let Some(explicit) = explicit
        && !configured.is_some_and(|configured| same_target(explicit, configured))
        && (!manifest.dependencies.is_empty() || !manifest.build_dependencies.is_empty())
    {
        bail!(
            "cannot override configured target `{}` with `{explicit}` in {}: this Zed consumer already has dependencies. Use a separate member .zpkg.toml for independently targeted packages",
            configured.unwrap_or("<unspecified>"),
            root.display()
        );
    }

    // Preserve the current spelling for equivalent aliases such as node /
    // nodejs. The explicit override can change the durable target only
    // when no existing direct dependency would be reinterpreted.
    let selected = match (explicit, configured) {
        (Some(explicit), Some(configured)) if same_target(explicit, configured) => Some(configured),
        (Some(explicit), _) => Some(explicit),
        (None, configured) => configured,
    };
    if let Some(selected) = selected {
        // A durable consumer target is authoritative, but a caller who walks
        // into a different native project should not silently redirect that
        // project's dependency to the parent's language. A deliberate
        // --target/--project remains the explicit override.
        if explicit.is_none() && cwd.starts_with(&root) && cwd != root {
            let mut directory = Some(cwd.as_path());
            while let Some(path) = directory {
                if path == root {
                    break;
                }
                let found = native_targets(path)?;
                if !found.is_empty() {
                    if !found
                        .iter()
                        .any(|candidate| same_target(candidate, selected))
                    {
                        bail!(
                            "native project {} targets {}, but Zed consumer {} selects {}; create a member .zpkg.toml or choose --project explicitly",
                            path.display(),
                            found.into_iter().collect::<Vec<_>>().join(", "),
                            root.display(),
                            selected
                        );
                    }
                    break;
                }
                directory = path.parent();
            }
        }
        return Ok(AddSelection {
            root,
            target: Some(selected.to_owned()),
        });
    }
    let consumer = if cwd.starts_with(&root) {
        cwd.as_path()
    } else {
        root.as_path()
    };
    let mut ancestor = Some(consumer);
    while let Some(directory) = ancestor {
        let detected = native_targets(directory)?;
        if !detected.is_empty() {
            let target = unique_target(directory, detected)?;
            return Ok(AddSelection {
                root,
                target: Some(target),
            });
        }
        if directory == root {
            break;
        }
        ancestor = directory.parent();
    }

    // When invoked at the package root, discover only nearby native member
    // contexts. Never traverse another Zed consumer project, the store, a
    // symlink, or an unbounded directory graph.
    let found = nested_targets(&root)?;
    Ok(AddSelection {
        target: if found.is_empty() {
            None
        } else {
            Some(unique_target(&root, found)?)
        },
        root,
    })
}

fn same_target(left: &str, right: &str) -> bool {
    match (Language::from_token(left), Language::from_token(right)) {
        (Some(a), Some(b)) => a == b,
        _ => {
            left == right
                || matches!(
                    (left, right),
                    ("attolang", "oreslang") | ("oreslang", "attolang")
                )
        }
    }
}

/// A manifest is trusted as a project selector only when its directory
/// contains a *regular file*. A symlink (including a dangling one) must not
/// silently redirect Zed into a different package authority, and a directory
/// named like a manifest is not a valid project marker.
fn regular_marker(directory: &Path, name: &str) -> Result<bool> {
    let path = directory.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "unsafe project marker {}: expected a regular file, not a symlink or directory",
            path.display()
        );
    }
    Ok(true)
}

fn native_targets(directory: &Path) -> Result<BTreeSet<String>> {
    // Native AttoLang markers are compiler-owned, but project inference must
    // not follow a symlink to an unrelated outside source tree.
    let canonical = regular_marker(directory, ".atto.pkg.toml")?;
    let legacy = regular_marker(directory, ".ores.pkg.toml")?;
    if canonical && legacy {
        bail!(
            "ambiguous AttoLang compiler markers in {}: migrate .ores.pkg.toml to .atto.pkg.toml; do not keep both",
            directory.display()
        );
    }
    Ok(ops::native_manifest_targets(directory))
}

fn unique_target(directory: &Path, targets: BTreeSet<String>) -> Result<String> {
    if targets.len() != 1 {
        bail!(
            "ambiguous language context in {} ({}). Select --target LANG and optionally --project PATH; Zed will not guess or edit native manifests",
            directory.display(),
            targets.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    match targets.into_iter().next() {
        Some(target) => Ok(target),
        None => bail!("no target was available after unique-target validation"),
    }
}

fn nested_targets(root: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    const MAX_SCANNED_ENTRIES: usize = 4096;
    let mut scanned = 0usize;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > 0 {
            if regular_marker(&directory, MANIFEST_FILE)? {
                continue; // Separate Zed authority: never infer across it.
            }
            found.extend(native_targets(&directory)?);
        }
        if depth >= 2 {
            continue;
        }
        for child in fs::read_dir(&directory)
            .with_context(|| format!("inspect Zed project {}", directory.display()))?
        {
            scanned += 1;
            if scanned > MAX_SCANNED_ENTRIES {
                bail!(
                    "target inference scanned more than {MAX_SCANNED_ENTRIES} directory entries under {}; set [install].target in .zpkg.toml or pass --target",
                    root.display()
                );
            }
            let child = child?;
            let file_type = child.file_type()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = child.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules"
                        | "zed_modules"
                        | "target"
                        | "vendor"
                        | "dist"
                        | "build"
                        | "out"
                        | ".git"
                )
            {
                continue;
            }
            pending.push((child.path(), depth + 1));
        }
    }
    Ok(found)
}

