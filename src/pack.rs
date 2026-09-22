use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::excludes::ALWAYS_INCLUDE;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{ARCHIVE_ROOT, PACK_OUT_DIR};

/// One independently publishable artifact produced from a source manifest.
/// A single-language manifest yields one item with `target = None`; a
/// polyglot manifest yields one item per declared target. A target rooted at
/// `dir = "."` is the canonical whole-repository package and is published
/// under the root manifest's exact `org/name` identity.
#[derive(Debug)]
pub struct PackagedTarget {
    pub target: Option<String>,
    pub manifest: Manifest,
    pub packed: PackResult,
}

#[derive(Debug)]
pub struct PackResult {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub file_count: usize,
    pub excluded_count: usize,
    pub format: ArtifactFormat,
}

fn glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(true)
            .build()
            .with_context(|| format!("invalid glob pattern `{pattern}`"))?;
        builder.add(glob);
    }
    Ok(builder.build()?)
}

/// Build the pruned, deterministic `tar.gz` artifact (the default format).
pub fn pack(project: &Path, manifest: &Manifest, out_dir: Option<&Path>) -> Result<PackResult> {
    pack_format(project, manifest, out_dir, ArtifactFormat::TarGz)
}

/// Fan a source repository out into independently publishable target packages.
///
/// Each language target is re-rooted in an isolated artifact:
/// `clients/ts/package.json` becomes `pkg/package.json`, not
/// `pkg/clients/ts/package.json`. Other language directories are never staged
/// into a target artifact. A target whose `dir = "."` retains the complete
/// source repository and is canonicalized to the root manifest's exact package
/// identity, regardless of a legacy target-level `name`. A derived,
/// single-target `.zpkg.toml` is written at every artifact root.
pub fn pack_all(
    project: &Path,
    manifest: &Manifest,
    out_dir: Option<&Path>,
) -> Result<Vec<PackagedTarget>> {
    if !manifest.is_polyglot() {
        return Ok(vec![PackagedTarget {
            target: None,
            manifest: manifest.clone(),
            packed: pack(project, manifest, out_dir)?,
        }]);
    }

    let output = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project.join(PACK_OUT_DIR));
    let mut packages = Vec::with_capacity(manifest.targets.len());

    for (target, _) in manifest.target_package_names() {
        packages.push(pack_target(project, manifest, &target, Some(&output))?);
    }
    Ok(packages)
}

/// Build the one artifact `pack_all` would emit for a single declared target.
///
/// Resolving one package must not require every sibling target to be present:
/// a target rooted at generated output is absent from a source tag, and that
/// alone should not make the repository's other packages unresolvable.
pub(crate) fn pack_target(
    project: &Path,
    manifest: &Manifest,
    target: &str,
    out_dir: Option<&Path>,
) -> Result<PackagedTarget> {
    let output = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| project.join(PACK_OUT_DIR));
    let mut derived = manifest
        .manifest_for_target(target)
        .with_context(|| format!("target `{target}` disappeared during packing"))?;
    let section = manifest
        .targets
        .get(target)
        .with_context(|| format!("target `{target}` disappeared during packing"))?;
    let source = project.join(&section.dir);
    if !source.is_dir() {
        bail!(
            "target `{target}` source root `{}` is not a directory",
            section.dir
        );
    }

    // A root target is the repository's canonical package. Older manifests
    // named it `<package>-repository` to avoid a schema-level name clash;
    // the emitted artifact must nevertheless use the exact root identity
    // so `zed install org/repository-name` works as expected.
    if section.dir == "." {
        derived.package.name = manifest.package.name.clone();
    } else if source.join(zed_interfaces::paths::MANIFEST_FILE).exists() {
        bail!(
            "target `{target}` contains its own {}; declare packages only in the repository-root manifest",
            zed_interfaces::paths::MANIFEST_FILE
        );
    }

    let ignore_rules = crate::publish_ignore::read_rules(&source)?;
    let staging = tempfile::tempdir().context("create target packing directory")?;
    copy_files(&source, staging.path(), &derived, &ignore_rules)?;
    copy_root_legal_files(project, staging.path())?;
    fs::write(
        staging.path().join(zed_interfaces::paths::MANIFEST_FILE),
        derived.to_toml_string()?,
    )?;
    validate_staged_cargo_path_dependencies(target, staging.path())?;

    let packed = pack_format_with_ignore_rules(
        staging.path(),
        &derived,
        Some(&output),
        ArtifactFormat::TarGz,
        &ignore_rules,
    )?;
    Ok(PackagedTarget {
        target: Some(target.to_string()),
        manifest: derived,
        packed,
    })
}

