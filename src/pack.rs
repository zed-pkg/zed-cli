use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::excludes::{ALWAYS_INCLUDE, effective_excludes};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{ARCHIVE_ROOT, IGNORE_FILE, PACK_OUT_DIR};

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
        let rel = entry
            .path()
            .strip_prefix(project)
            .expect("walkdir stays under root")
            .to_path_buf();
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
