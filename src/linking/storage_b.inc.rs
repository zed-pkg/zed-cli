fn apply_projection(
    project: &Path,
    source: &Path,
    package: &str,
    label: &str,
    lexical_destination: &Path,
) -> Result<ProjectionReceipt> {
    let source = canonical_source(source)?;
    let destination = safe_projection_destination(project, &source, lexical_destination)?;
    let destination_relative = relative_utf8(project, &destination, "local-link destination")?;
    let backup_relative = backup_relative_path(package, label)?;
    let previous = detach_previous(project, &destination, &backup_relative)?;

    if let Err(error) = create_directory_symlink(&source, &destination) {
        restore_previous(project, &destination, &previous)?;
        return Err(error);
    }

    Ok(ProjectionReceipt {
        destination: destination_relative,
        previous,
    })
}

fn preflight_restore_projection(
    project: &Path,
    source: &Path,
    projection: &ProjectionReceipt,
) -> Result<()> {
    let destination = safe_state_relative(project, &projection.destination, "projection destination")?;
    verify_expected_live_link(&destination, source)?;
    if let PreviousState::Directory { backup } = &projection.previous {
        let backup = safe_state_relative(project, backup, "local-link backup")?;
        let metadata = fs::symlink_metadata(&backup).with_context(|| {
            format!(
                "local-link backup {} is missing; refusing destructive unlink",
                backup.display()
            )
        })?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "local-link backup {} is not a regular directory",
            backup.display()
        );
    }
    Ok(())
}

fn restore_projection(
    project: &Path,
    source: &Path,
    projection: &ProjectionReceipt,
) -> Result<()> {
    let destination = safe_state_relative(project, &projection.destination, "projection destination")?;
    remove_expected_live_link(&destination, source)?;
    restore_previous(project, &destination, &projection.previous)
}

fn detach_previous(
    project: &Path,
    destination: &Path,
    backup_relative: &str,
) -> Result<PreviousState> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PreviousState::Absent);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading existing {}", destination.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(destination)
            .with_context(|| format!("reading symlink {}", destination.display()))?;
        let target = path_to_utf8(&target, "previous package symlink target")?;
        fs::remove_file(destination)
            .with_context(|| format!("detaching previous symlink {}", destination.display()))?;
        return Ok(PreviousState::Symlink { target });
    }

    if metadata.is_dir() {
        let backup = safe_state_relative(project, backup_relative, "local-link backup")?;
        if path_exists_no_follow(&backup)? {
            bail!(
                "local-link backup already exists at {}; run `zed unlink` or inspect stale state before retrying",
                backup.display()
            );
        }
        if let Some(parent) = backup.parent() {
            create_private_dir(parent)?;
        }
        fs::rename(destination, &backup).with_context(|| {
            format!(
                "moving existing package directory {} to reversible local-link backup {}",
                destination.display(),
                backup.display()
            )
        })?;
        return Ok(PreviousState::Directory {
            backup: backup_relative.to_string(),
        });
    }

    bail!(
        "refusing to replace non-directory package path {} with a local link",
        destination.display()
    )
}

fn restore_previous(
    project: &Path,
    destination: &Path,
    previous: &PreviousState,
) -> Result<()> {
    match previous {
        PreviousState::Absent => Ok(()),
        PreviousState::Symlink { target } => {
            ensure_destination_absent(destination)?;
            create_directory_symlink(Path::new(target), destination)
                .with_context(|| format!("restoring previous symlink {}", destination.display()))
        }
        PreviousState::Directory { backup } => {
            ensure_destination_absent(destination)?;
            let backup = safe_state_relative(project, backup, "local-link backup")?;
            let metadata = fs::symlink_metadata(&backup).with_context(|| {
                format!(
                    "local-link backup {} is missing; refusing destructive unlink",
                    backup.display()
                )
            })?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "local-link backup {} is not a regular directory",
                backup.display()
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&backup, destination).with_context(|| {
                format!(
                    "restoring package directory {} from {}",
                    destination.display(),
                    backup.display()
                )
            })
        }
    }
}

fn verify_expected_live_link(destination: &Path, expected_source: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(destination).with_context(|| {
        format!(
            "managed local-link destination {} is missing; refusing to restore over unknown state",
            destination.display()
        )
    })?;
    ensure!(
        metadata.file_type().is_symlink(),
        "managed local-link destination {} is no longer a symlink; refusing to remove it",
        destination.display()
    );

    let raw = fs::read_link(destination)
        .with_context(|| format!("reading managed symlink {}", destination.display()))?;
    let resolved = if raw.is_absolute() {
        raw
    } else {
        destination
            .parent()
            .context("managed symlink has no parent")?
            .join(raw)
    };

    let resolved = lexical_normalize(&resolved)?;
    let expected = lexical_normalize(expected_source)?;
    ensure!(
        resolved == expected,
        "managed local-link destination {} now points to {} instead of {}; refusing to remove a path changed by another tool",
        destination.display(),
        resolved.display(),
        expected.display()
    );
    Ok(())
}

fn remove_expected_live_link(destination: &Path, source: &Path) -> Result<()> {
    verify_expected_live_link(destination, source)?;
    fs::remove_file(destination)
        .with_context(|| format!("removing managed symlink {}", destination.display()))
}

fn ensure_destination_absent(destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => bail!(
            "cannot restore previous package state because {} is occupied",
            destination.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", destination.display())),
    }
}

fn projection_destinations(
    project: &Path,
    package: &str,
    adapter: LocalLinkAdapter,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let (org, name) = key_parts(package)?;
    let mut destinations = vec![(
        "zed_modules",
        project.join(MODULES_DIR).join(org).join(name),
    )];
    if adapter == LocalLinkAdapter::Node {
        destinations.push((
            "node_modules",
            project
                .join("node_modules")
                .join(format!("@{org}"))
                .join(name),
        ));
    }
    Ok(destinations)
}

fn effective_adapter(project: &Path, adapter: LocalLinkAdapter) -> LocalLinkAdapter {
    match adapter {
        LocalLinkAdapter::Auto if project.join("package.json").is_file() => LocalLinkAdapter::Node,
        LocalLinkAdapter::Auto => LocalLinkAdapter::None,
        other => other,
    }
}

struct RegistryGuard {
    root: PathBuf,
    _guard: LockGuard,
}

