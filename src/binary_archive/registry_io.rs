enum ResolvedBinaryMetadata {
    Legacy(Box<VersionMetadata>),
    Qualified(Box<BinaryArtifactMetadataV1>),
}

impl ResolvedBinaryMetadata {
    fn sha256(&self) -> &str {
        match self {
            Self::Legacy(metadata) => &metadata.sha256,
            Self::Qualified(metadata) => &metadata.sha256,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Self::Legacy(metadata) => metadata.size,
            Self::Qualified(metadata) => metadata.size,
        }
    }

    fn format_name(&self) -> &'static str {
        match self {
            Self::Legacy(metadata) => metadata.format.extension(),
            Self::Qualified(_) => "zip",
        }
    }
}

pub fn publish_binary_zip(
    cfg: &Config,
    result: &BinaryPackResult,
    dry_run: bool,
) -> Result<Option<PublishResponse>> {
    publish_binary_zip_with_route(cfg, result, dry_run, BinaryRegistryRoute::Legacy)
}

pub fn publish_binary_zip_with_route(
    cfg: &Config,
    result: &BinaryPackResult,
    dry_run: bool,
    route: BinaryRegistryRoute,
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
        mirrors: Vec::new(),
        published_at: None,
        signatures: Vec::new(),
    };
    let descriptor_sha256 = hex::encode(Sha256::digest(
        result
            .descriptor
            .canonical_json_bytes()
            .map_err(|error| anyhow::anyhow!(error))?,
    ));
    let qualified_meta = BinaryArtifactPublishMetaV1 {
        schema: BINARY_ARTIFACT_PUBLISH_META_SCHEMA_V1.to_owned(),
        manifest: result.manifest.clone(),
        platform: result.descriptor.platform.clone(),
        format: BinaryArchiveFormatV1::Zip,
        sha256: result.packed.sha256.clone(),
        size: result.packed.size,
        descriptor_sha256: descriptor_sha256.clone(),
        vcs_tag: meta.vcs_tag.clone(),
        vcs_commit: meta.vcs_commit.clone(),
        attachments: Vec::new(),
    };
    if route == BinaryRegistryRoute::Qualified {
        qualified_meta
            .validate()
            .map_err(|error| anyhow::anyhow!(error))?;
    }
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

    let registry = cfg.open_registry()?;
    match get_binary_version(
        registry.as_ref(),
        route,
        &identity.org,
        &identity.name,
        &identity.version,
        &result.descriptor.platform.target,
    ) {
        Ok(existing) => {
            validate_resolved_binary_metadata(
                &existing,
                &identity.org,
                &identity.name,
                &identity.version,
                &result.descriptor.platform.target,
            )?;
            if resolved_metadata_matches_publish(&existing, &meta, &qualified_meta)? {
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
            match route {
                BinaryRegistryRoute::Legacy => bail!(
                    "{}/{}@{} already has an immutable artifact (format {}, sha256 {}); the legacy registry route stores one artifact per version, so publish target `{}` with `--artifact-route qualified` rather than encoding it in SemVer metadata",
                    identity.org,
                    identity.name,
                    identity.version,
                    existing.format_name(),
                    existing.sha256(),
                    result.descriptor.platform.target
                ),
                BinaryRegistryRoute::Qualified => bail!(
                    "{}/{}@{} target `{}` already has a different immutable {} artifact (sha256 {})",
                    identity.org,
                    identity.name,
                    identity.version,
                    result.descriptor.platform.target,
                    existing.format_name(),
                    existing.sha256()
                ),
            }
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
    let response = match publish_binary_version(
        registry.as_ref(),
        route,
        &meta,
        &qualified_meta,
        &result.packed.path,
        token.as_deref(),
    ) {
        Ok(response) => response,
        Err(upload_error) => {
            // A server can commit the immutable object and lose the response.
            // Re-read the release before reporting failure so a retry is
            // idempotent across that ambiguous network boundary.
            match get_binary_version(
                registry.as_ref(),
                route,
                &identity.org,
                &identity.name,
                &identity.version,
                &result.descriptor.platform.target,
            ) {
                Ok(existing) => {
                    validate_resolved_binary_metadata(
                        &existing,
                        &identity.org,
                        &identity.name,
                        &identity.version,
                        &result.descriptor.platform.target,
                    )?;
                    if !resolved_metadata_matches_publish(&existing, &meta, &qualified_meta)? {
                        return Err(upload_error).context("publishing binary ZIP");
                    }
                    eprintln!(
                        "warning: publish response was lost, but the registry now contains the exact immutable binary ZIP; treating the operation as recovered"
                    );
                    PublishResponse {
                        org: identity.org.clone(),
                        name: identity.name.clone(),
                        version: identity.version.clone(),
                        sha256: meta.sha256.clone(),
                    }
                }
                _ => return Err(upload_error).context("publishing binary ZIP"),
            }
        }
    };
    ensure!(
        response.org == identity.org
            && response.name == identity.name
            && response.version == identity.version
            && response.sha256 == meta.sha256,
        "registry publish response does not match the uploaded binary artifact identity"
    );
    Ok(Some(response))
}

pub fn download_binary_zip(
    cfg: &Config,
    spec: &str,
    out_path: &Path,
    expected_target: Option<&str>,
) -> Result<VerifiedBinaryArtifact> {
    download_binary_zip_with_route(
        cfg,
        spec,
        out_path,
        expected_target,
        BinaryRegistryRoute::Legacy,
    )
}

pub fn download_binary_zip_with_route(
    cfg: &Config,
    spec: &str,
    out_path: &Path,
    expected_target: Option<&str>,
    route: BinaryRegistryRoute,
) -> Result<VerifiedBinaryArtifact> {
    let (org, name, version) = parse_exact_spec(spec)?;
    if route == BinaryRegistryRoute::Qualified {
        ensure!(
            expected_target.is_some(),
            "target-qualified downloads require --target"
        );
    }
    if let Some(target) = expected_target {
        validate_binary_target(target)?;
    }
    let registry = cfg.open_registry()?;
    let metadata = get_binary_version(
        registry.as_ref(),
        route,
        &org,
        &name,
        &version,
        expected_target.unwrap_or_default(),
    )?;
    validate_resolved_binary_metadata(
        &metadata,
        &org,
        &name,
        &version,
        expected_target.unwrap_or_default(),
    )?;
    let parent = out_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::tempdir_in(parent)?;
    let temporary_path = temporary.path().join("artifact.zip");
    match &metadata {
        ResolvedBinaryMetadata::Legacy(metadata) => registry.download(metadata, &temporary_path)?,
        ResolvedBinaryMetadata::Qualified(metadata) => {
            registry.download_binary_artifact(metadata, &temporary_path)?
        }
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary_path)
        .context("opening downloaded binary ZIP for synchronization")?
        .sync_all()
        .context("synchronizing downloaded binary ZIP before verification")?;
    let downloaded_size = fs::metadata(&temporary_path)
        .context("reading downloaded binary ZIP metadata")?
        .len();
    ensure!(
        downloaded_size <= max_binary_archive_bytes(),
        "registry downloaded {downloaded_size} bytes, above the {}-byte limit",
        max_binary_archive_bytes()
    );
    ensure!(
        downloaded_size == metadata.size(),
        "downloaded binary ZIP size mismatch: registry declared {}, got {downloaded_size}",
        metadata.size()
    );
    let (actual_sha, actual_size) = sha256_file(&temporary_path)?;
    ensure!(
        actual_sha == metadata.sha256(),
        "downloaded binary ZIP hash mismatch: registry declared {}, got {actual_sha}",
        metadata.sha256()
    );
    ensure!(
        actual_size == metadata.size(),
        "downloaded binary ZIP size mismatch: registry declared {}, got {actual_size}",
        metadata.size()
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
    if let ResolvedBinaryMetadata::Qualified(metadata) = &metadata {
        ensure!(
            verified.descriptor.platform == metadata.platform,
            "downloaded binary ZIP platform does not match qualified registry metadata"
        );
        let descriptor_sha256 = hex::encode(Sha256::digest(
            verified
                .descriptor
                .canonical_json_bytes()
                .map_err(|error| anyhow::anyhow!(error))?,
        ));
        ensure!(
            descriptor_sha256 == metadata.descriptor_sha256,
            "downloaded binary descriptor hash does not match qualified registry metadata"
        );
    }

    promote_verified_noclobber(
        &temporary_path,
        out_path,
        &verified.sha256,
        verified.size,
        "binary ZIP download",
    )?;
    Ok(verified)
}

fn get_binary_version(
    registry: &dyn crate::registry::Registry,
    route: BinaryRegistryRoute,
    org: &str,
    name: &str,
    version: &str,
    target: &str,
) -> Result<ResolvedBinaryMetadata> {
    match route {
        BinaryRegistryRoute::Legacy => registry
            .get_version(org, name, version)
            .map(Box::new)
            .map(ResolvedBinaryMetadata::Legacy),
        BinaryRegistryRoute::Qualified => registry
            .get_binary_artifact(org, name, version, target, BinaryArchiveFormatV1::Zip)
            .map(Box::new)
            .map(ResolvedBinaryMetadata::Qualified),
    }
}

fn publish_binary_version(
    registry: &dyn crate::registry::Registry,
    route: BinaryRegistryRoute,
    meta: &PublishMeta,
    qualified_meta: &BinaryArtifactPublishMetaV1,
    artifact: &Path,
    token: Option<&str>,
) -> Result<PublishResponse> {
    match route {
        BinaryRegistryRoute::Legacy => registry.publish(meta, artifact, token),
        BinaryRegistryRoute::Qualified => {
            let accepted = registry.publish_binary_artifact(qualified_meta, artifact, token)?;
            let resolved = ResolvedBinaryMetadata::Qualified(Box::new(accepted));
            validate_resolved_binary_metadata(
                &resolved,
                &meta.manifest.package.org,
                &meta.manifest.package.name,
                &meta.manifest.package.version,
                &qualified_meta.platform.target,
            )?;
            ensure!(
                resolved_metadata_matches_publish(&resolved, meta, qualified_meta)?,
                "registry accepted metadata does not match the uploaded binary artifact"
            );
            Ok(PublishResponse {
                org: meta.manifest.package.org.clone(),
                name: meta.manifest.package.name.clone(),
                version: meta.manifest.package.version.clone(),
                sha256: meta.sha256.clone(),
            })
        }
    }
}

fn validate_resolved_binary_metadata(
    metadata: &ResolvedBinaryMetadata,
    org: &str,
    name: &str,
    version: &str,
    target: &str,
) -> Result<()> {
    match metadata {
        ResolvedBinaryMetadata::Legacy(metadata) => {
            validate_binary_version_metadata(metadata)?;
            ensure_version_identity(metadata, org, name, version)?;
            ensure!(
                metadata.format == ArtifactFormat::Zip,
                "{org}/{name}@{version} is {}, not a binary ZIP artifact",
                metadata.format.extension()
            );
        }
        ResolvedBinaryMetadata::Qualified(metadata) => {
            metadata.validate()?;
            ensure!(
                metadata.org == org && metadata.name == name && metadata.version == version,
                "registry returned qualified binary metadata for a different release"
            );
            ensure!(
                metadata.platform.target == target,
                "registry returned qualified binary metadata for target `{}` while `{target}` was requested",
                metadata.platform.target
            );
            ensure!(
                metadata.size <= max_binary_archive_bytes(),
                "registry declares a binary artifact of {} bytes, above the {}-byte limit",
                metadata.size,
                max_binary_archive_bytes()
            );
        }
    }
    Ok(())
}

fn resolved_metadata_matches_publish(
    metadata: &ResolvedBinaryMetadata,
    legacy_publish: &PublishMeta,
    qualified_publish: &BinaryArtifactPublishMetaV1,
) -> Result<bool> {
    match metadata {
        ResolvedBinaryMetadata::Legacy(metadata) => Ok(metadata.sha256 == legacy_publish.sha256
            && metadata.size == legacy_publish.size
            && metadata.format == ArtifactFormat::Zip),
        ResolvedBinaryMetadata::Qualified(metadata) => qualified_publish
            .is_idempotent_with(metadata)
            .map_err(|error| anyhow::anyhow!(error)),
    }
}

fn registry_version_not_found(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("(404)") || message.contains("not found")
}
