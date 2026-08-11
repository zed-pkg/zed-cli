pub fn verify_binary_zip(
    archive_path: &Path,
    expected_platform: Option<&BinaryPlatformV1>,
) -> Result<VerifiedBinaryArtifact> {
    let metadata = fs::metadata(archive_path)
        .with_context(|| format!("reading binary ZIP metadata {}", archive_path.display()))?;
    ensure!(metadata.is_file(), "{} is not a file", archive_path.display());
    ensure!(
        metadata.len() <= max_binary_archive_bytes(),
        "binary ZIP is {} bytes, above the {}-byte limit",
        metadata.len(),
        max_binary_archive_bytes()
    );
    require_zip_magic(archive_path)?;
    let (sha256, size) = sha256_file(archive_path)?;
    ensure!(
        size == metadata.len(),
        "binary ZIP changed while its outer digest was being computed"
    );

    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("opening binary ZIP {}", archive_path.display()))?;
    ensure!(
        !archive.has_overlapping_files()?,
        "binary ZIP contains overlapping file ranges; refusing"
    );
    ensure!(
        archive.len() <= max_binary_entries(),
        "binary ZIP has {} entries, above the {}-entry limit",
        archive.len(),
        max_binary_entries()
    );

    let mut files = BTreeSet::<String>::new();
    let mut portable_paths = BTreeMap::<String, String>::new();
    let mut descriptor_bytes: Option<Vec<u8>> = None;
    let mut package_manifest_bytes: Option<Vec<u8>> = None;
    let mut expanded_total = 0_u64;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        ensure!(!entry.encrypted(), "ZIP entry `{}` is encrypted", entry.name());
        ensure!(
            matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            ),
            "ZIP entry `{}` uses unsupported compression {:?}",
            entry.name(),
            entry.compression()
        );
        ensure!(!entry.is_symlink(), "ZIP entry `{}` is a symlink", entry.name());
        ensure!(
            entry.is_file() || entry.is_dir(),
            "ZIP entry `{}` is neither a regular file nor a directory",
            entry.name()
        );
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0
                    || (entry.is_file() && kind == 0o100000)
                    || (entry.is_dir() && kind == 0o040000),
                "ZIP entry `{}` carries unsupported Unix file type {:o}",
                entry.name(),
                kind
            );
        }
        let raw_name = std::str::from_utf8(entry.name_raw())
            .with_context(|| format!("ZIP entry {index} name is not UTF-8"))?;
        ensure!(
            !raw_name.contains('\\'),
            "ZIP entry `{raw_name}` uses a backslash path separator"
        );
        let enclosed = entry
            .enclosed_name()
            .with_context(|| format!("ZIP entry `{raw_name}` escapes the archive root"))?;
        let normalized = enclosed.to_string_lossy().replace('\\', "/");
        if entry.is_dir() && normalized.trim_end_matches('/') == BINARY_ARCHIVE_ROOT {
            ensure!(
                raw_name == format!("{BINARY_ARCHIVE_ROOT}/"),
                "ZIP root directory `{raw_name}` is not canonically encoded"
            );
            continue;
        }
        let relative = normalized
            .strip_prefix(&format!("{BINARY_ARCHIVE_ROOT}/"))
            .with_context(|| {
                format!("ZIP entry `{raw_name}` is not beneath `{BINARY_ARCHIVE_ROOT}/`")
            })?
            .trim_end_matches('/')
            .to_owned();
        validate_safe_relative_path("ZIP entry path", &relative)
            .map_err(|error| anyhow::anyhow!(error))?;
        let canonical_name = if entry.is_dir() {
            format!("{BINARY_ARCHIVE_ROOT}/{relative}/")
        } else {
            format!("{BINARY_ARCHIVE_ROOT}/{relative}")
        };
        ensure!(
            raw_name == canonical_name,
            "ZIP entry `{raw_name}` is not canonically encoded as `{canonical_name}`"
        );
        let portable = portable_path_key(&relative);
        if let Some(existing) = portable_paths.insert(portable, relative.clone()) {
            bail!(
                "ZIP entries `{existing}` and `{relative}` collide under portable case rules"
            );
        }
        if entry.is_dir() {
            continue;
        }
        ensure!(
            !files.contains(&relative),
            "ZIP contains duplicate file `{relative}`"
        );
        enforce_compression_ratio(raw_name, entry.size(), entry.compressed_size())?;
        expanded_total = expanded_total
            .checked_add(entry.size())
            .context("binary ZIP expanded size overflows u64")?;
        ensure!(
            expanded_total <= max_binary_expanded_bytes(),
            "binary ZIP expands past the {}-byte limit",
            max_binary_expanded_bytes()
        );

        if relative == BINARY_DESCRIPTOR_PATH {
            ensure!(descriptor_bytes.is_none(), "binary ZIP has multiple descriptors");
            descriptor_bytes = Some(read_small_entry(
                &mut entry,
                MAX_DESCRIPTOR_BYTES,
                BINARY_DESCRIPTOR_PATH,
            )?);
        } else if relative == BINARY_PACKAGE_MANIFEST_PATH {
            ensure!(
                package_manifest_bytes.is_none(),
                "binary ZIP has multiple package manifests"
            );
            package_manifest_bytes = Some(read_small_entry(
                &mut entry,
                MAX_PACKAGE_MANIFEST_BYTES,
                BINARY_PACKAGE_MANIFEST_PATH,
            )?);
        }
        files.insert(relative);
    }

    let descriptor_bytes = descriptor_bytes
        .with_context(|| format!("binary ZIP is missing pkg/{BINARY_DESCRIPTOR_PATH}"))?;
    let descriptor: BinaryArtifactManifestV1 = serde_json::from_slice(&descriptor_bytes)
        .with_context(|| format!("parsing pkg/{BINARY_DESCRIPTOR_PATH}"))?;
    descriptor
        .validate()
        .map_err(|error| anyhow::anyhow!(error))?;
    let canonical_descriptor = descriptor
        .canonical_json_bytes()
        .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        descriptor_bytes == canonical_descriptor,
        "pkg/{BINARY_DESCRIPTOR_PATH} is not canonical JSON"
    );
    if let Some(expected) = expected_platform {
        ensure!(
            &descriptor.platform == expected,
            "binary platform mismatch: expected {}, archive contains {}",
            expected.target,
            descriptor.platform.target
        );
    }

    let manifest_bytes = package_manifest_bytes
        .with_context(|| format!("binary ZIP is missing pkg/{BINARY_PACKAGE_MANIFEST_PATH}"))?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .context("pkg/.zpkg.toml is not UTF-8")?;
    let manifest = Manifest::parse(manifest_text).context("parsing pkg/.zpkg.toml")?;
    manifest
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid pkg/.zpkg.toml: {error}"))?;
    ensure_descriptor_matches_manifest(&descriptor, &manifest)?;

    let descriptor_paths = descriptor
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    for archive_file in &files {
        if archive_file == BINARY_DESCRIPTOR_PATH {
            continue;
        }
        ensure!(
            descriptor_paths.contains(archive_file.as_str()),
            "binary ZIP contains unlisted payload file `{archive_file}`"
        );
    }
    for descriptor_file in &descriptor.files {
        ensure!(
            files.contains(&descriptor_file.path),
            "binary descriptor lists missing payload file `{}`",
            descriptor_file.path
        );
    }
    ensure!(
        descriptor.files.len().saturating_add(1) == files.len(),
        "binary descriptor/archive payload counts differ"
    );

    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for expected in &descriptor.files {
        let archive_name = format!("{BINARY_ARCHIVE_ROOT}/{}", expected.path);
        let mut entry = archive
            .by_name(&archive_name)
            .with_context(|| format!("opening `{archive_name}` for integrity verification"))?;
        if let Some(mode) = entry.unix_mode() {
            let executable = mode & 0o111 != 0;
            ensure!(
                executable == expected.executable,
                "payload `{}` executable mode disagrees with pkg/{BINARY_DESCRIPTOR_PATH}",
                expected.path
            );
        }
        let actual = hash_zip_entry(&mut entry, expected.size)?;
        ensure!(
            actual == expected.sha256,
            "payload digest mismatch for `{}`: expected {}, got {actual}",
            expected.path,
            expected.sha256
        );
    }

    Ok(VerifiedBinaryArtifact {
        manifest,
        descriptor,
        sha256,
        size,
        file_count: files.len(),
    })
}