fn copy_files(
    source: &Path,
    destination: &Path,
    manifest: &Manifest,
    ignore_rules: &[String],
) -> Result<()> {
    let excludes = glob_set(&crate::publish_ignore::effective_artifact_excludes(
        manifest,
        ignore_rules,
    ))?;
    let always: Vec<String> = ALWAYS_INCLUDE
        .iter()
        .map(|value| value.to_string())
        .collect();
    let always = glob_set(&always)?;
    for entry in WalkDir::new(source)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(source)?;
        if !always.is_match(rel) && excludes.is_match(rel) {
            continue;
        }
        let dest = destination.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), &dest)?;
    }
    Ok(())
}

/// Preserve repository-level license/notice files when a language directory
/// does not carry its own copy. These are the only source-root files allowed
/// into a target artifact.
fn copy_root_legal_files(project: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(project)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if !["LICENSE", "LICENCE", "COPYING", "NOTICE"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            continue;
        }
        let dest = destination.join(&name);
        if !dest.exists() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Validate native Cargo path dependencies against the artifact as staged,
/// not the larger source checkout. A path may walk upward inside the staged
/// tree (for example `crates/a -> ../b`) but must never escape the artifact or
/// refer to a directory that the artifact does not actually contain.
fn validate_staged_cargo_path_dependencies(target: &str, staged_root: &Path) -> Result<()> {
    for entry in WalkDir::new(staged_root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() || entry.file_name() != "Cargo.toml" {
            continue;
        }
        validate_cargo_manifest_path_dependencies(target, staged_root, entry.path())?;
    }
    Ok(())
}

fn validate_cargo_manifest_path_dependencies(
    target: &str,
    staged_root: &Path,
    manifest_path: &Path,
) -> Result<()> {
    let contents = fs::read_to_string(manifest_path)
        .with_context(|| format!("read staged Cargo manifest {}", manifest_path.display()))?;
    let document: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("parse staged Cargo manifest {}", manifest_path.display()))?;
    let root = document.as_table().with_context(|| {
        format!(
            "staged Cargo manifest {} must contain a TOML table",
            manifest_path.display()
        )
    })?;

    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        validate_cargo_dependency_table(
            target,
            staged_root,
            manifest_path,
            section,
            root.get(section).and_then(toml::Value::as_table),
        )?;
    }

    if let Some(workspace) = root.get("workspace").and_then(toml::Value::as_table) {
        validate_cargo_dependency_table(
            target,
            staged_root,
            manifest_path,
            "workspace.dependencies",
            workspace
                .get("dependencies")
                .and_then(toml::Value::as_table),
        )?;
    }

    if let Some(target_sections) = root.get("target").and_then(toml::Value::as_table) {
        for (selector, value) in target_sections {
            let Some(selector_table) = value.as_table() else {
                continue;
            };
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                let section_name = format!("target.{selector}.{section}");
                validate_cargo_dependency_table(
                    target,
                    staged_root,
                    manifest_path,
                    &section_name,
                    selector_table.get(section).and_then(toml::Value::as_table),
                )?;
            }
        }
    }

    Ok(())
}

fn validate_cargo_dependency_table(
    target: &str,
    staged_root: &Path,
    manifest_path: &Path,
    section: &str,
    dependencies: Option<&toml::value::Table>,
) -> Result<()> {
    let Some(dependencies) = dependencies else {
        return Ok(());
    };
    for (dependency, value) in dependencies {
        let Some(specification) = value.as_table() else {
            continue;
        };
        let Some(declared_path) = specification.get("path").and_then(toml::Value::as_str) else {
            continue;
        };
        validate_cargo_dependency_path(
            target,
            staged_root,
            manifest_path,
            section,
            dependency,
            declared_path,
        )?;
    }
    Ok(())
}

