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

fn require_zip_magic(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)?;
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
        !org.is_empty() && !name.is_empty(),
        "invalid package identity `{package}`"
    );
    Ok((org.to_owned(), name.to_owned(), version.to_owned()))
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

fn portable_path_key(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

#[cfg(unix)]
fn file_is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn file_is_executable(_path: &Path) -> Result<bool> {
    Ok(false)
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
