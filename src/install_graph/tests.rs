use std::process::{Command, Stdio};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc as test_mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use zed_interfaces::lockfile::LockedPackage;
use zed_interfaces::registry::{PackageMetadata, VersionMetadata};
use zed_interfaces::vcs::Vcs;

use super::resolver::normalize_concurrency;
use super::*;
use crate::pack::pack;

fn test_config(registry: &Path, home: &Path) -> Config {
    Config {
        registry: format!("file://{}", registry.display()),
        home: home.to_path_buf(),
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    }
}

fn manifest_text(org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) -> String {
    let mut text = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"

[package.repository]
vcs = "git"
url = "https://example.invalid/{org}/{name}"
"#,
    );
    if !dependencies.is_empty() {
        text.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            text.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    text
}

fn publish_fixture(
    registry_root: &Path,
    scratch: &Path,
    org: &str,
    name: &str,
    version: &str,
    dependencies: &[(&str, &str)],
) -> String {
    let source = scratch.join(format!("source-{name}"));
    fs::create_dir_all(&source).unwrap();
    let manifest_text = manifest_text(org, name, version, dependencies);
    fs::write(source.join(MANIFEST_FILE), &manifest_text).unwrap();
    fs::write(
        source.join("payload.txt"),
        format!("{org}/{name}@{version}\n"),
    )
    .unwrap();
    let manifest = Manifest::parse(&manifest_text).unwrap();
    let packed = pack(
        &source,
        &manifest,
        Some(&scratch.join(format!("packed-{name}"))),
    )
    .unwrap();

    let artifacts = registry_root.join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    fs::copy(
        &packed.path,
        artifacts.join(format!("{}.tar.gz", packed.sha256)),
    )
    .unwrap();

    let package_dir = registry_root.join("packages").join(org).join(name);
    fs::create_dir_all(package_dir.join("versions")).unwrap();
    let version_metadata = VersionMetadata {
        org: org.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
        vcs_tag: format!("v{version}"),
        vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        download_url: format!("file://{}/{}.tar.gz", artifacts.display(), packed.sha256),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
        mirrors: Vec::new(),
        signatures: Vec::new(),
    };
    fs::write(
        package_dir.join("versions").join(format!("{version}.json")),
        serde_json::to_string_pretty(&version_metadata).unwrap(),
    )
    .unwrap();
    let package_metadata = PackageMetadata {
        org: org.to_string(),
        name: name.to_string(),
        description: Some(format!("fixture {name}")),
        vcs: Vcs::Git,
        repo_url: format!("https://example.invalid/{org}/{name}"),
        version_scheme: manifest.package.version_scheme,
        latest: Some(version.to_string()),
        tags: Vec::new(),
        versions: vec![version.to_string()],
        mirrors: Vec::new(),
        signing_keys: Vec::new(),
    };
    fs::write(
        package_dir.join("package.json"),
        serde_json::to_string_pretty(&package_metadata).unwrap(),
    )
    .unwrap();
    packed.sha256
}

