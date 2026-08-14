fn ensure_descriptor_matches_manifest(
    descriptor: &BinaryArtifactManifestV1,
    manifest: &Manifest,
) -> Result<()> {
    ensure!(
        descriptor.package.org == manifest.package.org
            && descriptor.package.name == manifest.package.name
            && descriptor.package.version == manifest.package.version,
        "binary descriptor package identity does not match pkg/.zpkg.toml"
    );
    ensure!(
        descriptor.entrypoints == manifest.bin,
        "binary descriptor entrypoints do not exactly match pkg/.zpkg.toml [bin]"
    );
    if let Some(source) = &descriptor.source {
        ensure!(
            source.repository == manifest.package.repository.url,
            "binary descriptor source repository does not match pkg/.zpkg.toml"
        );
        ensure!(
            source.vcs_tag == manifest.vcs_tag(),
            "binary descriptor VCS tag does not match pkg/.zpkg.toml"
        );
    }
    Ok(())
}

fn read_small_entry<R: Read>(entry: &mut R, limit: u64, name: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut limited = entry.take(limit.saturating_add(1));
    limited.read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "ZIP entry `{name}` exceeds the {limit}-byte limit"
    );
    Ok(bytes)
}

fn hash_zip_entry<R: Read>(entry: &mut R, expected_size: u64) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = entry.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("ZIP entry size overflows u64")?;
        ensure!(
            size <= expected_size,
            "ZIP payload exceeds its descriptor-declared size"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        size == expected_size,
        "ZIP payload size mismatch: expected {expected_size}, got {size}"
    );
    Ok(hex::encode(hasher.finalize()))
}

fn require_zip_magic(file: &mut fs::File, path: &Path) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("{} is too short to be a ZIP", path.display()))?;
    ensure!(
        magic == *b"PK\x03\x04",
        "{} is not a canonical ZIP (self-extracting prefixes are not allowed)",
        path.display()
    );
    Ok(())
}

fn validate_matching_zip_headers(
    file: &mut fs::File,
    entries: &[(u64, u64, Vec<u8>)],
    ordinary_zip_limits_suffice: bool,
) -> Result<()> {
    const LOCAL_FIXED_BYTES: usize = 30;
    const CENTRAL_FIXED_BYTES: usize = 46;
    const ENCRYPTION_FLAGS: u16 = (1 << 0) | (1 << 6) | (1 << 13);

    for (local_offset, central_offset, parsed_name) in entries {
        file.seek(SeekFrom::Start(*local_offset))?;
        let mut local = [0_u8; LOCAL_FIXED_BYTES];
        file.read_exact(&mut local)
            .context("binary ZIP local header is truncated")?;
        ensure!(
            local.starts_with(b"PK\x03\x04"),
            "binary ZIP local header signature is invalid"
        );

        file.seek(SeekFrom::Start(*central_offset))?;
        let mut central = [0_u8; CENTRAL_FIXED_BYTES];
        file.read_exact(&mut central)
            .context("binary ZIP central header is truncated")?;
        ensure!(
            central.starts_with(b"PK\x01\x02"),
            "binary ZIP central header signature is invalid"
        );

        let local_flags = u16::from_le_bytes([local[6], local[7]]);
        let central_flags = u16::from_le_bytes([central[8], central[9]]);
        ensure!(
            local_flags == central_flags,
            "binary ZIP local and central header flags disagree"
        );
        ensure!(
            local_flags & ENCRYPTION_FLAGS == 0,
            "binary ZIP local header declares encryption"
        );
        ensure!(
            local_flags & (1 << 3) == 0,
            "binary ZIP data descriptors are not accepted because local-header sizes would be ambiguous"
        );
        ensure!(
            local[8..10] == central[10..12],
            "binary ZIP local and central compression methods disagree"
        );
        let local_name_len = u16::from_le_bytes([local[26], local[27]]) as usize;
        let central_name_len = u16::from_le_bytes([central[28], central[29]]) as usize;
        ensure!(
            local_name_len == parsed_name.len() && central_name_len == parsed_name.len(),
            "binary ZIP parsed and encoded filename lengths disagree"
        );
        let mut local_name = vec![0_u8; local_name_len];
        file.seek(SeekFrom::Start(
            local_offset.saturating_add(LOCAL_FIXED_BYTES as u64),
        ))?;
        file.read_exact(&mut local_name)
            .context("binary ZIP local filename is truncated")?;
        ensure!(
            local_name == *parsed_name,
            "binary ZIP local and central filenames disagree"
        );
        let mut central_name = vec![0_u8; central_name_len];
        file.seek(SeekFrom::Start(
            central_offset.saturating_add(CENTRAL_FIXED_BYTES as u64),
        ))?;
        file.read_exact(&mut central_name)
            .context("binary ZIP central filename is truncated")?;
        ensure!(
            central_name == *parsed_name,
            "binary ZIP parsed and encoded central filenames disagree"
        );
        ensure!(
            u16::from_le_bytes([central[34], central[35]]) == 0,
            "binary ZIP entry belongs to a different disk"
        );
        ensure!(
            local[14..26] == central[16..28],
            "binary ZIP local and central checksums or sizes disagree"
        );
        if ordinary_zip_limits_suffice {
            let local_extra_len = u16::from_le_bytes([local[28], local[29]]) as usize;
            let central_extra_len = u16::from_le_bytes([central[30], central[31]]) as usize;
            let mut local_extra = vec![0_u8; local_extra_len];
            file.seek(SeekFrom::Start(
                local_offset
                    .saturating_add(LOCAL_FIXED_BYTES as u64)
                    .saturating_add(local_name_len as u64),
            ))?;
            file.read_exact(&mut local_extra)
                .context("binary ZIP local extra fields are truncated")?;
            file.seek(SeekFrom::Start(
                central_offset
                    .saturating_add(CENTRAL_FIXED_BYTES as u64)
                    .saturating_add(central_name_len as u64),
            ))?;
            let mut central_extra = vec![0_u8; central_extra_len];
            file.read_exact(&mut central_extra)
                .context("binary ZIP central extra fields are truncated")?;
            ensure!(
                !zip_extra_contains_field(&local_extra, 0x0001)?
                    && !zip_extra_contains_field(&central_extra, 0x0001)?,
                "binary ZIP entry uses unnecessary ZIP64 metadata"
            );
        }
    }
    Ok(())
}

