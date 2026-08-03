from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"expected one match in {path}, found {count}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "src/install_graph/artifact.rs",
    '''    while let Some(task) = queue.pop() {
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
''',
    '''    while let Some(task) = queue.pop() {
        let sequence = task.sequence;
        let key = task.key.clone();
        let message = run_fetch_task(sequence, &key, || -> Result<FetchResult> {
            if registry.is_none() {
                registry = Some(registry_for(&registry_url)?);
            }
            let registry = registry
                .as_deref()
                .context("recursive install worker has no registry")?;
            prefetch_one(registry, &store, task)
        });
        if results.send(message).is_err() {
            return;
        }
    }
}

/// Convert a task panic into the same sequenced result channel used for normal
/// failures. Without this boundary, the worker would unwind after popping the
/// task, permanently losing that sequence while the coordinator waited for a
/// result that could never arrive.
pub(super) fn run_fetch_task<F>(sequence: usize, key: &str, work: F) -> FetchMessage
where
    F: FnOnce() -> Result<FetchResult>,
{
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(
        |payload| {
            let detail = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!(
                "recursive install worker panicked while processing {key} \
                 (sequence {sequence}): {detail}"
            ))
        },
    );
    FetchMessage { sequence, result }
}

fn prefetch_one(registry: &dyn Registry, store: &Store, task: FetchTask) -> Result<FetchResult> {
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''struct CountingRegistry {
    artifact: PathBuf,
    downloads: Arc<AtomicUsize>,
    started: Option<test_mpsc::Sender<()>>,
    release: Option<test_mpsc::Receiver<()>>,
}
''',
    '''struct CountingRegistry {
    artifact: PathBuf,
    downloads: Arc<AtomicUsize>,
    started: Option<test_mpsc::Sender<()>>,
    release: Option<test_mpsc::Receiver<()>>,
    fail_after_write: bool,
}
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''        if let Some(release) = &self.release {
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::copy(&self.artifact, dest)?;
        Ok(())
''',
    '''        if let Some(release) = &self.release {
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        if self.fail_after_write {
            fs::write(dest, b"partial fixture download")?;
            bail!("injected artifact download failure");
        }
        fs::copy(&self.artifact, dest)?;
        Ok(())
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''        started: Some(recursive_started_tx),
        release: Some(recursive_release_rx),
    };
''',
    '''        started: Some(recursive_started_tx),
        release: Some(recursive_release_rx),
        fail_after_write: false,
    };
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''        started: Some(legacy_started_tx),
        release: None,
    };
''',
    '''        started: Some(legacy_started_tx),
        release: None,
        fail_after_write: false,
    };
''',
)

path = Path("src/install_graph/tests.rs")
text = path.read_text()
if "worker_panics_are_reported_without_losing_the_task_sequence" in text:
    raise RuntimeError("worker panic regression test already exists")
if "failed_downloads_remove_staging_files_and_never_publish_cache_entries" in text:
    raise RuntimeError("failed download cleanup regression test already exists")

marker = '''#[test]
fn defaults_to_five_workers_and_bounds_overrides() {
'''
if text.count(marker) != 1:
    raise RuntimeError("could not locate install graph test insertion point")

block = r'''#[test]
fn worker_panics_are_reported_without_losing_the_task_sequence() {
    let message = super::artifact::run_fetch_task(
        17,
        "test/panic",
        || -> Result<FetchResult> { panic!("fixture worker panic") },
    );

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
    assert!(error.contains("injected artifact download failure"), "{error}");
    assert!(!store.cached_artifact(&sha).exists());
    assert!(!store.has(&sha));
    let cache_entries = fs::read_dir(store.cache_dir())
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(cache_entries, 0, "failed staging directory leaked");
}

'''

path.write_text(text.replace(marker, block + marker, 1))
