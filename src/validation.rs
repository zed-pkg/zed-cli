//! Offline, read-only validation for `.zpkg.toml` and `.zpkg.lock`.
//!
//! Runtime parsing and semantic checks remain owned by `zed-interfaces`.
//! The checked-in schemas supply the canonical property graph used here to
//! reject unknown fields that serde intentionally ignores for compatibility.
//! Zed CLI's additive `[[git-submodule]]` lock extension is removed from the
//! canonical schema view, then parsed and validated by its owning subsystem.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value as JsonValue;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::version::Requirement;

const DESCRIPTOR_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const REPORT_VERSION: u32 = 1;
const INTERFACE_REVISION: &str = "60a8ab55f8a55eb212a72dcb334c1c118047c7ef";
const TRANSITIVE_LIMIT: &str = "not-verifiable-in-lockfile-v1-without-dependency-edges";
const MANIFEST_SCHEMA: &str = include_str!("../schemas/zed-interfaces/manifest.json");
const LOCKFILE_SCHEMA: &str = include_str!("../schemas/zed-interfaces/lockfile.json");

#[derive(Debug, Serialize)]
struct ManifestSummary {
    path: String,
    package: String,
    direct_requirements: usize,
}

#[derive(Debug, Serialize)]
struct LockSummary {
    path: String,
    present: bool,
    version: Option<u32>,
    packages: usize,
    git_submodules: usize,
}

#[derive(Debug, Serialize)]
struct ValidationReport {
    report_version: u32,
    valid: bool,
    interface_revision: &'static str,
    manifest: ManifestSummary,
    lock: LockSummary,
    direct_requirements_checked: usize,
    transitive_completeness: &'static str,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ValidationFailure<'a> {
    report_version: u32,
    valid: bool,
    interface_revision: &'static str,
    manifest: String,
    lock: String,
    error: &'a str,
}

#[derive(Debug)]
struct ResolvedDirect {
    version: String,
    authority: &'static str,
}

/// Validate a manifest and its optional lock without creating configuration,
/// recovering transactions, accessing the package store, authenticating, or
/// performing network I/O.
pub fn run(
    cwd: &Path,
    manifest_arg: &Path,
    lock_arg: &Path,
    require_lock: bool,
    json: bool,
) -> Result<()> {
    match validate(cwd, manifest_arg, lock_arg, require_lock) {
        Ok(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_human(&report);
            }
            Ok(())
        }
        Err(error) => {
            if json {
                let message = format!("{error:#}");
                let failure = ValidationFailure {
                    report_version: REPORT_VERSION,
                    valid: false,
                    interface_revision: INTERFACE_REVISION,
                    manifest: manifest_arg.display().to_string(),
                    lock: lock_arg.display().to_string(),
                    error: &message,
                };
                println!("{}", serde_json::to_string_pretty(&failure)?);
            }
            Err(error)
        }
    }
}

