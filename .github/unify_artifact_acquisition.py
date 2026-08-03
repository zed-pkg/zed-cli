from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"expected one match in {path}, found {count}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "src/store.rs",
    '''    pub fn root(&self) -> PathBuf {
        self.home.join("store")
    }

    pub fn cache_dir(&self) -> PathBuf {
''',
    '''    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    pub fn root(&self) -> PathBuf {
        self.home.join("store")
    }

    pub fn cache_dir(&self) -> PathBuf {
''',
)

replace_once(
    "src/install_graph/artifact.rs",
    '''            prefetch_one(registry, &store, &home, task)
''',
    '''            prefetch_one(registry, &store, task)
''',
)

replace_once(
    "src/install_graph/artifact.rs",
    '''fn prefetch_one(
    registry: &dyn Registry,
    store: &Store,
    home: &Path,
    task: FetchTask,
) -> Result<FetchResult> {
    let (package_dir, downloaded) = ensure_artifact(registry, store, home, &task.version)
''',
    '''fn prefetch_one(
    registry: &dyn Registry,
    store: &Store,
    task: FetchTask,
) -> Result<FetchResult> {
    let (package_dir, downloaded) = ensure_artifact(registry, store, &task.version)
''',
)

replace_once(
    "src/install_graph/artifact.rs",
    '''fn ensure_artifact(
    registry: &dyn Registry,
    store: &Store,
    home: &Path,
    version: &VersionMetadata,
) -> Result<(PathBuf, bool)> {
''',
    '''/// Acquire one immutable artifact through the shared cache/store path.
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
''',
)

replace_once(
    "src/install_graph/artifact.rs",
    '''    let _artifact_lock = ArtifactProcessLock::acquire(home, &version.sha256)?;
''',
    '''    let _artifact_lock = ArtifactProcessLock::acquire(store.home(), &version.sha256)?;
''',
)

replace_once(
    "src/install_graph.rs",
    '''use artifact::worker_loop;
pub use resolver::prefetch;
''',
    '''pub(crate) use artifact::ensure_artifact;
use artifact::worker_loop;
pub use resolver::prefetch;
''',
)

replace_once(
    "src/ops.rs",
    '''fn ensure_artifact(reg: &dyn Registry, store: &Store, vm: &VersionMetadata) -> Result<PathBuf> {
    validate_version_metadata(vm)?;
    if store.has(&vm.sha256) {
        return Ok(store.pkg_dir(&vm.sha256));
    }
    let cached = store.cached_artifact(&vm.sha256);
    if !cached.exists() {
        reg.download(vm, &cached)?;
    }
    store.add_artifact(&cached, &vm.sha256)
}
''',
    '''fn ensure_artifact(reg: &dyn Registry, store: &Store, vm: &VersionMetadata) -> Result<PathBuf> {
    validate_version_metadata(vm)?;
    crate::install_graph::ensure_artifact(reg, store, vm)
        .map(|(package_dir, _downloaded)| package_dir)
}

#[cfg(test)]
pub(crate) fn legacy_ensure_artifact_for_test(
    reg: &dyn Registry,
    store: &Store,
    vm: &VersionMetadata,
) -> Result<PathBuf> {
    ensure_artifact(reg, store, vm)
}
''',
)

replace_once(
    "src/ops_entry.rs",
    '''pub(crate) use implementation::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
''',
    '''pub(crate) use implementation::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

#[cfg(test)]
pub(crate) use implementation::legacy_ensure_artifact_for_test;

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''use std::sync::{Arc, Barrier, mpsc as test_mpsc};
''',
    '''use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc as test_mpsc,
};
''',
)

path = Path("src/install_graph/tests.rs")
text = path.read_text()
if "legacy_and_recursive_acquisition_paths_share_one_download_lock" in text:
    raise RuntimeError("mixed-path lock test already exists")

marker = '''#[test]
fn defaults_to_five_workers_and_bounds_overrides() {
'''
if text.count(marker) != 1:
    raise RuntimeError("could not locate first install-graph test")