fn zip_extra_contains_field(extra: &[u8], wanted: u16) -> Result<bool> {
    let mut cursor = 0_usize;
    while cursor < extra.len() {
        ensure!(
            extra.len().saturating_sub(cursor) >= 4,
            "binary ZIP extra field header is truncated"
        );
        let id = u16::from_le_bytes([extra[cursor], extra[cursor + 1]]);
        let size = u16::from_le_bytes([extra[cursor + 2], extra[cursor + 3]]) as usize;
        cursor = cursor
            .checked_add(4)
            .and_then(|value| value.checked_add(size))
            .context("binary ZIP extra field length overflows")?;
        ensure!(cursor <= extra.len(), "binary ZIP extra field is truncated");
        if id == wanted {
            return Ok(true);
        }
    }
    Ok(false)
}

fn declared_zip_entry_count(file: &mut fs::File) -> Result<(usize, bool)> {
    const EOCD_MIN: usize = 22;
    const MAX_COMMENT: usize = u16::MAX as usize;
    let size = file.metadata()?.len();
    let tail_len = usize::try_from(size.min((EOCD_MIN + MAX_COMMENT) as u64))?;
    let tail_start = size.saturating_sub(tail_len as u64);
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)?;
    let eocd = (0..=tail.len().saturating_sub(EOCD_MIN))
        .rev()
        .find(|offset| {
            tail[*offset..].starts_with(b"PK\x05\x06")
                && tail
                    .get(*offset + 20..*offset + 22)
                    .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
                    .is_some_and(|comment| *offset + EOCD_MIN + comment == tail.len())
        })
        .context("binary ZIP is missing a canonical end-of-central-directory record")?;
    let disk = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let central_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let disk_entries = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let total_entries = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let central_size = u32::from_le_bytes(tail[eocd + 12..eocd + 16].try_into()?);
    let central_offset = u32::from_le_bytes(tail[eocd + 16..eocd + 20].try_into()?);
    let comment_len = u16::from_le_bytes([tail[eocd + 20], tail[eocd + 21]]);
    ensure!(
        comment_len == 0,
        "binary ZIP archive comments are not accepted in the canonical profile"
    );
    ensure!(
        disk == 0 && central_disk == 0,
        "multi-disk binary ZIPs are not supported"
    );
    ensure!(
        disk_entries == total_entries,
        "binary ZIP central-directory entry counts disagree"
    );
    let uses_zip64 = total_entries == u16::MAX
        || disk_entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX;
    let eocd_absolute = tail_start + eocd as u64;
    if !uses_zip64 {
        ensure!(
            u64::from(central_offset).checked_add(u64::from(central_size))
                == Some(eocd_absolute),
            "binary ZIP central directory is not contiguous with its end record"
        );
        return Ok((total_entries as usize, false));
    }

    ensure!(
        eocd_absolute >= 20,
        "binary ZIP64 archive is missing its locator"
    );
    file.seek(SeekFrom::Start(eocd_absolute - 20))?;
    let mut locator = [0_u8; 20];
    file.read_exact(&mut locator)?;
    ensure!(
        locator.starts_with(b"PK\x06\x07"),
        "binary ZIP64 archive is missing its locator"
    );
    let zip64_disk = u32::from_le_bytes(locator[4..8].try_into()?);
    let zip64_offset = u64::from_le_bytes(locator[8..16].try_into()?);
    let total_disks = u32::from_le_bytes(locator[16..20].try_into()?);
    ensure!(
        zip64_disk == 0 && total_disks == 1,
        "binary ZIP64 locator describes a multi-disk archive"
    );
    file.seek(SeekFrom::Start(zip64_offset))?;
    let mut zip64 = [0_u8; 56];
    file.read_exact(&mut zip64)?;
    ensure!(
        zip64.starts_with(b"PK\x06\x06"),
        "binary ZIP64 end record is invalid"
    );
    let record_size = u64::from_le_bytes(zip64[4..12].try_into()?);
    ensure!(record_size >= 44, "binary ZIP64 end record is truncated");
    ensure!(
        record_size == 44,
        "binary ZIP64 end record contains noncanonical extensible data"
    );
    ensure!(
        zip64_offset
            .checked_add(12)
            .and_then(|offset| offset.checked_add(record_size))
            == Some(eocd_absolute - 20),
        "binary ZIP64 end record does not end at its locator"
    );
    let disk = u32::from_le_bytes(zip64[16..20].try_into()?);
    let central_disk = u32::from_le_bytes(zip64[20..24].try_into()?);
    let disk_entries = u64::from_le_bytes(zip64[24..32].try_into()?);
    let total_entries = u64::from_le_bytes(zip64[32..40].try_into()?);
    let zip64_central_size = u64::from_le_bytes(zip64[40..48].try_into()?);
    let zip64_central_offset = u64::from_le_bytes(zip64[48..56].try_into()?);
    ensure!(
        disk == 0 && central_disk == 0 && disk_entries == total_entries,
        "binary ZIP64 is split across disks or has inconsistent entry counts"
    );
    ensure!(
        (u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]) == u16::MAX
            || u64::from(u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]])) == disk_entries)
            && (u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]) == u16::MAX
                || u64::from(u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]))
                    == total_entries),
        "binary ZIP and ZIP64 entry counts disagree"
    );
    ensure!(
        (central_size == u32::MAX || u64::from(central_size) == zip64_central_size)
            && (central_offset == u32::MAX
                || u64::from(central_offset) == zip64_central_offset),
        "binary ZIP and ZIP64 central-directory locations disagree"
    );
    ensure!(
        zip64_central_offset.checked_add(zip64_central_size) == Some(zip64_offset),
        "binary ZIP64 central directory is not contiguous with its end record"
    );
    Ok((
        usize::try_from(total_entries)
            .context("binary ZIP64 entry count exceeds this platform")?,
        true,
    ))
}

