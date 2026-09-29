fn acquire_registry(home: &Path) -> Result<RegistryGuard> {
    let root = canonical_registry_root(home)?;
    let lock_path = root.join(LINKS_LOCK_FILE);
    let guard = LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&lock_path)
                .operation("local package link registry")
                .class(LockClass::Custom(6))
                .queue_same_process(),
        )
        .context("locking local package link registry")?;
    Ok(RegistryGuard {
        root,
        _guard: guard,
    })
}

fn canonical_registry_root(home: &Path) -> Result<PathBuf> {
    fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let home = home
        .canonicalize()
        .with_context(|| format!("canonicalizing Zed home {}", home.display()))?;
    let root = home.join(LINKS_DIR);
    create_private_dir(&root)?;
    let metadata = fs::symlink_metadata(&root)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "local-link registry root {} must be a regular directory",
        root.display()
    );
    root.canonicalize()
        .with_context(|| format!("canonicalizing local-link registry {}", root.display()))
}

fn read_registered(home: &Path, requested: &str) -> Result<Registration> {
    let registry = acquire_registry(home)?;
    let package = normalize_or_resolve_key_locked(&registry.root, requested)?;
    let path = registration_path(&registry.root, &package)?;
    let registration: Registration = read_json_regular(&path)
        .with_context(|| format!("local package `{package}` is not registered"))?;
    validate_registration_shape(&registration, &package)?;
    Ok(registration)
}

fn resolve_registered_key(home: &Path, requested: &str) -> Result<String> {
    let registry = acquire_registry(home)?;
    normalize_or_resolve_key_locked(&registry.root, requested)
}

fn resolve_consumer_key(project: &Path, home: &Path, requested: &str) -> Result<String> {
    let trimmed = requested.trim();
    if trimmed.contains('/') {
        return normalize_key(trimmed);
    }
    ensure!(!trimmed.is_empty(), "local package name cannot be empty");

    let project = canonical_source(project).context("consumer project must be a directory")?;
    let local_matches = consumer_receipt_keys(&project)?
        .into_iter()
        .filter(|key| key_parts(key).is_ok_and(|(_, name)| name == trimmed))
        .collect::<Vec<_>>();
    match local_matches.as_slice() {
        [only] => return Ok(only.clone()),
        [] => {}
        _ => {
            bail!(
                "bare consumer local-link name `{trimmed}` is ambiguous: {}; use `org/name`",
                local_matches.join(", ")
            )
        }
    }

    resolve_registered_key(home, trimmed)
}

fn normalize_or_resolve_key_locked(root: &Path, requested: &str) -> Result<String> {
    let trimmed = requested.trim();
    if trimmed.contains('/') {
        return normalize_key(trimmed);
    }
    ensure!(!trimmed.is_empty(), "local package name cannot be empty");

    let matches = registration_keys_locked(root)?
        .into_iter()
        .filter(|key| key_parts(key).is_ok_and(|(_, name)| name == trimmed))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!(
            "no registered local package has bare name `{trimmed}`; use `zed links` or an `org/name` identity"
        ),
        _ => bail!(
            "bare local package name `{trimmed}` is ambiguous: {}; use `org/name`",
            matches.join(", ")
        ),
    }
}

fn list_registrations(home: &Path) -> Result<Vec<RegistrationStatus>> {
    let registry = acquire_registry(home)?;
    let mut statuses = Vec::new();
    for package in registration_keys_locked(&registry.root)? {
        let path = registration_path(&registry.root, &package)?;
        let registration: Registration = read_json_regular(&path)?;
        validate_registration_shape(&registration, &package)?;
        let status = match validate_registration_source_without_project(&registration) {
            Ok(_) => "ok".to_string(),
            Err(error) => format!("invalid: {error:#}"),
        };
        statuses.push(RegistrationStatus {
            package,
            source: registration.source,
            status,
        });
    }
    Ok(statuses)
}

fn registration_keys_locked(root: &Path) -> Result<Vec<String>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    let mut orgs = fs::read_dir(root)
        .with_context(|| format!("reading local-link registry {}", root.display()))?
        .collect::<io::Result<Vec<_>>>()?;
    orgs.sort_by_key(fs::DirEntry::file_name);

    for org_entry in orgs {
        if keys.len() >= MAX_REGISTERED_LINKS {
            bail!("local-link registry exceeds {MAX_REGISTERED_LINKS} registrations");
        }
        let metadata = fs::symlink_metadata(org_entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let org = org_entry.file_name().to_string_lossy().into_owned();
        let mut packages = fs::read_dir(org_entry.path())?.collect::<io::Result<Vec<_>>>()?;
        packages.sort_by_key(fs::DirEntry::file_name);
        for package_entry in packages {
            if keys.len() >= MAX_REGISTERED_LINKS {
                bail!("local-link registry exceeds {MAX_REGISTERED_LINKS} registrations");
            }
            let metadata = fs::symlink_metadata(package_entry.path())?;
            if !metadata.file_type().is_file() {
                continue;
            }
            let filename = package_entry.file_name().to_string_lossy().into_owned();
            let Some(name) = filename.strip_suffix(".json") else {
                continue;
            };
            let key = format!("{org}/{name}");
            normalize_key(&key)?;
            keys.push(key);
        }
    }
    keys.sort();
    Ok(keys)
}

fn consumer_receipt_keys(project: &Path) -> Result<Vec<String>> {
    let root = safe_contained_path(
        project,
        &project.join(CONSUMER_STATE_DIR),
        "consumer local-link state root",
    )?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    let mut orgs = fs::read_dir(&root)?.collect::<io::Result<Vec<_>>>()?;
    orgs.sort_by_key(fs::DirEntry::file_name);
    for org_entry in orgs {
        let metadata = fs::symlink_metadata(org_entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let org = org_entry.file_name().to_string_lossy().into_owned();
        let mut receipts = fs::read_dir(org_entry.path())?.collect::<io::Result<Vec<_>>>()?;
        receipts.sort_by_key(fs::DirEntry::file_name);
        for receipt in receipts {
            let metadata = fs::symlink_metadata(receipt.path())?;
            if !metadata.file_type().is_file() {
                continue;
            }
            let filename = receipt.file_name().to_string_lossy().into_owned();
            let Some(name) = filename.strip_suffix(".json") else {
                continue;
            };
            let key = format!("{org}/{name}");
            normalize_key(&key)?;
            keys.push(key);
        }
    }
    keys.sort();
    Ok(keys)
}

