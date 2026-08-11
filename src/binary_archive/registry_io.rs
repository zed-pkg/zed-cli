pub fn publish_binary_zip(
    cfg: &Config,
    result: &BinaryPackResult,
    dry_run: bool,
) -> Result<Option<PublishResponse>> {
    let identity = &result.manifest.package;
    let meta = PublishMeta {
        manifest: result.manifest.clone(),
        vcs_tag: result.manifest.vcs_tag(),
        vcs_commit: result
            .descriptor
            .source
            .as_ref()
            .and_then(|source| source.vcs_commit.clone()),
        sha256: result.packed.sha256.clone(),
        size: result.packed.size,
        format: ArtifactFormat::Zip,
    };
    if dry_run {
        println!(
            "dry run: verified binary ZIP for {}/{}@{} target {} (sha256 {}, {} bytes); would upload to {}",
            identity.org,
            identity.name,
            identity.version,
            result.descriptor.platform.target,
            meta.sha256,
            meta.size,
            cfg.registry
        );
        return Ok(None);
    }

    let registry = registry_for(&cfg.registry)?;
    match registry.get_version(&identity.org, &identity.name, &identity.version) {
        Ok(existing) => {
            if existing.sha256 == meta.sha256 && existing.format == ArtifactFormat::Zip {
                println!(
                    "already published {}/{}@{} with identical binary ZIP sha256; skipping",
                    identity.org, identity.name, identity.version
                );
                return Ok(Some(PublishResponse {
                    org: identity.org.clone(),
                    name: identity.name.clone(),
                    version: identity.version.clone(),
                    sha256: meta.sha256,
                }));
            }
            bail!(
                "{}/{}@{} already has an immutable artifact (format {}, sha256 {}); the current registry route stores one artifact per version, so publishing target `{}` requires the artifact-qualified registry route rather than SemVer metadata",
                identity.org,
                identity.name,
                identity.version,
                existing.format.extension(),
                existing.sha256,
                result.descriptor.platform.target
            );
        }
        Err(error) if registry_version_not_found(&error) => {}
        Err(error) => {
            return Err(error).context("checking whether the binary release already exists");
        }
    }

    interactive::confirm(
        cfg.interactive,
        &format!(
            "publish binary ZIP {}/{}@{} for {} (sha256 {}) to {}",
            identity.org,
            identity.name,
            identity.version,
            result.descriptor.platform.target,
            meta.sha256,
            cfg.registry
        ),
    )?;
    let token = cfg.resolve_token()?;
    let response = registry.publish(&meta, &result.packed.path, token.as_deref())?;
    Ok(Some(response))
}

pub fn download_binary_zip(
    cfg: &Config,
    spec: &str,
    out_path: &Path,
    expected_target: Option<&str>,
) -> Result<VerifiedBinaryArtifact> {
    let (org, name, version) = parse_exact_spec(spec)?;
    let registry = registry_for(&cfg.registry)?;
    let metadata = registry.get_version(&org, &name, &version)?;
    validate_binary_version_metadata(&metadata)?;
    ensure!(
        metadata.format == ArtifactFormat::Zip,
        "{org}/{name}@{version} is {}, not a binary ZIP artifact",
        metadata.format.extension()
    );
    let parent = out_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::tempdir_in(parent)?;
    let temporary_path = temporary.path().join("artifact.zip");
    registry.download(&metadata, &temporary_path)?;
    let downloaded_size = fs::metadata(&temporary_path)
        .context("reading downloaded binary ZIP metadata")?
        .len();
    ensure!(
        downloaded_size <= max_binary_archive_bytes(),
        "registry downloaded {downloaded_size} bytes, above the {}-byte limit",
        max_binary_archive_bytes()
    );
    ensure!(
        downloaded_size == metadata.size,
        "downloaded binary ZIP size mismatch: registry declared {}, got {downloaded_size}",
        metadata.size
    );
    let (actual_sha, actual_size) = sha256_file(&temporary_path)?;
    ensure!(
        actual_sha == metadata.sha256,
        "downloaded binary ZIP hash mismatch: registry declared {}, got {actual_sha}",
        metadata.sha256
    );
    ensure!(
        actual_size == metadata.size,
        "downloaded binary ZIP size mismatch: registry declared {}, got {actual_size}",
        metadata.size
    );
    let verified = verify_binary_zip(&temporary_path, None)?;
    ensure!(
        verified.manifest.package.org == org
            && verified.manifest.package.name == name
            && verified.manifest.package.version == version,
        "downloaded binary ZIP package identity does not match {org}/{name}@{version}"
    );
    if let Some(target) = expected_target {
        ensure!(
            verified.descriptor.platform.target == target,
            "downloaded target mismatch: expected {target}, got {}",
            verified.descriptor.platform.target
        );
    }

    if out_path.exists() {
        let (existing_sha, _) = sha256_file(out_path)?;
        if existing_sha == verified.sha256 {
            return Ok(verified);
        }
        fs::remove_file(out_path)
            .with_context(|| format!("replacing existing {}", out_path.display()))?;
    }
    fs::rename(&temporary_path, out_path)
        .with_context(|| format!("promoting verified download to {}", out_path.display()))?;
    Ok(verified)
}

fn registry_version_not_found(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("(404)") || message.contains("not found")
}