fn validate(
    cwd: &Path,
    manifest_arg: &Path,
    lock_arg: &Path,
    require_lock: bool,
) -> Result<ValidationReport> {
    let manifest_path = resolve(cwd, manifest_arg);
    let lock_path = resolve(cwd, lock_arg);
    let manifest_text = read_required(&manifest_path, "manifest")?;

    validate_schema_shape(&manifest_text, MANIFEST_SCHEMA, "manifest", None)?;
    let manifest = Manifest::parse(&manifest_text)
        .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
    let direct_requirements = manifest.dependencies.len() + manifest.build_dependencies.len();

    let Some(lock_text) = read_optional(&lock_path, "lockfile")? else {
        if require_lock {
            bail!("required lockfile {} does not exist", lock_path.display());
        }
        let mut warnings =
            vec!["lockfile is absent; direct dependency coverage was not checked".to_string()];
        warnings.push(transitive_warning());
        return Ok(ValidationReport {
            report_version: REPORT_VERSION,
            valid: true,
            interface_revision: INTERFACE_REVISION,
            manifest: ManifestSummary {
                path: manifest_arg.display().to_string(),
                package: manifest.full_name(),
                direct_requirements,
            },
            lock: LockSummary {
                path: lock_arg.display().to_string(),
                present: false,
                version: None,
                packages: 0,
                git_submodules: 0,
            },
            direct_requirements_checked: 0,
            transitive_completeness: TRANSITIVE_LIMIT,
            warnings,
        });
    };

    validate_schema_shape(
        &lock_text,
        LOCKFILE_SCHEMA,
        "lockfile",
        Some("git-submodule"),
    )?;
    let lock = Lockfile::parse(&lock_text)
        .with_context(|| format!("invalid lockfile {}", lock_path.display()))?;
    let git_submodules =
        crate::git_submodules::validate_lock_extensions(&lock_text).with_context(|| {
            format!(
                "invalid Git-submodule lock extension in {}",
                lock_path.display()
            )
        })?;

    let mut resolved = BTreeMap::<String, ResolvedDirect>::new();
    for package in &lock.packages {
        resolved.insert(
            package.full_name(),
            ResolvedDirect {
                version: package.version.clone(),
                authority: "canonical package lock",
            },
        );
    }
    for (package, version) in &git_submodules {
        if let Some(canonical) = resolved.get(package) {
            bail!(
                "lockfile records `{package}` through both {} and Git-submodule authority",
                canonical.authority
            );
        }
        resolved.insert(
            package.clone(),
            ResolvedDirect {
                version: version.clone(),
                authority: "Git-submodule lock extension",
            },
        );
    }

    let mut checked = 0;
    for (kind, requirements) in [
        ("dependency", &manifest.dependencies),
        ("build dependency", &manifest.build_dependencies),
    ] {
        for (package, requested) in requirements {
            let locked = resolved.get(package).with_context(|| {
                format!(
                    "incomplete lock state: direct {kind} `{package}` ({requested}) has no canonical or Git-submodule lock entry"
                )
            })?;
            if !Requirement::parse(requested).matches(&locked.version) {
                bail!(
                    "direct {kind} `{package}` requires `{requested}`, but {} pins `{}`",
                    locked.authority,
                    locked.version
                );
            }
            checked += 1;
        }
    }

    Ok(ValidationReport {
        report_version: REPORT_VERSION,
        valid: true,
        interface_revision: INTERFACE_REVISION,
        manifest: ManifestSummary {
            path: manifest_arg.display().to_string(),
            package: manifest.full_name(),
            direct_requirements,
        },
        lock: LockSummary {
            path: lock_arg.display().to_string(),
            present: true,
            version: Some(lock.version),
            packages: lock.packages.len(),
            git_submodules: git_submodules.len(),
        },
        direct_requirements_checked: checked,
        transitive_completeness: TRANSITIVE_LIMIT,
        warnings: vec![transitive_warning()],
    })
}

