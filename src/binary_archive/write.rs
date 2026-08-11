fn write_binary_zip(
    path: &Path,
    files: &BTreeMap<String, CollectedFile>,
    descriptor_bytes: &[u8],
) -> Result<()> {
    let output = fs::File::create(path)?;
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
                CollectedSource::File(source) => (mode, None, Some((source, file))),
            }
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode)
            .last_modified_time(epoch);
        let archive_path = format!("{BINARY_ARCHIVE_ROOT}/{relative}");
        writer.start_file(&archive_path, options)?;
        if let Some(bytes) = bytes {
            writer.write_all(bytes)?;
        } else if let Some((source, expected)) = source {
            stream_source_into_zip(&mut writer, source, expected)?;
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
    expected: &CollectedFile,
) -> Result<()> {
    let mut input = fs::File::open(source)
        .with_context(|| format!("opening binary payload {}", source.display()))?;
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
    let metadata = fs::symlink_metadata(&source)?;
    ensure!(
        metadata.file_type().is_file(),
        "binary payload {} is not a regular file",
        source.display()
    );
    let (sha256, size) = sha256_file(&source)?;
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
                    source: CollectedSource::File(source),
                    sha256,
                    size,
                    executable,
                },
            );
        }
    }
    Ok(())
}

fn validate_payload_path(
    relative: &str,
    portable_paths: &mut BTreeMap<String, String>,
) -> Result<()> {
    validate_safe_relative_path("binary payload path", relative)
        .map_err(|error| anyhow::anyhow!(error))?;
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