fn validate_cargo_dependency_path(
    target: &str,
    staged_root: &Path,
    manifest_path: &Path,
    section: &str,
    dependency: &str,
    declared_path: &str,
) -> Result<()> {
    let manifest_relative = manifest_path
        .strip_prefix(staged_root)
        .unwrap_or(manifest_path);
    let manifest_dir = manifest_path
        .parent()
        .context("staged Cargo manifest has no parent directory")?;
    let mut relative = manifest_dir
        .strip_prefix(staged_root)
        .with_context(|| {
            format!(
                "staged Cargo manifest {} is outside target root {}",
                manifest_path.display(),
                staged_root.display()
            )
        })?
        .to_path_buf();

    let declared = Path::new(declared_path);
    if declared.is_absolute() {
        bail!(
            "target `{target}` staged Cargo manifest `{}` dependency `{dependency}` in [{section}] uses absolute path `{declared_path}`; native path dependencies must stay inside the staged target artifact",
            manifest_relative.display()
        );
    }

    for component in declared.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir => {
                if !relative.pop() {
                    bail!(
                        "target `{target}` staged Cargo manifest `{}` dependency `{dependency}` in [{section}] path `{declared_path}` escapes the staged target artifact",
                        manifest_relative.display()
                    );
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "target `{target}` staged Cargo manifest `{}` dependency `{dependency}` in [{section}] uses non-relative path `{declared_path}`; native path dependencies must stay inside the staged target artifact",
                    manifest_relative.display()
                );
            }
        }
    }

    let resolved = staged_root.join(&relative);
    if !resolved.is_dir() {
        bail!(
            "target `{target}` staged Cargo manifest `{}` dependency `{dependency}` in [{section}] path `{declared_path}` resolves to missing staged directory `{}`",
            manifest_relative.display(),
            relative.display()
        );
    }
    if !resolved.join("Cargo.toml").is_file() {
        bail!(
            "target `{target}` staged Cargo manifest `{}` dependency `{dependency}` in [{section}] path `{declared_path}` resolves to `{}` without Cargo.toml",
            manifest_relative.display(),
            relative.display()
        );
    }
    Ok(())
}

/// Build the pruned, deterministic artifact for the project in the given
/// format (`tar.gz` or `zip`). Entries are rooted under `pkg/`, sorted by
/// path, with zeroed timestamps and ids so the same tree always produces the
/// same sha256 regardless of format.
pub fn pack_format(
    project: &Path,
    manifest: &Manifest,
    out_dir: Option<&Path>,
    format: ArtifactFormat,
) -> Result<PackResult> {
    let ignore_rules = crate::publish_ignore::read_rules(project)?;
    pack_format_with_ignore_rules(project, manifest, out_dir, format, &ignore_rules)
}

fn pack_format_with_ignore_rules(
    project: &Path,
    manifest: &Manifest,
    out_dir: Option<&Path>,
    format: ArtifactFormat,
    ignore_rules: &[String],
) -> Result<PackResult> {
    let excludes = glob_set(&crate::publish_ignore::effective_artifact_excludes(
        manifest,
        ignore_rules,
    ))?;
    let always: Vec<String> = ALWAYS_INCLUDE.iter().map(|s| s.to_string()).collect();
    let always = glob_set(&always)?;

    let out_dir = match out_dir {
        Some(directory) => directory.to_path_buf(),
        None => project.join(PACK_OUT_DIR),
    };
    let file_name = format!(
        "{}-{}-{}.{}",
        manifest.package.org,
        manifest.package.name,
        manifest.package.version,
        format.extension()
    );
    let out_path = out_dir.join(file_name);
    let output_directory_relative = out_dir
        .strip_prefix(project)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf);
    let output_file_relative = out_path.strip_prefix(project).ok().map(Path::to_path_buf);

    let mut included: Vec<PathBuf> = Vec::new();
    let mut excluded_count = 0usize;
    for entry in WalkDir::new(project)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(project).map(Path::to_path_buf) else {
            continue;
        };
        if output_file_relative.as_ref() == Some(&rel)
            || output_directory_relative
                .as_ref()
                .is_some_and(|output| rel.starts_with(output))
        {
            excluded_count += 1;
            continue;
        }
        if always.is_match(&rel) || !excludes.is_match(&rel) {
            included.push(rel);
        } else {
            excluded_count += 1;
        }
    }
    included.sort();

    fs::create_dir_all(&out_dir)?;

    match format {
        ArtifactFormat::TarGz => write_tar_gz(project, &included, &out_path)?,
        ArtifactFormat::Zip => write_zip(project, &included, &out_path)?,
    }

    let (sha256, size) = sha256_file(&out_path)?;
    Ok(PackResult {
        path: out_path,
        sha256,
        size,
        file_count: included.len(),
        excluded_count,
        format,
    })
}