fn print_human(report: &ValidationReport) {
    println!(
        "valid package manifest: {} ({})",
        report.manifest.path, report.manifest.package
    );
    if report.lock.present {
        println!(
            "valid lockfile: {} (version {}, {} package(s), {} Git submodule(s))",
            report.lock.path,
            report.lock.version.unwrap_or_default(),
            report.lock.packages,
            report.lock.git_submodules
        );
        println!(
            "checked {} direct dependency requirement(s)",
            report.direct_requirements_checked
        );
    } else {
        println!("lockfile not present: {} (not required)", report.lock.path);
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

fn transitive_warning() -> String {
    "lockfile version 1 has no dependency edges; offline validation cannot prove transitive completeness"
        .to_string()
}

fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn read_required(path: &Path, kind: &str) -> Result<String> {
    read_optional(path, kind)?
        .with_context(|| format!("required {kind} {} does not exist", path.display()))
}

fn read_optional(path: &Path, kind: &str) -> Result<Option<String>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {kind} {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("{kind} {} is not a regular file", path.display());
    }
    if metadata.len() > DESCRIPTOR_LIMIT_BYTES {
        bail!(
            "{kind} {} exceeds the {}-byte validation limit",
            path.display(),
            DESCRIPTOR_LIMIT_BYTES
        );
    }
    let bytes = fs::read(path).with_context(|| format!("reading {kind} {}", path.display()))?;
    if bytes.len() as u64 > DESCRIPTOR_LIMIT_BYTES {
        bail!(
            "{kind} {} grew beyond the {}-byte validation limit while being read",
            path.display(),
            DESCRIPTOR_LIMIT_BYTES
        );
    }
    String::from_utf8(bytes)
        .with_context(|| format!("{kind} {} is not valid UTF-8", path.display()))
        .map(Some)
}

fn validate_schema_shape(
    text: &str,
    schema_text: &str,
    kind: &str,
    allowed_extension: Option<&str>,
) -> Result<()> {
    let mut toml_value: toml::Value =
        toml::from_str(text).with_context(|| format!("parsing {kind} as TOML"))?;
    if let Some(extension) = allowed_extension
        && let Some(table) = toml_value.as_table_mut()
    {
        table.remove(extension);
    }
    let document = serde_json::to_value(toml_value)
        .with_context(|| format!("converting {kind} to its schema view"))?;
    let schema: JsonValue = serde_json::from_str(schema_text)
        .with_context(|| format!("loading checked-in {kind} schema"))?;
    validate_shape(&document, &schema, &schema, "$", kind)
}

fn validate_shape(
    value: &JsonValue,
    schema: &JsonValue,
    root: &JsonValue,
    path: &str,
    kind: &str,
) -> Result<()> {
    if let Some(reference) = schema.get("$ref").and_then(JsonValue::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .with_context(|| format!("unsupported external schema reference `{reference}`"))?;
        let target = root
            .pointer(pointer)
            .with_context(|| format!("unresolved schema reference `{reference}`"))?;
        return validate_shape(value, target, root, path, kind);
    }

    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(JsonValue::as_array) {
            let candidates = branches
                .iter()
                .filter(|candidate| schema_type_matches(value, candidate, root))
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return Ok(());
            }
            let mut errors = Vec::new();
            for candidate in candidates {
                match validate_shape(value, candidate, root, path, kind) {
                    Ok(()) => return Ok(()),
                    Err(error) => errors.push(format!("{error:#}")),
                }
            }
            bail!("{}", errors.join("; "));
        }
    }

    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(JsonValue::as_object);
        let additional = schema.get("additionalProperties");
        if properties.is_some() || additional.is_some() {
            for (key, child) in object {
                if let Some(child_schema) = properties.and_then(|known| known.get(key)) {
                    validate_shape(child, child_schema, root, &format!("{path}.{key}"), kind)?;
                    continue;
                }
                match additional {
                    Some(JsonValue::Object(_)) => validate_shape(
                        child,
                        additional.expect("matched object schema"),
                        root,
                        &format!("{path}.{key}"),
                        kind,
                    )?,
                    Some(JsonValue::Bool(true)) => {}
                    _ => bail!("unknown {kind} field `{path}.{key}`"),
                }
            }
        }
    }

    if let Some(items) = schema.get("items")
        && let Some(array) = value.as_array()
    {
        for (index, child) in array.iter().enumerate() {
            validate_shape(child, items, root, &format!("{path}[{index}]"), kind)?;
        }
    }
    Ok(())
}

fn schema_type_matches(value: &JsonValue, schema: &JsonValue, root: &JsonValue) -> bool {
    if let Some(reference) = schema.get("$ref").and_then(JsonValue::as_str)
        && let Some(pointer) = reference.strip_prefix('#')
        && let Some(target) = root.pointer(pointer)
    {
        return schema_type_matches(value, target, root);
    }
    if let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(JsonValue::as_array)
    {
        return branches
            .iter()
            .any(|branch| schema_type_matches(value, branch, root));
    }
    match schema.get("type").and_then(JsonValue::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_schemas_are_pinned_to_the_dependency_revision() {
        let cargo = include_str!("../Cargo.toml");
        assert!(cargo.contains(&format!("rev = \"{INTERFACE_REVISION}\"")));
        for (schema, title) in [(MANIFEST_SCHEMA, "Manifest"), (LOCKFILE_SCHEMA, "Lockfile")] {
            let value: JsonValue = serde_json::from_str(schema).unwrap();
            assert_eq!(value["title"], title);
            assert_eq!(
                value["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
        }
    }

    #[test]
    fn schema_shape_closes_structs_but_keeps_dependency_maps_open() {
        let unknown = r#"
[package]
org = "acme"
name = "tool"
version = "1.0.0"
surprise = true
[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"
"#;
        let error = validate_schema_shape(unknown, MANIFEST_SCHEMA, "manifest", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("$.package.surprise"), "{error}");

        let map = r#"
[package]
org = "acme"
name = "tool"
version = "1.0.0"
[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"
[dependencies]
"acme/anything" = "^1"
"#;
        validate_schema_shape(map, MANIFEST_SCHEMA, "manifest", None).unwrap();
    }
}
