//! Recursive, bounded-concurrency artifact prefetch for `zed install`.
//!
//! The normal installer remains the authority for resolution, lockfile writes,
//! project transactions, adapters, and symlink/copy materialization. This module
//! runs immediately before it: it walks package manifests recursively, installs
//! artifacts into the content-addressed store with a five-worker queue, and lets
//! the normal install phase reuse the warm store.
//!
//! Cross-process coordination is deliberately per artifact. Waiters block in
//! the operating system's file-lock primitive instead of polling, so unrelated
//! dependencies and unrelated projects can keep making progress.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use globset::Glob;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::{Manifest, is_slug};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::registry::VersionMetadata;
use zed_interfaces::version::{self, Requirement};

use crate::config::{Config, read_manifest};
use crate::pack::sha256_file;
use crate::registry::{Registry, registry_for};
use crate::store::{Store, require_sha256};

/// Default number of artifact installs allowed to make progress at once.
pub const DEFAULT_INSTALL_CONCURRENCY: usize = 5;
const MAX_INSTALL_CONCURRENCY: usize = 32;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchReport {
    /// Unique registry packages resolved (workspace links are not counted).
    pub resolved: usize,
    /// Artifacts that were absent locally and downloaded by this process.
    pub downloaded: usize,
}

#[derive(Debug, Clone)]
struct DependencyRequest {
    key: String,
    requirement: String,
}

#[derive(Debug, Clone)]
struct FetchTask {
    sequence: usize,
    key: String,
    version: VersionMetadata,
}

#[derive(Debug)]
struct FetchResult {
    dependencies: BTreeMap<String, String>,
    downloaded: bool,
}

#[derive(Debug)]
struct FetchMessage {
    sequence: usize,
    result: Result<FetchResult>,
}

#[derive(Debug, Default)]
struct QueueState {
    tasks: VecDeque<FetchTask>,
    closed: bool,
}

#[derive(Debug, Default)]
struct TaskQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl TaskQueue {
    fn state(&self) -> MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push(&self, task: FetchTask) -> bool {
        let mut state = self.state();
        if state.closed {
            return false;
        }
        state.tasks.push_back(task);
        self.ready.notify_one();
        true
    }

    fn pop(&self) -> Option<FetchTask> {
        let mut state = self.state();
        loop {
            if let Some(task) = state.tasks.pop_front() {
                return Some(task);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        self.ready.notify_all();
    }

    fn cancel(&self) {
        let mut state = self.state();
        state.tasks.clear();
        state.closed = true;
        self.ready.notify_all();
    }
}

struct FetchPool {
    queue: Arc<TaskQueue>,
    results: mpsc::Receiver<FetchMessage>,
    workers: Vec<JoinHandle<()>>,
}

impl FetchPool {
    fn new(concurrency: usize, registry_url: &str, home: &Path) -> Result<Self> {
        let queue = Arc::new(TaskQueue::default());
        let (result_tx, results) = mpsc::channel();
        let mut workers = Vec::with_capacity(concurrency);

        for index in 0..concurrency {
            let worker_queue = Arc::clone(&queue);
            let worker_tx = result_tx.clone();
            let worker_registry = registry_url.to_string();
            let worker_home = home.to_path_buf();
            let spawn = thread::Builder::new()
                .name(format!("zed-install-{index}"))
                .spawn(move || worker_loop(worker_queue, worker_tx, worker_registry, worker_home));
            match spawn {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    queue.cancel();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error).context("starting recursive install worker");
                }
            }
        }
        drop(result_tx);
        Ok(Self {
            queue,
            results,
            workers,
        })
    }

    fn submit(&self, task: FetchTask) -> Result<()> {
        if !self.queue.push(task) {
            bail!("recursive install worker queue closed unexpectedly");
        }
        Ok(())
    }

    fn receive(&self) -> Result<FetchMessage> {
        self.results
            .recv()
            .context("all recursive install workers stopped unexpectedly")
    }

    fn shutdown(self, cancel: bool) -> Result<()> {
        if cancel {
            self.queue.cancel();
        } else {
            self.queue.close();
        }
        let mut panicked = false;
        for worker in self.workers {
            panicked |= worker.join().is_err();
        }
        if panicked {
            bail!("a recursive install worker panicked");
        }
        Ok(())
    }
}

/// A process lock for one content hash. `lock_exclusive` blocks in the kernel;
/// it does not wake up periodically to retry. The file descriptor owns the
/// lock, so process exit or panic releases it automatically.
struct ArtifactProcessLock {
    file: fs::File,
}

impl ArtifactProcessLock {
    fn acquire(home: &Path, sha256: &str) -> Result<Self> {
        require_sha256(sha256)?;
        let path = home.join("locks").join(format!("artifact-{sha256}.lock"));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening artifact lock {}", path.display()))?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("waiting for artifact lock {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for ArtifactProcessLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Default)]
struct WorkspaceGraph {
    dependencies: BTreeMap<String, BTreeMap<String, String>>,
}

impl WorkspaceGraph {
    fn discover(project: &Path) -> Self {
        let mut current = Some(project);
        while let Some(directory) = current {
            if directory.join(MANIFEST_FILE).is_file()
                && let Ok(manifest) = read_manifest(directory)
                && let Some(workspace) = manifest.workspace.as_ref()
            {
                return Self::collect(directory, &workspace.members);
            }
            current = directory.parent();
        }
        Self::default()
    }

    fn collect(root: &Path, patterns: &[String]) -> Self {
        let mut graph = Self::default();
        for pattern in patterns {
            let mut candidates = vec![root.to_path_buf()];
            for segment in pattern.split('/') {
                let mut next = Vec::new();
                for base in &candidates {
                    if segment.contains('*') {
                        let Ok(glob) = Glob::new(segment) else {
                            continue;
                        };
                        let matcher = glob.compile_matcher();
                        if let Ok(entries) = fs::read_dir(base) {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                if entry.path().is_dir()
                                    && matcher.is_match(Path::new(&name))
                                    && !name.to_string_lossy().starts_with('.')
                                {
                                    next.push(entry.path());
                                }
                            }
                        }
                    } else {
                        let candidate = base.join(segment);
                        if candidate.is_dir() {
                            next.push(candidate);
                        }
                    }
                }
                candidates = next;
            }
            for member_dir in candidates {
                if let Ok(member) = read_manifest(&member_dir) {
                    let key = member.full_name();
                    graph.dependencies.insert(key, member.dependencies);
                }
            }
        }
        graph
    }
}

mod artifact;
mod resolver;
#[cfg(test)]
mod tests;

use artifact::worker_loop;
pub use resolver::prefetch;
