pub fn pack_binary_zip(project: &Path, options: &BinaryPackOptions) -> Result<BinaryPackResult> {
    let project = project
        .canonicalize()
        .with_context(|| format!("resolving project root {}", project.display()))?;
    let manifest = read_manifest(&project)?;
    let packed = pack_binary_zip_with_manifest(&project, &manifest, options)?;
    ensure!(
        read_manifest(&project)? == manifest,
        "{MANIFEST_FILE} changed while the binary ZIP was being packed"
    );
    Ok(packed)
}

/// Pack using the exact manifest instance whose release provenance the caller
/// already checked. This prevents a concurrent `.zpkg.toml` replacement from
/// changing package identity between tag verification and archive creation.
pub fn pack_binary_zip_with_manifest(
    project: &Path,
    manifest: &Manifest,
    options: &BinaryPackOptions,
) -> Result<BinaryPackResult> {
    let project = project
        .canonicalize()
        .with_context(|| format!("resolving project root {}", project.display()))?;
    let manifest = manifest.clone();
    manifest
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid {MANIFEST_FILE}: {error}"))?;
    options
        .platform
        .validate()
        .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        !manifest.bin.is_empty(),
        "binary publishing requires at least one [bin] entry in {MANIFEST_FILE}"
    );

    let manifest_bytes = manifest.to_toml_string()?.into_bytes();
    let mut files = BTreeMap::<String, CollectedFile>::new();
    let mut portable_paths = BTreeMap::<String, String>::new();
    insert_bytes(
        &mut files,
        &mut portable_paths,
        BINARY_PACKAGE_MANIFEST_PATH,
        manifest_bytes,
        false,
    )?;

    let entrypoint_paths = manifest.bin.values().cloned().collect::<BTreeSet<_>>();
    for (command, relative) in &manifest.bin {
        validate_safe_relative_path(&format!("[bin].{command}"), relative)
            .map_err(|error| anyhow::anyhow!(error))?;
        ensure!(
            relative != BINARY_DESCRIPTOR_PATH,
            "[bin].{command} cannot point at reserved {BINARY_DESCRIPTOR_PATH}"
        );
        let source = resolve_project_input(&project, Path::new(relative))?;
        ensure!(
            source.is_file(),
            "[bin].{command} points at missing or non-file {}",
            source.display()
        );
        insert_source_file(
            &mut files,
            &mut portable_paths,
            relative,
            source,
            true,
        )?;
    }

    for include in &options.includes {
        collect_explicit_include(
            &project,
            include,
            &entrypoint_paths,
            &mut files,
            &mut portable_paths,
        )?;
    }
    collect_root_legal_files(
        &project,
        &entrypoint_paths,
        &mut files,
        &mut portable_paths,
    )?;

    let expanded_size = files.values().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size)
            .context("binary payload expanded size overflows u64")
    })?;
    ensure!(
        expanded_size <= max_binary_expanded_bytes(),
        "binary payload expands to {expanded_size} bytes, above the {}-byte limit",
        max_binary_expanded_bytes()
    );
    ensure!(
        files.len().saturating_add(1) <= max_binary_entries(),
        "binary artifact has {} files, above the {}-entry limit",
        files.len().saturating_add(1),
        max_binary_entries()
    );

    let descriptor = BinaryArtifactManifestV1 {
        schema: BINARY_ARTIFACT_SCHEMA_V1.to_owned(),
        package: BinaryPackageIdentityV1 {
            org: manifest.package.org.clone(),
            name: manifest.package.name.clone(),
            version: manifest.package.version.clone(),
        },
        platform: options.platform.clone(),
        format: BinaryArchiveFormatV1::Zip,
        package_manifest: BINARY_PACKAGE_MANIFEST_PATH.to_owned(),
        expanded_size,
        files: files
            .values()
            .map(|file| BinaryFileV1 {
                path: file.path.clone(),
                sha256: file.sha256.clone(),
                size: file.size,
                executable: file.executable,
            })
            .collect(),
        entrypoints: manifest.bin.clone(),
        source: Some(BinarySourceProvenanceV1 {
            repository: manifest.package.repository.url.clone(),
            vcs_tag: manifest.vcs_tag(),
            vcs_commit: options.vcs_commit.clone(),
        }),
    };
    let descriptor_bytes = descriptor
        .canonical_json_bytes()
        .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        descriptor_bytes.len() as u64 <= MAX_DESCRIPTOR_BYTES,
        "generated {BINARY_DESCRIPTOR_PATH} exceeds {MAX_DESCRIPTOR_BYTES} bytes"
    );

    let out_dir = options
        .out_dir
        .clone()
        .unwrap_or_else(|| project.join(PACK_OUT_DIR));
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating binary output directory {}", out_dir.display()))?;
    let file_name = format!(
        "{}-{}-{}-{}.zip",
        manifest.package.org,
        manifest.package.name,
        manifest.package.version,
        descriptor.platform.target
    );
    let out_path = out_dir.join(file_name);
    let temporary = tempfile::tempdir_in(&out_dir)
        .context("creating temporary binary ZIP directory")?;
    let temporary_path = temporary.path().join("artifact.zip");
    write_binary_zip(&temporary_path, &files, &descriptor_bytes)?;

    let verified = verify_binary_zip(&temporary_path, Some(&descriptor.platform))?;
    ensure!(
        verified.descriptor == descriptor,
        "binary ZIP verification returned a descriptor different from the generated descriptor"
    );
    ensure!(
        verified.manifest == manifest,
        "binary ZIP verification returned a package manifest different from the generated manifest"
    );
    ensure!(
        verified.size <= max_binary_archive_bytes(),
        "binary ZIP is {} bytes, above the {}-byte limit",
        verified.size,
        max_binary_archive_bytes()
    );

    promote_verified_noclobber(
        &temporary_path,
        &out_path,
        &verified.sha256,
        verified.size,
        "binary ZIP",
    )?;

    Ok(BinaryPackResult {
        manifest,
        descriptor,
        packed: PackResult {
            path: out_path,
            sha256: verified.sha256,
            size: verified.size,
            file_count: verified.file_count,
            excluded_count: 0,
            format: ArtifactFormat::Zip,
        },
    })
}