fn enforce_compression_ratio(name: &str, expanded: u64, compressed: u64) -> Result<()> {
    if expanded == 0 {
        return Ok(());
    }
    ensure!(compressed > 0, "ZIP entry `{name}` has zero compressed bytes");
    let ratio = expanded.saturating_add(compressed - 1) / compressed;
    ensure!(
        ratio <= max_binary_compression_ratio(),
        "ZIP entry `{name}` has compression ratio {ratio}:1, above the {}:1 limit",
        max_binary_compression_ratio()
    );
    Ok(())
}

fn parse_exact_spec(spec: &str) -> Result<(String, String, String)> {
    let (package, version) = spec
        .rsplit_once('@')
        .context("expected exact package spec org/name@version")?;
    ensure!(!version.trim().is_empty(), "package version is empty");
    let (org, name) = package
        .split_once('/')
        .context("expected exact package spec org/name@version")?;
    ensure!(
        is_slug(org) && is_slug(name),
        "invalid package identity `{package}`"
    );
    ensure!(
        version.len() <= 256
            && !matches!(version, "." | "..")
            && !version.bytes().any(|byte| {
                byte.is_ascii_control()
                    || byte.is_ascii_whitespace()
                    || matches!(byte, b'/' | b'\\' | b':' | b'?' | b'#' | b'%')
            }),
        "package version is not a safe registry path segment"
    );
    Ok((org.to_owned(), name.to_owned(), version.to_owned()))
}

