#!/usr/bin/env python3
"""Make frozen prefetch use authenticated lock metadata for local bytes."""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/install_graph/resolver.rs",
    '''    let concurrency = install_concurrency();
    let registry = registry_for(&cfg.registry)?;
    let report = prefetch_locked(project, cfg, registry.as_ref(), concurrency)?;
''',
    '''    let concurrency = install_concurrency();
    let report = prefetch_locked(project, cfg, concurrency)?;
''',
    "frozen prefetch registry construction",
)

replace_once(
    "src/install_graph/resolver.rs",
    '''fn prefetch_locked(
    project: &Path,
    cfg: &Config,
    registry: &dyn Registry,
    concurrency: usize,
) -> Result<PrefetchReport> {
''',
    '''fn prefetch_locked(
    project: &Path,
    cfg: &Config,
    concurrency: usize,
) -> Result<PrefetchReport> {
''',
    "frozen prefetch signature",
)

replace_once(
    "src/install_graph/resolver.rs",
    '''    let lock = Lockfile::parse(&text)?;
    let mut tasks = Vec::with_capacity(lock.packages.len());
    let mut seen = BTreeSet::new();
''',
    '''    let lock = Lockfile::parse(&text)?;
    let store = Store::new(&cfg.home);
    let mut registry: Option<Box<dyn Registry>> = None;
    let mut tasks = Vec::with_capacity(lock.packages.len());
    let mut seen = BTreeSet::new();
''',
    "frozen prefetch local store",
)

replace_once(
    "src/install_graph/resolver.rs",
    '''        let version = registry.get_version(&locked.org, &locked.name, &locked.version)?;
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
''',
    '''        let version = if store.has(&locked.sha256)
            || store.cached_artifact(&locked.sha256).is_file()
        {
            // The lockfile already authenticates every immutable field needed
            // to verify bytes that are present in the content store or cache.
            // Frozen replay must not turn local restoration into a registry
            // metadata availability check.
            VersionMetadata {
                org: locked.org.clone(),
                name: locked.name.clone(),
                version: locked.version.clone(),
                sha256: locked.sha256.clone(),
                size: locked.size,
                format: locked.format,
                vcs_tag: locked.vcs_tag.clone(),
                vcs_commit: locked.vcs_commit.clone(),
                download_url: String::new(),
                published_at: "1970-01-01T00:00:00Z".to_string(),
                yanked: false,
            }
        } else {
            if registry.is_none() {
                registry = Some(registry_for(&cfg.registry)?);
            }
            let version = registry
                .as_deref()
                .context("frozen prefetch registry was not initialized")?
                .get_version(&locked.org, &locked.name, &locked.version)?;
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
            version
        };
        validate_version_identity(&version, &locked.org, &locked.name, &locked.version)?;
''',
    "lock-authoritative frozen metadata",
)

replace_once(
    "src/install_graph/tests.rs",
    "fn frozen_prefetch_installs_every_locked_artifact_without_a_manifest() {",
    "fn frozen_prefetch_replays_every_locked_artifact_without_registry_metadata() {",
    "offline frozen prefetch test name",
)

replace_once(
    "src/install_graph/tests.rs",
    '''    let report = prefetch(&project, &test_config(&registry, &home), true).unwrap();
    assert_eq!(report.resolved, 4);
    assert_eq!(report.downloaded, 4);
}

#[test]
fn a_corrupt_partial_cache_is_replaced_under_the_artifact_lock() {
''',
    '''    let cfg = test_config(&registry, &home);
    let cold = prefetch(&project, &cfg, true).unwrap();
    assert_eq!(cold.resolved, 4);
    assert_eq!(cold.downloaded, 4);

    fs::remove_dir_all(&registry).unwrap();
    let offline = prefetch(&project, &cfg, true).unwrap();
    assert_eq!(offline.resolved, 4);
    assert_eq!(offline.downloaded, 0);
}

#[test]
fn a_corrupt_partial_cache_is_replaced_under_the_artifact_lock() {
''',
    "offline frozen prefetch assertions",
)