fn published_version(
    registry_root: &Path,
    org: &str,
    name: &str,
    version: &str,
) -> VersionMetadata {
    let path = registry_root
        .join("packages")
        .join(org)
        .join(name)
        .join("versions")
        .join(format!("{version}.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

struct CountingRegistry {
    artifact: PathBuf,
    downloads: Arc<AtomicUsize>,
    started: Option<test_mpsc::Sender<()>>,
    release: Option<test_mpsc::Receiver<()>>,
    fail_after_write: bool,
}

impl Registry for CountingRegistry {
    fn get_package(&self, _org: &str, _name: &str) -> Result<PackageMetadata> {
        unreachable!("mixed-path acquisition test does not resolve package metadata")
    }

    fn get_version(&self, _org: &str, _name: &str, _version: &str) -> Result<VersionMetadata> {
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
        if self.fail_after_write {
            fs::write(dest, b"partial fixture download")?;
            bail!("injected artifact download failure");
        }
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

    let sha = publish_fixture(&registry_root, &scratch, "test", "shared", "1.0.0", &[]);
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
        fail_after_write: false,
    };
    let legacy_registry = CountingRegistry {
        artifact,
        downloads: Arc::clone(&downloads),
        started: Some(legacy_started_tx),
        release: None,
        fail_after_write: false,
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

#[test]
fn worker_panics_are_reported_without_losing_the_task_sequence() {
    let message = super::artifact::run_fetch_task(17, "test/panic", || -> Result<FetchResult> {
        panic!("fixture worker panic")
    });

    assert_eq!(message.sequence, 17);
    let error = message.result.unwrap_err().to_string();
    assert!(error.contains("test/panic"), "{error}");
    assert!(error.contains("sequence 17"), "{error}");
    assert!(error.contains("fixture worker panic"), "{error}");
}

#[test]
fn failed_downloads_remove_staging_files_and_never_publish_cache_entries() {
    let temp = tempfile::tempdir().unwrap();
    let registry_root = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");

    let sha = publish_fixture(&registry_root, &scratch, "test", "failure", "1.0.0", &[]);
    let version = published_version(&registry_root, "test", "failure", "1.0.0");
    let registry = CountingRegistry {
        artifact: registry_root
            .join("artifacts")
            .join(format!("{sha}.tar.gz")),
        downloads: Arc::new(AtomicUsize::new(0)),
        started: None,
        release: None,
        fail_after_write: true,
    };
    let store = Store::new(&home);

    let error = super::ensure_artifact(&registry, &store, &version)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("injected artifact download failure"),
        "{error}"
    );
    assert!(!store.cached_artifact(&sha).exists());
    assert!(!store.has(&sha));
    let cache_entries = fs::read_dir(store.cache_dir())
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(cache_entries, 0, "failed staging directory leaked");
}

#[test]
fn defaults_to_five_workers_and_bounds_overrides() {
    assert_eq!(normalize_concurrency(None), 5);
    assert_eq!(normalize_concurrency(Some("not-a-number")), 5);
    assert_eq!(normalize_concurrency(Some("0")), 5);
    assert_eq!(normalize_concurrency(Some("1")), 1);
    assert_eq!(normalize_concurrency(Some("999")), MAX_INSTALL_CONCURRENCY);
}

#[test]
fn recursively_prefetches_packages_of_packages_and_deduplicates_diamonds() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let leaf = publish_fixture(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    let left = publish_fixture(
        &registry,
        &scratch,
        "test",
        "left",
        "1.0.0",
        &[("test/leaf", "^1")],
    );
    let right = publish_fixture(
        &registry,
        &scratch,
        "test",
        "right",
        "1.0.0",
        &[("test/leaf", "^1")],
    );
    let root = publish_fixture(
        &registry,
        &scratch,
        "test",
        "root",
        "1.0.0",
        &[("test/left", "^1"), ("test/right", "^1")],
    );
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", &[("test/root", "^1")]),
    )
    .unwrap();

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, 4);
    assert_eq!(report.downloaded, 4);
    let store = Store::new(&home);
    for sha in [leaf, left, right, root] {
        assert!(store.has(&sha), "missing prefetched artifact {sha}");
    }
}

#[test]
fn dependency_cycles_terminate_after_each_package_is_resolved_once() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();

    publish_fixture(
        &registry,
        &scratch,
        "test",
        "a",
        "1.0.0",
        &[("test/b", "^1")],
    );
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "b",
        "1.0.0",
        &[("test/a", "^1")],
    );
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", &[("test/a", "^1")]),
    )
    .unwrap();

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, 2);
    assert_eq!(report.downloaded, 2);
}

#[test]
fn concurrent_prefetches_share_one_artifact_download() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let first_project = temp.path().join("first-project");
    let second_project = temp.path().join("second-project");
    fs::create_dir_all(&first_project).unwrap();
    fs::create_dir_all(&second_project).unwrap();

    publish_fixture(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    let consumer_manifest = manifest_text("consumer", "app", "0.1.0", &[("test/leaf", "^1")]);
    fs::write(first_project.join(MANIFEST_FILE), &consumer_manifest).unwrap();
    fs::write(second_project.join(MANIFEST_FILE), &consumer_manifest).unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let first_cfg = test_config(&registry, &home);
    let second_cfg = first_cfg.clone();
    let first_barrier = Arc::clone(&barrier);
    let second_barrier = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_barrier.wait();
        prefetch(&first_project, &first_cfg, false).unwrap()
    });
    let second = thread::spawn(move || {
        second_barrier.wait();
        prefetch(&second_project, &second_cfg, false).unwrap()
    });

    barrier.wait();
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_eq!(first.resolved + second.resolved, 2);
    assert_eq!(
        first.downloaded + second.downloaded,
        1,
        "one process should publish the shared content hash and the other should reuse it"
    );
}

#[test]
fn a_warm_store_resolves_the_graph_without_redownloading_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();

    publish_fixture(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "left",
        "1.0.0",
        &[("test/leaf", "^1")],
    );
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "right",
        "1.0.0",
        &[("test/leaf", "^1")],
    );
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "root",
        "1.0.0",
        &[("test/left", "^1"), ("test/right", "^1")],
    );
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", &[("test/root", "^1")]),
    )
    .unwrap();
    let cfg = test_config(&registry, &home);

    let cold = prefetch(&project, &cfg, false).unwrap();
    let warm = prefetch(&project, &cfg, false).unwrap();
    assert_eq!(cold.resolved, 4);
    assert_eq!(cold.downloaded, 4);
    assert_eq!(warm.resolved, 4);
    assert_eq!(warm.downloaded, 0);
}