/// Deterministic gzip'd tar: entries rooted under `pkg/`, zeroed mtime/uid/gid.
fn write_tar_gz(project: &Path, included: &[PathBuf], out_path: &Path) -> Result<()> {
    let file = fs::File::create(out_path)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for rel in included {
        let full = project.join(rel);
        let data = fs::read(&full)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(file_mode(&full)?);
        let archive_path = format!("{ARCHIVE_ROOT}/{}", rel.to_string_lossy());
        builder.append_data(&mut header, archive_path, data.as_slice())?;
    }
    let encoder = builder.into_inner()?;
    let mut file = encoder.finish()?;
    file.flush()?;
    Ok(())
}

/// Deterministic zip: entries rooted under `pkg/`, fixed 1980 zip-epoch
/// timestamp so the same tree always produces the same sha256.
fn write_zip(project: &Path, included: &[PathBuf], out_path: &Path) -> Result<()> {
    use std::io::Write as _;
    let file = fs::File::create(out_path)?;
    let mut writer = zip::ZipWriter::new(file);
    let epoch = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).unwrap_or_default();
    for rel in included {
        let full = project.join(rel);
        let data = fs::read(&full)?;
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(file_mode(&full)?)
            .last_modified_time(epoch);
        let archive_path = format!("{ARCHIVE_ROOT}/{}", rel.to_string_lossy());
        writer.start_file(archive_path, options)?;
        writer.write_all(&data)?;
    }
    writer.finish()?;
    Ok(())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    Ok(if mode & 0o111 != 0 { 0o755 } else { 0o644 })
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<u32> {
    Ok(0o644)
}

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use flate2::read::GzDecoder;

    use super::*;

    fn archive_files(path: &Path) -> BTreeSet<String> {
        let file = fs::File::open(path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    fn rust_target_manifest() -> &'static str {
        r#"
[package]
org = "acme"
name = "rust-client"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/rust-client"

[targets.rust]
dir = "client"
adapter = "rust"
"#
    }

    fn write_minimal_crate(path: &Path, manifest: &str) {
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("Cargo.toml"), manifest).unwrap();
        fs::write(path.join("src/lib.rs"), "pub fn ready() -> bool { true }\n").unwrap();
    }

    #[test]
    fn target_pack_rejects_cargo_path_dependency_outside_staged_root() {
        let project = tempfile::tempdir().unwrap();
        write_minimal_crate(
            &project.path().join("client"),
            r#"[package]
name = "client"
version = "0.1.0"
edition = "2024"

[dependencies]
shared = { path = "../shared" }
"#,
        );
        write_minimal_crate(
            &project.path().join("shared"),
            r#"[package]
name = "shared"
version = "0.1.0"
edition = "2024"
"#,
        );
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            rust_target_manifest(),
        )
        .unwrap();
        let manifest = Manifest::parse(rust_target_manifest()).unwrap();

        let error = pack_target(project.path(), &manifest, "rust", None).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("target `rust`"), "{message}");
        assert!(message.contains("Cargo.toml"), "{message}");
        assert!(message.contains("dependency `shared`"), "{message}");
        assert!(message.contains("../shared"), "{message}");
        assert!(
            message.contains("escapes the staged target artifact"),
            "{message}"
        );
    }

    #[test]
    fn target_pack_rejects_target_specific_escaping_path_dependency() {
        let project = tempfile::tempdir().unwrap();
        write_minimal_crate(
            &project.path().join("client"),
            r#"[package]
name = "client"
version = "0.1.0"
edition = "2024"

[target.'cfg(unix)'.build-dependencies]
helper = { path = "../helper" }
"#,
        );
        write_minimal_crate(
            &project.path().join("helper"),
            r#"[package]
name = "helper"
version = "0.1.0"
edition = "2024"
"#,
        );
        let manifest = Manifest::parse(rust_target_manifest()).unwrap();

        let error = pack_target(project.path(), &manifest, "rust", None).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("target.cfg(unix).build-dependencies"),
            "{message}"
        );
        assert!(message.contains("dependency `helper`"), "{message}");
        assert!(message.contains("../helper"), "{message}");
    }

    #[test]
    fn target_pack_accepts_contained_cargo_path_dependency() {
        let project = tempfile::tempdir().unwrap();
        write_minimal_crate(
            &project.path().join("client"),
            r#"[package]
name = "client"
version = "0.1.0"
edition = "2024"

[dependencies]
shared = { path = "vendor/shared" }
"#,
        );
        write_minimal_crate(
            &project.path().join("client/vendor/shared"),
            r#"[package]
name = "shared"
version = "0.1.0"
edition = "2024"
"#,
        );
        let manifest = Manifest::parse(rust_target_manifest()).unwrap();

        let packed = pack_target(project.path(), &manifest, "rust", None).unwrap();
        let files = archive_files(&packed.packed.path);
        assert!(files.contains("pkg/Cargo.toml"));
        assert!(files.contains("pkg/vendor/shared/Cargo.toml"));
    }

    #[test]
    fn target_pack_accepts_parent_segments_that_stay_inside_staged_root() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("client/crates/a/src")).unwrap();
        fs::write(
            project.path().join("client/Cargo.toml"),
            r#"[workspace]
members = ["crates/a", "crates/b"]
resolver = "2"
"#,
        )
        .unwrap();
        write_minimal_crate(
            &project.path().join("client/crates/a"),
            r#"[package]
name = "a"
version = "0.1.0"
edition = "2024"

[dependencies]
b = { path = "../b" }
"#,
        );
        write_minimal_crate(
            &project.path().join("client/crates/b"),
            r#"[package]
name = "b"
version = "0.1.0"
edition = "2024"
"#,
        );
        let manifest = Manifest::parse(rust_target_manifest()).unwrap();

        let packed = pack_target(project.path(), &manifest, "rust", None).unwrap();
        let files = archive_files(&packed.packed.path);
        assert!(files.contains("pkg/crates/a/Cargo.toml"));
        assert!(files.contains("pkg/crates/b/Cargo.toml"));
    }

    #[test]
    fn polyglot_pack_re_roots_and_isolates_every_target() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("clients/ts/src")).unwrap();
        fs::create_dir_all(project.path().join("clients/java/src")).unwrap();
        fs::write(
            project.path().join("clients/ts/package.json"),
            r#"{"name":"@acme/client"}"#,
        )
        .unwrap();
        fs::write(project.path().join("clients/ts/src/index.js"), "export {};").unwrap();
        fs::write(
            project.path().join("clients/java/pom.xml"),
            "<project></project>",
        )
        .unwrap();
        fs::write(
            project.path().join("clients/java/src/Client.java"),
            "class Client {}",
        )
        .unwrap();
        fs::write(project.path().join("LICENSE"), "MIT").unwrap();

        let source_manifest = r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/clients"

