fn write_binary_zip(
    path: &Path,
    files: &BTreeMap<String, CollectedFile>,
    descriptor_bytes: &[u8],
) -> Result<()> {
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("opening private binary ZIP staging file {}", path.display()))?;
    let mut writer = zip::ZipWriter::new(output);
    let epoch = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).unwrap_or_default();
    let mut archive_paths = files.keys().cloned().collect::<Vec<_>>();
    archive_paths.push(BINARY_DESCRIPTOR_PATH.to_owned());
    archive_paths.sort();

    for relative in archive_paths {
        let (mode, bytes, source) = if relative == BINARY_DESCRIPTOR_PATH {
            (0o644, Some(descriptor_bytes), None)
        } else {
            let file = files
                .get(&relative)
                .with_context(|| format!("collected file `{relative}` disappeared"))?;
            let mode = if file.executable { 0o755 } else { 0o644 };
            match &file.source {
                CollectedSource::Bytes(bytes) => (mode, Some(bytes.as_slice()), None),
                CollectedSource::File { path, metadata } => {
                    (mode, None, Some((path.as_path(), metadata, file)))
                }
            }
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(6))
            .unix_permissions(mode)
            .last_modified_time(epoch);
        let archive_path = format!("{BINARY_ARCHIVE_ROOT}/{relative}");
        writer.start_file(&archive_path, options)?;
        if let Some(bytes) = bytes {
            writer.write_all(bytes)?;
        } else if let Some((source, metadata, expected)) = source {
            stream_source_into_zip(&mut writer, source, metadata, expected)?;
        }
    }
    let mut output = writer.finish()?;
    output.flush()?;
    output.sync_all()?;
    Ok(())
}

fn stream_source_into_zip<W: Write + std::io::Seek>(
    writer: &mut zip::ZipWriter<W>,
    source: &Path,
    collected_metadata: &fs::Metadata,
    expected: &CollectedFile,
) -> Result<()> {
    // Reopen without retaining one descriptor per input (archives may contain
    // up to 200k files), then bind the open handle to the object inspected
    // during collection. The content digest below also detects in-place edits.
    let (mut input, opened_metadata) = open_regular_file(source, "binary payload")?;
    ensure_same_file(
        collected_metadata,
        &opened_metadata,
        source,
        "binary payload",
    )?;
    ensure!(
        opened_metadata.len() == expected.size,
        "payload `{}` changed size before it was packed; expected {}, got {}",
        expected.path,
        expected.size,
        opened_metadata.len()
    );
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("payload size overflows u64 while packing")?;
        ensure!(
            size <= expected.size,
            "payload `{}` grew while it was being packed; refusing unbounded input",
            expected.path
        );
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    let sha256 = hex::encode(hasher.finalize());
    ensure!(
        size == expected.size && sha256 == expected.sha256,
        "payload `{}` changed while it was being packed; refusing a torn binary ZIP",
        expected.path
    );
    Ok(())
}

fn insert_bytes(
    files: &mut BTreeMap<String, CollectedFile>,
    portable_paths: &mut BTreeMap<String, String>,
    relative: &str,
    bytes: Vec<u8>,
    executable: bool,
) -> Result<()> {
    validate_payload_path(relative, portable_paths)?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    files.insert(
        relative.to_owned(),
        CollectedFile {
            path: relative.to_owned(),
            size: bytes.len() as u64,
            sha256,
            executable,
            source: CollectedSource::Bytes(bytes),
        },
    );
    Ok(())
}

fn insert_source_file(
    files: &mut BTreeMap<String, CollectedFile>,
    portable_paths: &mut BTreeMap<String, String>,
    relative: &str,
    source: PathBuf,
    executable: bool,
) -> Result<()> {
    validate_payload_path(relative, portable_paths)?;
    let (mut handle, metadata) = open_regular_file(&source, "binary payload")?;
    let executable = executable || metadata_is_executable(&metadata);
    let (sha256, size) = sha256_open_file(&mut handle)?;
    match files.get(relative) {
        Some(existing) => {
            ensure!(
                existing.sha256 == sha256 && existing.size == size,
                "two different files map to binary payload path `{relative}`"
            );
            if executable && !existing.executable {
                bail!("binary payload path `{relative}` has inconsistent executable intent");
            }
        }
        None => {
            files.insert(
                relative.to_owned(),
                CollectedFile {
                    path: relative.to_owned(),
                    source: CollectedSource::File {
                        path: source,
                        metadata,
                    },
                    sha256,
                    size,
                    executable,
                },
            );
        }
    }
    Ok(())
}