block = r'''struct CountingRegistry {
    artifact: PathBuf,
    downloads: Arc<AtomicUsize>,
    started: Option<test_mpsc::Sender<()>>,
    release: Option<test_mpsc::Receiver<()>>,
}

impl Registry for CountingRegistry {
    fn get_package(&self, _org: &str, _name: &str) -> Result<PackageMetadata> {
        unreachable!("mixed-path acquisition test does not resolve package metadata")
    }

    fn get_version(
        &self,
        _org: &str,
        _name: &str,
        _version: &str,
    ) -> Result<VersionMetadata> {
        unreachable!("mixed-path acquisition test receives resolved version metadata")
    }

    fn download(&self, _version: &VersionMetadata, dest: &Path) -> Result<()> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = &self.started {
            started.send(()).unwrap();
        }
        if let Some(release) = &self.release {
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::copy(&self.artifact, dest)?;
        Ok(())
    }

    fn publish(
        &self,
        _meta: &zed_interfaces::registry::PublishMeta,
        _artifact: &Path,
        _token: Option<&str>,
    ) -> Result<zed_interfaces::registry::PublishResponse> {
        unreachable!("mixed-path acquisition test does not publish")
    }

    fn claim_org(
        &self,
        _slug: &str,
        _token: Option<&str>,
    ) -> Result<zed_interfaces::registry::ClaimOrgResponse> {
        unreachable!("mixed-path acquisition test does not claim organizations")
    }

    fn search(&self, _query: &str) -> Result<zed_interfaces::registry::SearchResponse> {
        unreachable!("mixed-path acquisition test does not search")
    }

    fn yank(
        &self,
        _org: &str,
        _name: &str,
        _version: &str,
        _yanked: bool,
        _token: Option<&str>,
    ) -> Result<zed_interfaces::registry::YankResponse> {
        unreachable!("mixed-path acquisition test does not yank")
    }

    fn audit_log(
        &self,
        _org: &str,
        _limit: Option<u64>,
        _token: Option<&str>,
    ) -> Result<zed_interfaces::registry::AuditLogResponse> {
        unreachable!("mixed-path acquisition test does not read audit logs")
    }
}

#[test]
fn legacy_and_recursive_acquisition_paths_share_one_download_lock() {
    let temp = tempfile::tempdir().unwrap();
    let registry_root = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");

    let sha = publish_fixture(
        &registry_root,
        &scratch,
        "test",
        "shared",
        "1.0.0",
        &[],
    );
    let version = published_version(&registry_root, "test", "shared", "1.0.0");
    let artifact = registry_root
        .join("artifacts")
        .join(format!("{sha}.tar.gz"));
    let downloads = Arc::new(AtomicUsize::new(0));
    let (recursive_started_tx, recursive_started_rx) = test_mpsc::channel();
    let (recursive_release_tx, recursive_release_rx) = test_mpsc::channel();
    let (legacy_started_tx, legacy_started_rx) = test_mpsc::channel();

    let recursive_registry = CountingRegistry {
        artifact: artifact.clone(),
        downloads: Arc::clone(&downloads),
        started: Some(recursive_started_tx),
        release: Some(recursive_release_rx),
    };
    let legacy_registry = CountingRegistry {
        artifact,
        downloads: Arc::clone(&downloads),
        started: Some(legacy_started_tx),
        release: None,
    };
    let recursive_store = Store::new(&home);
    let legacy_store = Store::new(&home);
    let recursive_version = version.clone();
    let legacy_version = version;

    let recursive = thread::spawn(move || {
        super::ensure_artifact(&recursive_registry, &recursive_store, &recursive_version).unwrap()
    });
    recursive_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let legacy = thread::spawn(move || {
        crate::ops::legacy_ensure_artifact_for_test(
            &legacy_registry,
            &legacy_store,
            &legacy_version,
        )
        .unwrap()
    });

    let legacy_entered_download = legacy_started_rx
        .recv_timeout(Duration::from_millis(300))
        .is_ok();
    recursive_release_tx.send(()).unwrap();

    let (recursive_path, recursive_downloaded) = recursive.join().unwrap();
    let legacy_path = legacy.join().unwrap();
    assert!(recursive_downloaded);
    assert!(
        !legacy_entered_download,
        "the legacy path bypassed the shared artifact lock and started a duplicate download"
    );
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_path, recursive_path);
    assert!(recursive_path.is_dir());
}

'''

path.write_text(text.replace(marker, block + marker, 1))
