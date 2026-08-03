use super::*;

pub(super) fn worker_loop(
    queue: Arc<TaskQueue>,
    results: mpsc::Sender<FetchMessage>,
    registry_url: String,
    home: PathBuf,
) {
    let store = Store::new(&home);
    let mut registry: Option<Box<dyn Registry>> = None;

    while let Some(task) = queue.pop() {
        let sequence = task.sequence;
        let result = (|| -> Result<FetchResult> {
            if registry.is_none() {
                registry = Some(registry_for(&registry_url)?);
            }
            let registry = registry
                .as_deref()
                .context("recursive install worker has no registry")?;
            prefetch_one(registry, &store, task)
        })();
        if results.send(FetchMessage { sequence, result }).is_err() {
            return;
        }
    }
}

fn prefetch_one(registry: &dyn Registry, store: &Store, task: FetchTask) -> Result<FetchResult> {
    let (package_dir, downloaded) = ensure_artifact(registry, store, &task.version)
        .with_context(|| format!("prefetching {}@{}", task.key, task.version.version))?;
    let dependencies = read_manifest(&package_dir)
        .map(|manifest| manifest.dependencies)
        .unwrap_or_default();
    Ok(FetchResult {
        dependencies,
        downloaded,
    })
}

/// Acquire one immutable artifact through the shared cache/store path.
///
/// Every caller uses the same blocking per-hash process lock, staged download,
/// integrity check, atomic cache publication, and extraction sequence. This is
/// intentionally crate-visible so the legacy transactional installer cannot
/// bypass recursive-prefetch locking.
pub(crate) fn ensure_artifact(
    registry: &dyn Registry,
    store: &Store,
    version: &VersionMetadata,
) -> Result<(PathBuf, bool)> {
    validate_version_identity(version, &version.org, &version.name, &version.version)?;
    if store.has(&version.sha256) {
        return Ok((store.pkg_dir(&version.sha256), false));
    }

    let _artifact_lock = ArtifactProcessLock::acquire(store.home(), &version.sha256)?;
    if store.has(&version.sha256) {
        return Ok((store.pkg_dir(&version.sha256), false));
    }

    let cached = store.cached_artifact(&version.sha256);
    let mut downloaded = false;
    if !cached.is_file() {
        download_atomic(registry, version, &cached)?;
        downloaded = true;
    }

    match store.add_artifact(&cached, &version.sha256) {
        Ok(package_dir) => Ok((package_dir, downloaded)),
        Err(first_error) if !downloaded => {
            // A killed legacy client may have left a partial cache file. The
            // per-artifact lock makes removal and replacement safe.
            let _ = fs::remove_file(&cached);
            download_atomic(registry, version, &cached)?;
            store
                .add_artifact(&cached, &version.sha256)
                .with_context(|| {
                    format!("cached artifact was invalid ({first_error:#}); redownload also failed")
                })
                .map(|package_dir| (package_dir, true))
        }
        Err(error) => Err(error),
    }
}

fn download_atomic(
    registry: &dyn Registry,
    version: &VersionMetadata,
    cached: &Path,
) -> Result<()> {
    let cache_dir = cached.parent().context("cached artifact has a parent")?;
    fs::create_dir_all(cache_dir)?;
    let staging_dir = tempfile::tempdir_in(cache_dir)?;
    let staged = staging_dir.path().join("artifact.download");
    registry.download(version, &staged)?;

    let (actual_sha256, _) = sha256_file(&staged)?;
    if actual_sha256 != version.sha256 {
        bail!(
            "artifact hash mismatch: expected {}, got {}",
            version.sha256,
            actual_sha256
        );
    }

    if cached.exists() {
        let (cached_sha256, _) = sha256_file(cached)?;
        if cached_sha256 == version.sha256 {
            return Ok(());
        }
        fs::remove_file(cached)?;
    }
    match fs::rename(&staged, cached) {
        Ok(()) => Ok(()),
        Err(error) if cached.is_file() => {
            let (cached_sha256, _) = sha256_file(cached)?;
            if cached_sha256 == version.sha256 {
                Ok(())
            } else {
                Err(error).with_context(|| {
                    format!("publishing downloaded artifact to {}", cached.display())
                })
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("publishing downloaded artifact to {}", cached.display())),
    }
}

pub(super) fn split_key(key: &str) -> Result<(String, String)> {
    let mut parts = key.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(org), Some(name)) if !org.is_empty() && !name.is_empty() => {
            Ok((org.to_string(), name.to_string()))
        }
        _ => bail!("invalid package spec `{key}` (expected org/name)"),
    }
}

pub(super) fn validate_version_identity(
    version: &VersionMetadata,
    expected_org: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    if !is_slug(&version.org) || !is_slug(&version.name) {
        bail!(
            "registry returned invalid package identity `{}/{}`; refusing",
            version.org,
            version.name
        );
    }
    require_sha256(&version.sha256)?;
    if version.org != expected_org
        || version.name != expected_name
        || version.version != expected_version
    {
        bail!(
            "registry returned `{}/{}@{}` while resolving `{expected_org}/{expected_name}@{expected_version}`; refusing",
            version.org,
            version.name,
            version.version
        );
    }
    Ok(())
}

pub(super) fn resolve_version(
    registry: &dyn Registry,
    org: &str,
    name: &str,
    requirement_text: &str,
) -> Result<VersionMetadata> {
    let key = format!("{org}/{name}");
    let requirement = Requirement::parse(requirement_text);
    let package = registry.get_package(org, name)?;
    let mut candidates = package.versions.clone();
    let mut skipped_yanked = Vec::new();

    loop {
        let selected = version::resolve(&requirement, &candidates).map(str::to_string);
        let Some(selected) = selected else {
            if !skipped_yanked.is_empty() {
                bail!(
                    "the only version(s) of {key} satisfying `{requirement_text}` are yanked ({}); existing lockfiles keep working via `zed install --frozen`",
                    skipped_yanked.join(", ")
                );
            }
            bail!(
                "no version of {key} satisfies `{requirement_text}` (available: {})",
                package.versions.join(", ")
            );
        };
        let version = registry.get_version(org, name, &selected)?;
        validate_version_identity(&version, org, name, &selected)?;
        if version.yanked {
            candidates.retain(|candidate| *candidate != selected);
            skipped_yanked.push(selected);
            continue;
        }
        return Ok(version);
    }
}