[targets.nodejs]
dir = "clients/ts"
adapter = "node"

[targets.java]
dir = "clients/java"
adapter = "java"
"#;
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            source_manifest,
        )
        .unwrap();
        let manifest = Manifest::parse(source_manifest).unwrap();

        let packages = pack_all(project.path(), &manifest, None).unwrap();
        assert_eq!(packages.len(), 2);
        assert!(packages.iter().all(|package| package.target.is_some()));

        let node = packages
            .iter()
            .find(|package| package.target.as_deref() == Some("nodejs"))
            .unwrap();
        assert_eq!(node.manifest.package.name, "clients-nodejs");
        let node_files = archive_files(&node.packed.path);
        assert!(node_files.contains("pkg/.zpkg.toml"));
        assert!(node_files.contains("pkg/package.json"));
        assert!(node_files.contains("pkg/src/index.js"));
        assert!(node_files.contains("pkg/LICENSE"));
        assert!(!node_files.iter().any(|path| path.contains("clients/")));
        assert!(!node_files.iter().any(|path| path.ends_with("pom.xml")));

        let java = packages
            .iter()
            .find(|package| package.target.as_deref() == Some("java"))
            .unwrap();
        assert_eq!(java.manifest.package.name, "clients-java");
        let java_files = archive_files(&java.packed.path);
        assert!(java_files.contains("pkg/pom.xml"));
        assert!(java_files.contains("pkg/src/Client.java"));
        assert!(!java_files.iter().any(|path| path.ends_with("package.json")));
    }

    #[test]
    fn whole_repository_target_uses_the_canonical_package_identity() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("clients/ts/src")).unwrap();
        fs::write(
            project.path().join("clients/ts/package.json"),
            r#"{"name":"@acme/client"}"#,
        )
        .unwrap();
        fs::write(project.path().join("clients/ts/src/index.js"), "export {};").unwrap();
        fs::write(project.path().join("LICENSE"), "MIT").unwrap();

        let source_manifest = r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/clients"

