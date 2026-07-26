use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{ARCHIVE_ROOT, IGNORE_FILE, PACK_OUT_DIR};

/// One independently publishable artifact produced from a source manifest.
/// A single-language manifest yields one item with `target = None`; a
/// polyglot manifest yields one item per declared target.
pub struct PackagedTarget {
    pub target: Option<String>,
    pub manifest: Manifest,
    pub packed: PackResult,
}

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

/// Fan a source repository out into independently publishable packages.
///
/// A target directory is re-rooted in its artifact: `clients/ts/package.json`
/// becomes `pkg/package.json`, not `pkg/clients/ts/package.json`. The source
/// repository's other language directories are never staged and therefore
/// cannot leak into the artifact. A derived, single-language `.zpkg.toml` is
/// written at the artifact root.
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
        let derived = manifest
            .manifest_for_target(&target)
            .with_context(|| format!("target `{target}` disappeared during packing"))?;
        let section = manifest
            .targets
            .get(&target)
            .with_context(|| format!("target `{target}` disappeared during packing"))?;
        let source = project.join(&section.dir);
        if !source.is_dir() {
            bail!(
                "target `{target}` source root `{}` is not a directory",
                section.dir
            );
        }
        // `dir = "."` is the explicit whole-repository target used alongside
        // language slices. Its manifest is the source manifest by definition
        // and is replaced with the derived single-target manifest below.
        // Nested targets must still never carry a second manifest.
        if section.dir != "." && source.join(zed_interfaces::paths::MANIFEST_FILE).exists() {
            bail!(
                "target `{target}` contains its own {}; declare packages only in the repository-root manifest",
                zed_interfaces::paths::MANIFEST_FILE
            );
        }

        let staging = tempfile::tempdir().context("create target packing directory")?;
        copy_files(&source, staging.path(), &derived)?;
        copy_root_legal_files(project, staging.path())?;
        fs::write(
            staging.path().join(zed_interfaces::paths::MANIFEST_FILE),
            derived.to_toml_string()?,
        )?;

        let packed = pack_format(
            staging.path(),
            &derived,
            Some(&output),
            ArtifactFormat::TarGz,
        )?;
        packages.push(PackagedTarget {
            target: Some(target),
            manifest: derived,
            packed,
        });
    }
    Ok(packages)
}

fn copy_files(source: &Path, destination: &Path, manifest: &Manifest) -> Result<()> {
    let excludes = glob_set(&effective_excludes(
        &manifest.publish.exclude,
        manifest.publish.include_readme,
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
    let mut extra = manifest.publish.exclude.clone();
    // The default excludes strip `zed_modules/**`, but a package may relocate
    // its installed tree with `[install].dir`. Never publish a dependency
    // tree: it bloats the artifact and (in symlink mode) would ship links
    // into the author's own store that mean nothing on a consumer's machine.
    let modules_dir = manifest.modules_dir().trim_matches('/').to_string();
    if !modules_dir.is_empty() {
        extra.push(format!("{modules_dir}/**"));
    }
    let ignore_file = project.join(IGNORE_FILE);
    if ignore_file.exists() {
        for line in fs::read_to_string(&ignore_file)?.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                extra.push(line.to_string());
            }
        }
    }
    let excludes = glob_set(&effective_excludes(&extra, manifest.publish.include_readme))?;
    let always: Vec<String> = ALWAYS_INCLUDE.iter().map(|s| s.to_string()).collect();
    let always = glob_set(&always)?;

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
        if always.is_match(&rel) || !excludes.is_match(&rel) {
            included.push(rel);
        } else {
            excluded_count += 1;
        }
    }
    included.sort();

    let out_dir = match out_dir {
        Some(d) => d.to_path_buf(),
        None => project.join(PACK_OUT_DIR),
    };
    fs::create_dir_all(&out_dir)?;
    let file_name = format!(
        "{}-{}-{}.{}",
        manifest.package.org,
        manifest.package.name,
        manifest.package.version,
        format.extension()
    );
    let out_path = out_dir.join(file_name);

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

        let manifest = Manifest::parse(
            r#"
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
"#,
        )
        .unwrap();

        let packages = pack_all(project.path(), &manifest, None).unwrap();
        assert_eq!(packages.len(), 2);

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

        let manifest_text = {
            let file = fs::File::open(&node.packed.path).unwrap();
            let mut archive = tar::Archive::new(GzDecoder::new(file));
            let mut text = String::new();
            for entry in archive.entries().unwrap() {
                let mut entry = entry.unwrap();
                if entry.path().unwrap() == Path::new("pkg/.zpkg.toml") {
                    entry.read_to_string(&mut text).unwrap();
                }
            }
            text
        };
        let derived = Manifest::parse(&manifest_text).unwrap();
        assert!(!derived.is_polyglot());
        assert_eq!(derived.install.adapter.as_deref(), Some("node"));
    }

    #[test]
    fn polyglot_pack_can_include_a_whole_repository_target() {
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

[targets.repository]
dir = "."
name = "clients-repository"

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
        assert_eq!(repository.manifest.package.name, "clients-repository");
        assert!(!repository.manifest.is_polyglot());

        let files = archive_files(&repository.packed.path);
        assert!(files.contains("pkg/.zpkg.toml"));
        assert!(files.contains("pkg/clients/ts/package.json"));
        assert!(files.contains("pkg/clients/ts/src/index.js"));
        assert!(files.contains("pkg/LICENSE"));
        assert!(
            !files.iter().any(|path| path.starts_with("pkg/.zed-pack/")),
            "the pack output directory must never be packed into the repository target"
        );
    }
}