fn validate_binary_target(target: &str) -> Result<()> {
    ensure!(
        !target.is_empty()
            && target.len() <= 128
            && target.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
            && target
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && target
                .as_bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "invalid binary target `{target}`"
    );
    Ok(())
}

fn validate_binary_version_metadata(metadata: &VersionMetadata) -> Result<()> {
    ensure!(
        metadata.sha256.len() == 64
            && metadata
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "registry returned invalid sha256 `{}`",
        metadata.sha256
    );
    ensure!(
        metadata.size <= max_binary_archive_bytes(),
        "registry declares an artifact of {} bytes, above the {}-byte limit",
        metadata.size,
        max_binary_archive_bytes()
    );
    Ok(())
}

fn ensure_version_identity(
    metadata: &VersionMetadata,
    org: &str,
    name: &str,
    version: &str,
) -> Result<()> {
    ensure!(
        metadata.org == org && metadata.name == name && metadata.version == version,
        "registry returned binary metadata for {}/{}@{} while {org}/{name}@{version} was requested",
        metadata.org,
        metadata.name,
        metadata.version,
    );
    Ok(())
}

fn portable_path_key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn validate_portable_archive_path(field: &str, path: &str) -> Result<()> {
    for component in path.split('/') {
        ensure!(
            component.len() <= 255,
            "{field} component exceeds the portable 255-byte limit: `{component}`"
        );
        ensure!(
            !component.ends_with(['.', ' ']),
            "{field} component has a trailing dot or space and is not portable: `{component}`"
        );
        ensure!(
            !component
                .chars()
                .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')),
            "{field} component contains a Windows-reserved character: `{component}`"
        );
        let device_stem = component
            .split_once('.')
            .map_or(component, |(stem, _)| stem)
            .to_ascii_uppercase();
        let numbered_device = device_stem
            .strip_prefix("COM")
            .or_else(|| device_stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            });
        ensure!(
            !matches!(
                device_stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$"
            ) && !numbered_device,
            "{field} component uses a Windows-reserved device name: `{component}`"
        );
    }
    Ok(())
}

fn validate_no_file_directory_collision(
    existing: &BTreeMap<String, bool>,
    portable: &str,
    original: &str,
    is_directory: bool,
) -> Result<()> {
    let mut ancestor = portable;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if existing.get(parent).is_some_and(|entry_is_dir| !entry_is_dir) {
            bail!(
                "ZIP entry `{original}` is nested beneath an existing file and cannot be extracted portably"
            );
        }
        ancestor = parent;
    }
    if !is_directory {
        let descendant_prefix = format!("{portable}/");
        ensure!(
            !existing.keys().any(|path| path.starts_with(&descendant_prefix)),
            "ZIP file entry `{original}` collides with an existing child path and cannot be extracted portably"
        );
    }
    Ok(())
}

fn max_binary_archive_bytes() -> u64 {
    env_u64(
        "ZED_PKG_MAX_BINARY_ARCHIVE_BYTES",
        DEFAULT_MAX_BINARY_ARCHIVE_BYTES,
    )
}

fn max_binary_expanded_bytes() -> u64 {
    env_u64(
        "ZED_PKG_MAX_BINARY_EXPANDED_BYTES",
        DEFAULT_MAX_BINARY_EXPANDED_BYTES,
    )
}

fn max_binary_entries() -> usize {
    std::env::var("ZED_PKG_MAX_BINARY_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_BINARY_ENTRIES)
}

fn max_binary_compression_ratio() -> u64 {
    env_u64(
        "ZED_PKG_MAX_BINARY_COMPRESSION_RATIO",
        DEFAULT_MAX_BINARY_COMPRESSION_RATIO,
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