[publish]
exclude = [".zed-pack/**"]

[targets.repository]
dir = "."
name = "clients"

[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#;
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            source_manifest,
        )
        .unwrap();
        let manifest = Manifest::parse(source_manifest).unwrap();

        let packages = pack_all(project.path(), &manifest, None).unwrap();
        assert_eq!(packages.len(), 2);

        let repository = packages
            .iter()
            .find(|package| package.target.as_deref() == Some("repository"))
            .unwrap();
        assert_eq!(repository.manifest.package.name, "clients");
        assert!(!repository.manifest.is_polyglot());
        assert_eq!(
            repository
                .packed
                .path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "acme-clients-1.2.3.tar.gz"
        );

        let files = archive_files(&repository.packed.path);
        assert!(files.contains("pkg/.zpkg.toml"));
        assert!(files.contains("pkg/clients/ts/package.json"));
        assert!(files.contains("pkg/clients/ts/src/index.js"));
        assert!(files.contains("pkg/LICENSE"));
        assert!(
            !files.iter().any(|path| path.starts_with("pkg/.zed-pack/")),
            "the pack output directory must never be packed into the repository target"
        );

        let node = packages
            .iter()
            .find(|package| package.target.as_deref() == Some("nodejs"))
            .unwrap();
        assert_eq!(node.manifest.package.name, "clients-nodejs");
        let node_files = archive_files(&node.packed.path);
        assert!(node_files.contains("pkg/package.json"));
        assert!(!node_files.iter().any(|path| path.contains("clients/")));
    }

    #[test]
    fn consecutive_default_packs_are_identical_and_exclude_prior_outputs() {
        let project = tempfile::tempdir().unwrap();
        let source_manifest = r#"
[package]
org = "acme"
name = "deterministic"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/deterministic"
"#;
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            source_manifest,
        )
        .unwrap();
        fs::write(project.path().join("payload.txt"), "stable payload\n").unwrap();
        let manifest = Manifest::parse(source_manifest).unwrap();

        let first = pack(project.path(), &manifest, None).unwrap();
        let first_files = archive_files(&first.path);
        let second = pack(project.path(), &manifest, None).unwrap();
        let second_files = archive_files(&second.path);

        assert_eq!(second.sha256, first.sha256);
        assert_eq!(second.size, first.size);
        assert_eq!(second.file_count, first.file_count);
        assert!(first_files.contains("pkg/.zpkg.toml"));
        assert!(first_files.contains("pkg/payload.txt"));
        assert_eq!(second_files, first_files);
        assert!(
            !second_files
                .iter()
                .any(|entry| entry.starts_with("pkg/.zed/pack/"))
        );
    }

    #[test]
    fn output_in_project_root_excludes_only_the_prior_archive() {
        let project = tempfile::tempdir().unwrap();
        let source_manifest = r#"
[package]
org = "acme"
name = "root-output"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/root-output"
"#;
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            source_manifest,
        )
        .unwrap();
        fs::write(project.path().join("payload.txt"), "stable payload\n").unwrap();
        let manifest = Manifest::parse(source_manifest).unwrap();

        let first = pack(project.path(), &manifest, Some(project.path())).unwrap();
        let second = pack(project.path(), &manifest, Some(project.path())).unwrap();
        let files = archive_files(&second.path);

        assert_eq!(second.sha256, first.sha256);
        assert!(files.contains("pkg/payload.txt"));
        assert!(!files.iter().any(|entry| entry.ends_with(".tar.gz")));
    }
}
