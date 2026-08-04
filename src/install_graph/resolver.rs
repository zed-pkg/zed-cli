use super::artifact::validate_version_identity;
use super::solver::{PreparedInstall, solve_install};
use super::*;

/// Solve the complete active constraint set before the transactional installer
/// mutates project output. Candidate manifests are still loaded through the
/// bounded worker pool and the shared per-artifact process locks.
pub(crate) fn prepare(project: &Path, cfg: &Config) -> Result<PreparedInstall> {
    let concurrency = install_concurrency();
    let registry = registry_for(&cfg.registry)?;
    let manifest = read_manifest(project)?;
    let prepared = if manifest.dependencies.is_empty() {
        PreparedInstall::default()
    } else {
        run_with_pool(cfg, concurrency, |pool| {
            solve_install(project, &manifest, registry.as_ref(), pool)
        })?
    };
    report_prefetch(prepared.report, concurrency);
    Ok(prepared)
}

/// Warm the content-addressed store for an install. Non-frozen installs use the
/// same complete graph returned to the normal installer facade. Frozen replay
/// remains lock-authoritative and never re-solves or rewrites the graph.
pub fn prefetch(project: &Path, cfg: &Config, frozen: bool) -> Result<PrefetchReport> {
    if !frozen {
        return prepare(project, cfg).map(|prepared| prepared.report);
    }

    let concurrency = install_concurrency();
    let registry = registry_for(&cfg.registry)?;
    let report = prefetch_locked(project, cfg, registry.as_ref(), concurrency)?;
    report_prefetch(report, concurrency);
    Ok(report)
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
    concurrency: usize,
    operation: impl FnOnce(&FetchPool) -> Result<T>,
) -> Result<T> {
    let pool = FetchPool::new(concurrency, &cfg.registry, &cfg.home)?;
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

fn prefetch_locked(
    project: &Path,
    cfg: &Config,
    registry: &dyn Registry,
    concurrency: usize,
) -> Result<PrefetchReport> {
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("--frozen requires {}", lock_path.display()))?;
    let lock = Lockfile::parse(&text)?;
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
        let version = registry.get_version(&locked.org, &locked.name, &locked.version)?;
        validate_version_identity(&version, &locked.org, &locked.name, &locked.version)?;
        if version.sha256 != locked.sha256 {
            bail!(
                "registry artifact for {}@{} changed (lock {} vs registry {}); refusing",
                key,
                locked.version,
                locked.sha256,
                version.sha256
            );
        }
        tasks.push(FetchTask {
            sequence: tasks.len(),
            key,
            version,
        });
    }

    if tasks.is_empty() {
        return Ok(PrefetchReport::default());
    }

    run_with_pool(cfg, concurrency, |pool| {
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