fn sha256_open_file(file: &mut fs::File) -> Result<(String, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("file size overflows u64 while hashing")?;
        hasher.update(&buffer[..read]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn open_regular_file(path: &Path, kind: &str) -> Result<(fs::File, fs::Metadata)> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {kind} {}", path.display()))?;
    ensure!(
        !path_metadata.file_type().is_symlink() && path_metadata.is_file(),
        "{kind} {} is not a regular, non-symlink file",
        path.display()
    );
    let file = fs::File::open(path).with_context(|| format!("opening {kind} {}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("inspecting open {kind} {}", path.display()))?;
    ensure!(
        opened_metadata.is_file(),
        "{kind} {} changed to a non-regular file while being opened",
        path.display()
    );
    ensure_same_file(&path_metadata, &opened_metadata, path, kind)?;
    Ok((file, opened_metadata))
}

#[cfg(unix)]
fn ensure_same_file(
    before: &fs::Metadata,
    opened: &fs::Metadata,
    path: &Path,
    kind: &str,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        before.dev() == opened.dev() && before.ino() == opened.ino(),
        "{kind} {} changed while being opened",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(
    _before: &fs::Metadata,
    _opened: &fs::Metadata,
    _path: &Path,
    _kind: &str,
) -> Result<()> {
    // Stable Rust's Windows metadata API does not expose a portable file ID.
    // The second-pass digest still rejects changed content, and the handle is
    // retained for the entire read once it is opened.
    Ok(())
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn promote_verified_noclobber(
    staging: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_size: u64,
    kind: &str,
) -> Result<()> {
    if existing_verified_file_matches(destination, expected_sha256, expected_size, kind)? {
        return Ok(());
    }

    match fs::hard_link(staging, destination) {
        Ok(()) => {
            sync_parent_directory(destination)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if existing_verified_file_matches(
                destination,
                expected_sha256,
                expected_size,
                kind,
            )? {
                Ok(())
            } else {
                bail!(
                    "verified {kind} destination {} appeared and disappeared concurrently; retry",
                    destination.display()
                )
            }
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "atomically promoting verified {kind} to {} without overwriting",
                destination.display()
            )
        }),
    }
}

fn existing_verified_file_matches(
    path: &Path,
    expected_sha256: &str,
    expected_size: u64,
    kind: &str,
) -> Result<bool> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting existing {kind} {}", path.display()));
        }
    };
    ensure!(
        !path_metadata.file_type().is_symlink() && path_metadata.is_file(),
        "refusing to replace existing {kind} path because it is not a regular, non-symlink file: {}",
        path.display()
    );
    let (mut file, opened_metadata) = open_regular_file(path, kind)?;
    let (sha256, size) = sha256_open_file(&mut file)?;
    ensure!(
        sha256 == expected_sha256 && size == expected_size,
        "refusing to overwrite conflicting existing {kind} {} (sha256 {sha256}, {size} bytes)",
        path.display()
    );
    let final_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking existing {kind} {}", path.display()))?;
    ensure!(
        !final_metadata.file_type().is_symlink() && final_metadata.is_file(),
        "existing {kind} {} changed while it was being verified",
        path.display()
    );
    ensure_same_file(&opened_metadata, &final_metadata, path, kind)?;
    Ok(true)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)
        .with_context(|| format!("opening output directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("synchronizing output directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn validate_payload_path(
    relative: &str,
    portable_paths: &mut BTreeMap<String, String>,
) -> Result<()> {
    validate_safe_relative_path("binary payload path", relative)
        .map_err(|error| anyhow::anyhow!(error))?;
    validate_portable_archive_path("binary payload path", relative)?;
    ensure!(
        relative != BINARY_DESCRIPTOR_PATH,
        "payload path `{relative}` is reserved"
    );
    let portable = portable_path_key(relative);
    if let Some(existing) = portable_paths.get(&portable) {
        ensure!(
            existing == relative,
            "payload paths `{existing}` and `{relative}` collide under portable case rules"
        );
    } else {
        portable_paths.insert(portable, relative.to_owned());
    }
    Ok(())
}
