fn register(home: &Path, source: &Path) -> Result<Registration> {
    let canonical = canonical_source(source)?;
    let package = package_identity(&canonical)?;
    let source = path_to_utf8(&canonical, "local package source")?;
    let registration = Registration {
        schema_version: RECEIPT_SCHEMA_VERSION,
        package: package.clone(),
        source,
    };

    let registry = acquire_registry(home)?;
    let path = registration_path(&registry.root, &package)?;
    write_json_atomic(&path, &registration)?;
    Ok(registration)
}

fn unregister(home: &Path, package: &str) -> Result<()> {
    let package = normalize_key(package)?;
    let registry = acquire_registry(home)?;
    let path = registration_path(&registry.root, &package)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("local package `{package}` is not registered")
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    ensure!(
        metadata.file_type().is_file(),
        "refusing to remove non-regular local-link registration {}",
        path.display()
    );
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

fn consume(
    project: &Path,
    home: &Path,
    requested: &str,
    adapter: LocalLinkAdapter,
) -> Result<ConsumerReceipt> {
    let registration = read_registered(home, requested)?;
    let source = validate_registration_source(project, &registration)?;
    let project = canonical_source(project).context("consumer project must be a directory")?;
    let adapter = effective_adapter(&project, adapter);
    crate::project_lock::with_lock(&project, "link local package working tree", || {
        consume_locked(&project, &registration, &source, adapter)
    })
}

fn consume_locked(
    project: &Path,
    registration: &Registration,
    source: &Path,
    adapter: LocalLinkAdapter,
) -> Result<ConsumerReceipt> {
    let package = &registration.package;
    let receipt_path = consumer_receipt_path(project, package)?;
    if path_exists_no_follow(&receipt_path)? {
        unlink_consumer_locked(project, package)?;
    }

    let projections = projection_destinations(project, package, adapter)?;
    let mut applied = Vec::new();
    for (label, destination) in projections {
        match apply_projection(project, source, package, label, &destination) {
            Ok(receipt) => applied.push(receipt),
            Err(error) => {
                rollback_projections(project, source, &applied)?;
                return Err(error);
            }
        }
    }

    let receipt = ConsumerReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        package: package.clone(),
        source: path_to_utf8(source, "local package source")?,
        projections: applied,
    };

    if let Err(error) = write_json_atomic(&receipt_path, &receipt) {
        rollback_projections(project, source, &receipt.projections)
            .context("rolling back local links after receipt write failure")?;
        return Err(error).context("persisting local-link ownership receipt");
    }
    Ok(receipt)
}

fn unlink_consumer(project: &Path, package: &str) -> Result<()> {
    let package = normalize_key(package)?;
    let project = canonical_source(project).context("consumer project must be a directory")?;
    crate::project_lock::with_lock(&project, "unlink local package working tree", || {
        unlink_consumer_locked(&project, &package)
    })
}

fn unlink_consumer_locked(project: &Path, package: &str) -> Result<()> {
    let path = consumer_receipt_path(project, package)?;
    let receipt: ConsumerReceipt = read_json_regular(&path)
        .with_context(|| format!("no managed consumer link for `{package}`"))?;
    validate_consumer_receipt(&receipt, package)?;

    let source = PathBuf::from(&receipt.source);
    ensure!(
        source.is_absolute(),
        "consumer local-link receipt for `{package}` has a non-absolute source"
    );

    // Preflight every projection before mutating any of them. This prevents a
    // two-projection Node consumer from being half-restored if a different tool
    // changed one managed destination after Zed linked it.
    for projection in receipt.projections.iter().rev() {
        preflight_restore_projection(project, &source, projection)?;
    }
    for projection in receipt.projections.iter().rev() {
        restore_projection(project, &source, projection)?;
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

fn rollback_projections(
    project: &Path,
    source: &Path,
    projections: &[ProjectionReceipt],
) -> Result<()> {
    let mut first_error = None;
    for projection in projections.iter().rev() {
        if let Err(error) = restore_projection(project, source, projection) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error).context("rolling back partially applied local package link");
    }
    Ok(())
}

