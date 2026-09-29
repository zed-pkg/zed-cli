fn validate_registration_source(project: &Path, registration: &Registration) -> Result<PathBuf> {
    let mut raw = BTreeMap::new();
    raw.insert(registration.package.clone(), registration.source.clone());
    let resolved = crate::local_overrides::resolve(project, MODULES_DIR, &raw)?;
    let source = resolved
        .get(&registration.package)
        .context("local-link source resolution omitted registered package")?
        .clone();
    validate_registration_identity(registration, &source)?;
    Ok(source)
}

fn validate_registration_source_without_project(registration: &Registration) -> Result<PathBuf> {
    let source = canonical_source(Path::new(&registration.source))?;
    validate_registration_identity(registration, &source)?;
    Ok(source)
}

fn validate_registration_identity(registration: &Registration, source: &Path) -> Result<()> {
    let actual = package_identity(source)?;
    ensure!(
        actual == registration.package,
        "registered source {} now declares `{actual}` instead of `{}`",
        source.display(),
        registration.package
    );
    Ok(())
}

fn validate_registration_shape(registration: &Registration, expected: &str) -> Result<()> {
    ensure!(
        registration.schema_version == RECEIPT_SCHEMA_VERSION,
        "unsupported local-link registration schema {}",
        registration.schema_version
    );
    let package = normalize_key(&registration.package)?;
    ensure!(
        package == expected,
        "local-link registration identity `{package}` does not match registry path `{expected}`"
    );
    let source = Path::new(&registration.source);
    ensure!(
        source.is_absolute(),
        "local-link registration `{expected}` has a non-absolute source"
    );
    Ok(())
}

fn validate_consumer_receipt(receipt: &ConsumerReceipt, expected: &str) -> Result<()> {
    ensure!(
        receipt.schema_version == RECEIPT_SCHEMA_VERSION,
        "unsupported consumer local-link receipt schema {}",
        receipt.schema_version
    );
    ensure!(
        receipt.package == expected,
        "consumer local-link receipt for `{}` does not match requested `{expected}`",
        receipt.package
    );
    ensure!(
        !receipt.projections.is_empty(),
        "consumer local-link receipt for `{expected}` has no managed projections"
    );
    ensure!(
        Path::new(&receipt.source).is_absolute(),
        "consumer local-link receipt for `{expected}` has a non-absolute source"
    );
    Ok(())
}

fn package_identity(source: &Path) -> Result<String> {
    let manifest = source.join(MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&manifest)
        .with_context(|| format!("local package has no {}", manifest.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "local package manifest {} must be a regular file",
        manifest.display()
    );
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let document: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let package = document
        .get("package")
        .and_then(toml::Value::as_table)
        .context("local package manifest must contain [package]")?;
    let org = package
        .get("org")
        .and_then(toml::Value::as_str)
        .context("[package].org must be a string")?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .context("[package].name must be a string")?;
    normalize_key(&format!("{org}/{name}"))
}

fn normalize_key(raw: &str) -> Result<String> {
    let key = raw.trim().strip_prefix('@').unwrap_or(raw.trim());
    crate::ops::split_key(key)?;
    Ok(key.to_string())
}

fn key_parts(key: &str) -> Result<(&str, &str)> {
    crate::ops::split_key(key)?;
    key.split_once('/')
        .context("package identity must use org/name")
}

fn canonical_source(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );
    Ok(canonical)
}

fn safe_projection_destination(project: &Path, source: &Path, destination: &Path) -> Result<PathBuf> {
    let destination = safe_contained_path(project, destination, "local-link destination")?;
    ensure!(
        source != destination && !source.starts_with(&destination) && !destination.starts_with(source),
        "local-link source {} overlaps destination {}; refusing self-linking or recursive ownership",
        source.display(),
        destination.display()
    );
    Ok(destination)
}

fn safe_state_relative(project: &Path, raw: &str, label: &str) -> Result<PathBuf> {
    let relative = Path::new(raw);
    ensure!(
        !relative.is_absolute(),
        "managed local-link path must be project-relative: {raw}"
    );
    ensure!(
        !relative
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "managed local-link path may not contain `..`: {raw}"
    );
    safe_contained_path(project, &project.join(relative), label)
}

fn safe_contained_path(project: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    let project = project
        .canonicalize()
        .with_context(|| format!("canonicalizing project {}", project.display()))?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    };
    ensure!(
        absolute.starts_with(&project),
        "{label} {} escapes consumer project {}",
        absolute.display(),
        project.display()
    );

    let name = absolute.file_name().context("managed path has no file name")?;
    let parent = absolute.parent().context("managed path has no parent")?;
    let canonical_parent = canonicalize_existing_ancestor(parent, &project, label)?;
    let resolved = canonical_parent.join(name);
    ensure!(
        resolved.starts_with(&project),
        "{label} {} escapes consumer project {} through a symlinked parent",
        resolved.display(),
        project.display()
    );
    Ok(resolved)
}

fn canonicalize_existing_ancestor(parent: &Path, root: &Path, label: &str) -> Result<PathBuf> {
    let mut existing = parent;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = existing.file_name().with_context(|| {
                    format!("cannot locate an existing ancestor for {label} {}", parent.display())
                })?;
                missing.push(component.to_os_string());
                existing = existing.parent().with_context(|| {
                    format!("cannot locate an existing ancestor for {label} {}", parent.display())
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading {label} ancestor {}", existing.display())
                });
            }
        }
    }

    let mut canonical = existing
        .canonicalize()
        .with_context(|| format!("canonicalizing {label} ancestor {}", existing.display()))?;
    ensure!(
        canonical.starts_with(root),
        "{label} ancestor {} escapes root {}",
        canonical.display(),
        root.display()
    );
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