#[test]
fn conflicting_transitive_requirements_fail_in_deterministic_graph_order() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();

    publish_fixture(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "left",
        "1.0.0",
        &[("test/leaf", "^1")],
    );
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "right",
        "1.0.0",
        &[("test/leaf", "^2")],
    );
    publish_fixture(
        &registry,
        &scratch,
        "test",
        "root",
        "1.0.0",
        &[("test/left", "^1"), ("test/right", "^1")],
    );
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", &[("test/root", "^1")]),
    )
    .unwrap();

    let error = prefetch(&project, &test_config(&registry, &home), false).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("version conflict for test/leaf"),
        "{message}"
    );
    assert!(
        message.contains(
            "`^1` via consumer/app@0.1.0 -> test/root@1.0.0 -> test/left@1.0.0 -> test/leaf"
        ),
        "{message}"
    );
    assert!(
        message.contains(
            "`^2` via consumer/app@0.1.0 -> test/root@1.0.0 -> test/right@1.0.0 -> test/leaf"
        ),
        "{message}"
    );
}

#[test]
fn frozen_prefetch_installs_every_locked_artifact_without_a_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();

    for (name, dependencies) in [
        ("leaf", vec![]),
        ("left", vec![("test/leaf", "^1")]),
        ("right", vec![("test/leaf", "^1")]),
        ("root", vec![("test/left", "^1"), ("test/right", "^1")]),
    ] {
        publish_fixture(&registry, &scratch, "test", name, "1.0.0", &dependencies);
    }

    let source = format!("file://{}", registry.display());
    let mut lock = Lockfile::default();
    for name in ["leaf", "left", "right", "root"] {
        let version = published_version(&registry, "test", name, "1.0.0");
        lock.upsert(LockedPackage {
            org: version.org,
            name: version.name,
            version: version.version,
            sha256: version.sha256,
            size: version.size,
            format: version.format,
            vcs_tag: version.vcs_tag,
            vcs_commit: version.vcs_commit,
            source: source.clone(),
            mirrors: Vec::new(),
            signed_by: None,
            signing_key: None,
        });
    }
    fs::write(project.join(LOCKFILE_FILE), lock.to_toml_string().unwrap()).unwrap();

    let report = prefetch(&project, &test_config(&registry, &home), true).unwrap();
    assert_eq!(report.resolved, 4);
    assert_eq!(report.downloaded, 4);
}

#[test]
fn a_corrupt_partial_cache_is_replaced_under_the_artifact_lock() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();

    let sha = publish_fixture(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", &[("test/leaf", "^1")]),
    )
    .unwrap();
    let store = Store::new(&home);
    let cached = store.cached_artifact(&sha);
    fs::create_dir_all(cached.parent().unwrap()).unwrap();
    fs::write(&cached, b"partial download").unwrap();

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, 1);
    assert_eq!(report.downloaded, 1);
    assert!(store.has(&sha));
    assert_eq!(sha256_file(&cached).unwrap().0, sha);
}

#[test]
fn artifact_waiters_block_until_the_owner_releases_the_kernel_lock() {
    const CHILD_HOME: &str = "ZED_PKG_TEST_LOCK_CHILD_HOME";
    const CHILD_SHA: &str = "ZED_PKG_TEST_LOCK_CHILD_SHA";
    const CHILD_ATTEMPTING: &str = "ZED_PKG_TEST_LOCK_CHILD_ATTEMPTING";
    const CHILD_ACQUIRED: &str = "ZED_PKG_TEST_LOCK_CHILD_ACQUIRED";
    const TEST_NAME: &str =
        "install_graph::tests::artifact_waiters_block_until_the_owner_releases_the_kernel_lock";

    if let Some(home) = std::env::var_os(CHILD_HOME) {
        let sha = std::env::var(CHILD_SHA).unwrap();
        let attempting = PathBuf::from(std::env::var_os(CHILD_ATTEMPTING).unwrap());
        let acquired = PathBuf::from(std::env::var_os(CHILD_ACQUIRED).unwrap());
        fs::write(attempting, b"attempting").unwrap();
        let _waiter = ArtifactProcessLock::acquire(Path::new(&home), &sha).unwrap();
        fs::write(acquired, b"acquired").unwrap();
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let sha = "a".repeat(64);
    let owner = ArtifactProcessLock::acquire(temp.path(), &sha).unwrap();
    let attempting = temp.path().join("waiter-attempting");
    let acquired = temp.path().join("waiter-acquired");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(TEST_NAME)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_HOME, temp.path())
        .env(CHILD_SHA, &sha)
        .env(CHILD_ATTEMPTING, &attempting)
        .env(CHILD_ACQUIRED, &acquired)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let attempting_deadline = Instant::now() + Duration::from_secs(5);
    while !attempting.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("lock waiter child exited before attempting acquisition: {status}");
        }
        if Instant::now() >= attempting_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock waiter child did not reach acquisition");
        }
        thread::sleep(Duration::from_millis(20));
    }

    thread::sleep(Duration::from_millis(200));
    assert!(
        !acquired.exists(),
        "a separate process acquired the artifact lock while the owner still held it"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "lock waiter child exited before the owner released the lock"
    );

    drop(owner);
    let release_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "lock waiter child failed: {status}");
            break;
        }
        if Instant::now() >= release_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock waiter child did not wake after lock release");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        acquired.is_file(),
        "lock waiter child exited without recording acquisition"
    );
}
