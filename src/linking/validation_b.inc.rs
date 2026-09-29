fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "managed symlink target must be absolute");
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(
                    normalized.pop(),
                    "managed symlink target escapes its filesystem root"
                );
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn path_to_utf8(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))
}

fn relative_utf8(project: &Path, path: &Path, label: &str) -> Result<String> {
    let relative = path
        .strip_prefix(project)
        .with_context(|| format!("{label} escapes project {}", project.display()))?;
    path_to_utf8(relative, label)
}

fn registration_path(root: &Path, package: &str) -> Result<PathBuf> {
    let (org, name) = key_parts(package)?;
    let org_dir = root.join(org);
    create_private_dir(&org_dir)?;
    let org_dir = org_dir.canonicalize()?;
    ensure!(
        org_dir.starts_with(root),
        "local-link registration directory escapes registry root"
    );
    Ok(org_dir.join(format!("{name}.json")))
}

fn consumer_receipt_path(project: &Path, package: &str) -> Result<PathBuf> {
    let (org, name) = key_parts(package)?;
    safe_contained_path(
        project,
        &project
            .join(CONSUMER_STATE_DIR)
            .join(org)
            .join(format!("{name}.json")),
        "consumer local-link receipt",
    )
}

fn backup_relative_path(package: &str, label: &str) -> Result<String> {
    let (org, name) = key_parts(package)?;
    ensure!(
        !label.contains('/') && !label.contains('\\'),
        "local-link backup label may not contain path separators"
    );
    Ok(format!("{CONSUMER_BACKUP_DIR}/{org}/{name}/{label}"))
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "managed directory {} must be a regular directory, not a symlink",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "managed directory {} must be a regular directory, not a symlink",
                path.display()
            );
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

fn read_json_regular<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading managed state {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "managed state {} must be a regular file",
        path.display()
    );
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn write_json_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let parent = path.parent().context("managed state path has no parent")?;
    create_private_dir(parent)?;

    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.file_type().is_file(),
            "refusing to replace non-regular managed state {}",
            path.display()
        );
    }

    let filename = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("managed state path has no UTF-8 file name")?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));
    let result = write_json_file(&temporary, value)
        .and_then(|()| replace_atomic(&temporary, path))
        .with_context(|| format!("committing managed state {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("serializing {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("syncing {}", path.display()))
}

fn replace_atomic(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if fs::symlink_metadata(destination).is_ok() {
        fs::remove_file(destination)
            .with_context(|| format!("removing old state {}", destination.display()))?;
    }
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "renaming {} to {}",
            temporary.display(),
            destination.display()
        )
    })?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing state directory {}", parent.display()))?;
    }
    Ok(())
}

fn create_directory_symlink(target: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("local-link destination has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating local-link parent {}", parent.display()))?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, destination).with_context(|| {
            format!(
                "creating live package link {} -> {}",
                destination.display(),
                target.display()
            )
        })
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, destination).with_context(|| {
            format!(
                "creating live package directory link {} -> {}; Windows may require Developer Mode or symlink privileges",
                destination.display(),
                target.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        let _ = destination;
        bail!("live local package links are unsupported on this platform")
    }
}

fn path_exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn is_explicit_path(raw: &str) -> bool {
    let path = Path::new(raw);
    path.is_absolute()
        || raw == "."
        || raw == ".."
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with(".\\")
        || raw.starts_with("..\\")
}

fn resolve_cli_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

