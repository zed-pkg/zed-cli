use super::artifact::{resolve_version, split_key, validate_version_identity};
use super::*;

/// Warm the content-addressed store for an install. The normal installer runs
/// immediately afterward and remains responsible for the project transaction.
pub fn prefetch(project: &Path, cfg: &Config, frozen: bool) -> Result<PrefetchReport> {
    let concurrency = install_concurrency();
    let registry = registry_for(&cfg.registry)?;
    let report = if frozen {
        prefetch_locked(project, cfg, registry.as_ref(), concurrency)?
    } else {
        let manifest = read_manifest(project)?;
        prefetch_recursive(project, cfg, registry.as_ref(), &manifest, concurrency)?
    };

    if report.resolved > 0 {
        eprintln!(
            "recursive install prefetch: {} package(s), up to {} concurrent, {} downloaded",
            report.resolved, concurrency, report.downloaded
        );
    }
    Ok(report)
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

fn prefetch_recursive(
    project: &Path,
    cfg: &Config,
    registry: &dyn Registry,
    manifest: &Manifest,
    concurrency: usize,
) -> Result<PrefetchReport> {
    if manifest.dependencies.is_empty() {
        return Ok(PrefetchReport::default());
    }
    let workspace = WorkspaceGraph::discover(project);

    run_with_pool(cfg, concurrency, |pool| {
        let mut pending: VecDeque<DependencyRequest> = manifest
            .dependencies
            .iter()
            .map(|(key, requirement)| DependencyRequest {
                key: key.clone(),
                requirement: requirement.clone(),
            })
            .collect();
        let mut resolved: BTreeMap<String, VersionMetadata> = BTreeMap::new();
        let mut expanded_workspace = BTreeSet::new();
        let mut in_flight = 0usize;
        let mut next_sequence = 0usize;
        let mut next_result = 0usize;
        let mut buffered_results: BTreeMap<usize, Result<FetchResult>> = BTreeMap::new();
        let mut downloaded = 0usize;

        loop {
            while let Some(request) = pending.pop_front() {
                if let Some(member_dependencies) = workspace.dependencies.get(&request.key) {
                    if expanded_workspace.insert(request.key.clone()) {
                        pending.extend(member_dependencies.iter().map(|(key, requirement)| {
                            DependencyRequest {
                                key: key.clone(),
                                requirement: requirement.clone(),
                            }
                        }));
                    }
                    continue;
                }

                let (org, name) = split_key(&request.key)?;
                let requirement = Requirement::parse(&request.requirement);
                if let Some(existing) = resolved.get(&request.key) {
                    if requirement.matches(&existing.version) {
                        continue;
                    }
                    bail!(
                        "version conflict for {}: resolved {} but another dependency requires `{}` (zed installs one version per package)",
                        request.key,
                        existing.version,
                        request.requirement
                    );
                }

                let version = resolve_version(registry, &org, &name, &request.requirement)?;
                resolved.insert(request.key.clone(), version.clone());
                pool.submit(FetchTask {
                    sequence: next_sequence,
                    key: request.key,
                    version,
                })?;
                next_sequence += 1;
                in_flight += 1;
            }

            if in_flight == 0 {
                break;
            }

            let result = receive_in_order(pool, &mut buffered_results, next_result)?;
            next_result += 1;
            in_flight -= 1;
            downloaded += usize::from(result.downloaded);
            pending.extend(
                result
                    .dependencies
                    .into_iter()
                    .map(|(key, requirement)| DependencyRequest { key, requirement }),
            );
        }

        Ok(PrefetchReport {
            resolved: resolved.len(),
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
