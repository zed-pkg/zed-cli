use super::artifact::validate_version_identity;
use super::solver::{PreparedInstall, solve_install};
use super::*;

/// Solve the complete active constraint set before the transactional installer
/// mutates project output. Candidate manifests are still loaded through the
/// bounded worker pool and the shared per-artifact process locks.
pub(crate) fn prepare(
    project: &Path,
    cfg: &Config,
    local_mode: LocalRegistryMode,
) -> Result<PreparedInstall> {
    let concurrency = install_concurrency();
    let context = cfg.mirror_context(project_trust_anchors(project));
    let registry = context.open(&cfg.registry)?;
    let manifest = read_manifest(project)?;
    // Solving is where the network would be contacted, so the local registry
    // has to be visible here and not only to the materializer downstream.
    let local = local_index_for_solving(cfg, local_mode)?;
    let prepared = if manifest.dependencies.is_empty() {
        PreparedInstall::default()
    } else {
        run_with_pool(cfg, &context, concurrency, |pool| {
            solve_install(
                project,
                &manifest,
                registry.as_ref(),
                pool,
                local.as_ref(),
                local_mode,
            )
        })?
    };
    report_prefetch(prepared.report, concurrency);
    Ok(prepared)
}

/// Warm the content-addressed store for an install. Non-frozen installs use the
/// same complete graph returned to the normal installer facade. Frozen replay
/// remains lock-authoritative and never re-solves or rewrites the graph.
pub fn prefetch(project: &Path, cfg: &Config, frozen: bool) -> Result<PrefetchReport> {
    prefetch_with_mode(project, cfg, frozen, LocalRegistryMode::from_env()?)
}

/// [`prefetch`] with the dependency-source policy passed explicitly, for
/// callers that already hold one instead of inheriting it from the process
/// environment.
pub(crate) fn prefetch_with_mode(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    local_mode: LocalRegistryMode,
) -> Result<PrefetchReport> {
    if !frozen {
        return prepare(project, cfg, local_mode).map(|prepared| prepared.report);
    }

    // Frozen replay is lock-authoritative: the lockfile never pins a live
    // source link, so there is nothing here for the local registry to answer.
    // The mirrors it *does* pin are still honored by `prefetch_locked`.
    let concurrency = install_concurrency();
    let report = prefetch_locked(project, cfg, concurrency)?;
    report_prefetch(report, concurrency);
    Ok(report)
}

fn local_index_for_solving(cfg: &Config, mode: LocalRegistryMode) -> Result<Option<LocalIndex>> {
    if !mode.enabled() {
        return Ok(None);
    }
    crate::local_registry::load(cfg)
        .context("reading the local project registry")
        .map(Some)
}

fn report_prefetch(report: PrefetchReport, concurrency: usize) {
    if report.resolved > 0 {
        eprintln!(
            "recursive install prefetch: {} package(s), up to {} concurrent, {} downloaded",
            report.resolved, concurrency, report.downloaded
        );
    }
}

fn install_concurrency() -> usize {
    normalize_concurrency(std::env::var("ZED_PKG_INSTALL_CONCURRENCY").ok().as_deref())
}

pub(super) fn normalize_concurrency(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INSTALL_CONCURRENCY)
        .min(MAX_INSTALL_CONCURRENCY)
}

fn run_with_pool<T>(
    cfg: &Config,
    context: &MirrorContext,
    concurrency: usize,
    operation: impl FnOnce(&FetchPool) -> Result<T>,
) -> Result<T> {
    let pool = FetchPool::new(concurrency, context, &cfg.home)?;
    match operation(&pool) {
        Ok(value) => {
            pool.shutdown(false)?;
            Ok(value)
        }
        Err(error) => {
            let _ = pool.shutdown(true);
            Err(error)
        }
    }
}

fn locked_version_metadata(locked: &zed_interfaces::lockfile::LockedPackage) -> VersionMetadata {
    VersionMetadata {
        org: locked.org.clone(),
        name: locked.name.clone(),
        version: locked.version.clone(),
        sha256: locked.sha256.clone(),
        size: locked.size,
        format: locked.format,
        vcs_tag: locked.vcs_tag.clone(),
        vcs_commit: locked.vcs_commit.clone(),
        // The worker never consumes this URL while the authenticated bytes are
        // already present in the store or verified artifact cache.
        download_url: String::new(),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
        mirrors: locked.mirrors.clone(),
        signatures: Vec::new(),
    }
}

/// What a project's lockfile already establishes about the packages it pins.
///
/// Read best-effort: a project with no lockfile yet, or one being resolved for
/// the first time, simply has no anchors, and every check that consults them
/// degrades to "no pin" rather than to an error.
pub(crate) fn project_trust_anchors(project: &Path) -> TrustAnchors {
    fs::read_to_string(project.join(LOCKFILE_FILE))
        .ok()
        .and_then(|text| Lockfile::parse(&text).ok())
        .map(|lock| TrustAnchors::from_lockfile(&lock))
        .unwrap_or_default()
}

fn prefetch_locked(project: &Path, cfg: &Config, concurrency: usize) -> Result<PrefetchReport> {
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("--frozen requires {}", lock_path.display()))?;
    let lock = Lockfile::parse(&text)?;
    let store = Store::new(&cfg.home);
    let context = cfg.mirror_context(TrustAnchors::from_lockfile(&lock));
    let mut registry: Option<Box<dyn Registry>> = None;
    let mut tasks = Vec::with_capacity(lock.packages.len());
    let mut seen = BTreeSet::new();

    for locked in &lock.packages {
        if !is_slug(&locked.org) || !is_slug(&locked.name) {
            bail!(
                "lockfile entry `{}/{}` has an invalid identity; refusing",
                locked.org,
                locked.name
            );
        }
        require_sha256(&locked.sha256)?;
        let key = locked.full_name();
        if !seen.insert(key.clone()) {
            bail!("duplicate package `{key}` in {LOCKFILE_FILE}");
        }

        let version = if store.has(&locked.sha256)
            || store.cached_artifact(&locked.sha256).is_file()
        {
            // The lockfile authenticates every immutable field needed to
            // verify locally owned bytes. Frozen replay must not turn a local
            // restore into a registry metadata availability check.
            locked_version_metadata(locked)
        } else {
            if registry.is_none() {
                registry = Some(context.open(&cfg.registry)?);
            }
            let live = registry
                .as_deref()
                .context("frozen prefetch registry was not initialized")?
                .get_version(&locked.org, &locked.name, &locked.version);
            match live {
                Ok(version) => {
                    validate_version_identity(
                        &version,
                        &locked.org,
                        &locked.name,
                        &locked.version,
                    )?;
                    if version.sha256 != locked.sha256 {
                        bail!(
                            "registry artifact for {}@{} changed (lock {} vs registry {}); refusing",
                            key,
                            locked.version,
                            locked.sha256,
                            version.sha256
                        );
                    }
                    version
                }
                // A frozen install already knows everything about this
                // artifact that the registry could tell it. When the
                // registry is unreachable and the lock names other places
                // to look, proceed from the pin — the store verifies the
                // bytes against it either way.
                Err(registry_error)
                    if !locked.mirrors.is_empty() && context.policy.allows_artifacts() =>
                {
                    eprintln!(
                        "warning: {key}@{}: registry unavailable ({}); \
                             restoring from the mirrors pinned in {LOCKFILE_FILE}",
                        locked.version,
                        registry_error
                            .to_string()
                            .lines()
                            .next()
                            .unwrap_or_default()
                    );
                    locked_version_metadata(locked)
                }
                Err(registry_error) => return Err(registry_error),
            }
        };
        validate_version_identity(&version, &locked.org, &locked.name, &locked.version)?;
        tasks.push(FetchTask {
            sequence: tasks.len(),
            key,
            version,
        });
    }

    if tasks.is_empty() {
        return Ok(PrefetchReport::default());
    }

    run_with_pool(cfg, &context, concurrency, |pool| {
        let total = tasks.len();
        for task in tasks {
            pool.submit(task)?;
        }
        let mut downloaded = 0usize;
        let mut buffered_results: BTreeMap<usize, Result<FetchResult>> = BTreeMap::new();
        for expected in 0..total {
            let result = receive_in_order(pool, &mut buffered_results, expected)?;
            downloaded += usize::from(result.downloaded);
        }
        Ok(PrefetchReport {
            resolved: total,
            downloaded,
        })
    })
}

pub(super) fn receive_in_order(
    pool: &FetchPool,
    buffered: &mut BTreeMap<usize, Result<FetchResult>>,
    expected: usize,
) -> Result<FetchResult> {
    loop {
        if let Some(result) = buffered.remove(&expected) {
            return result;
        }
        let message = pool.receive()?;
        if message.sequence == expected {
            return message.result;
        }
        if buffered.insert(message.sequence, message.result).is_some() {
            bail!(
                "recursive install worker returned duplicate result sequence {}",
                message.sequence
            );
        }
    }
}
