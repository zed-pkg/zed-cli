use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use tokio_util::sync::CancellationToken;
use zed_interfaces::binary_artifact::{
    BinaryArchiveFormatV1, BinaryArtifactMetadataV1, BinaryArtifactPublishMetaV1,
};
use zed_interfaces::registry::{
    AuditLogResponse, ClaimOrgResponse, PackageMetadata, PublishMeta, PublishResponse,
    SearchResponse, VersionMetadata, YankResponse,
};

use super::resolver::project_trust_anchors;
use super::solver::{PreparedInstall, solve_install};
use super::{DEFAULT_INSTALL_CONCURRENCY, FetchPool, MAX_INSTALL_CONCURRENCY, PrefetchReport};
use crate::config::{Config, read_manifest};
use crate::registry::{Registry, registry_for};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateDecision {
    Pending,
    Continue,
    Cancel,
}

/// One-shot boundary between speculative metadata work and artifact acquisition.
///
/// The remote resolver may issue cancellable Hyper package/version metadata
/// requests while local checkout discovery is running. The first selected
/// version cannot be returned to the solver until this gate opens, which means
/// the solver cannot enqueue an artifact download before local discovery has
/// decided whether remote work is actually needed.
#[derive(Debug, Clone)]
pub(crate) struct ResolutionGate {
    inner: Arc<(Mutex<GateDecision>, Condvar)>,
}

impl ResolutionGate {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(GateDecision::Pending), Condvar::new())),
        }
    }

    pub(crate) fn continue_resolution(&self) {
        self.set(GateDecision::Continue);
    }

    pub(crate) fn cancel_resolution(&self) {
        self.set(GateDecision::Cancel);
    }

    fn set(&self, decision: GateDecision) {
        let (lock, ready) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == GateDecision::Pending {
            *state = decision;
            ready.notify_all();
        }
    }

    fn wait(&self) -> GateDecision {
        let (lock, ready) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *state == GateDecision::Pending {
            state = ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *state
    }
}

/// Resolution-only registry adapter whose canonical HTTP leg is truly async.
///
/// The synchronous `Registry` trait remains the solver boundary for now, but
/// each metadata call is backed by a Hyper future inside a Tokio runtime. A
/// `CancellationToken` stops the future itself; it does not merely detach a
/// thread containing `reqwest::blocking`.
struct CancellableResolutionRegistry {
    runtime: tokio::runtime::Runtime,
    async_client: zed_client_async::AsyncClient,
    legacy_fallback: Box<dyn Registry>,
    cancel: CancellationToken,
    gate: ResolutionGate,
}

impl CancellableResolutionRegistry {
    fn new(url: &str, cancel: CancellationToken, gate: ResolutionGate) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("zed-registry-async")
            .build()
            .context("building async registry runtime")?;
        let async_client = zed_client_async::AsyncClient::new(url)
            .map_err(|error| anyhow!("creating async registry client: {error}"))?;
        Ok(Self {
            runtime,
            async_client,
            // Compatibility bridge only. New resolution traffic uses Hyper;
            // this old registry is entered only after an async primary error
            // and only after local discovery explicitly says to continue.
            legacy_fallback: registry_for(url)?,
            cancel,
            gate,
        })
    }

    fn cancelled(&self) -> anyhow::Error {
        anyhow!("speculative registry resolution cancelled after local checkout win")
    }

    fn require_continue(&self) -> Result<()> {
        match self.gate.wait() {
            GateDecision::Continue => Ok(()),
            GateDecision::Cancel | GateDecision::Pending => Err(self.cancelled()),
        }
    }

    fn package_async(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let result = self.runtime.block_on(
            self.cancel
                .run_until_cancelled(self.async_client.get_package(org, name)),
        );
        match result {
            None => Err(self.cancelled()),
            Some(Ok(package)) => Ok(package),
            Some(Err(async_error)) => {
                if self.cancel.is_cancelled() {
                    return Err(self.cancelled());
                }
                // A primary-metadata failure must not enter the legacy blocking
                // fallback while local discovery might still win the race.
                self.require_continue()?;
                eprintln!(
                    "warning: async registry package read failed ({async_error}); using legacy source fallback"
                );
                self.legacy_fallback.get_package(org, name)
            }
        }
    }

    fn version_async(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        let result = self.runtime.block_on(
            self.cancel
                .run_until_cancelled(self.async_client.get_version(org, name, version)),
        );
        match result {
            None => Err(self.cancelled()),
            Some(Ok(metadata)) => {
                // Critical race boundary: solve_install cannot enqueue the
                // artifact corresponding to this metadata until local discovery
                // has explicitly chosen remote continuation.
                self.require_continue()?;
                Ok(metadata)
            }
            Some(Err(async_error)) => {
                if self.cancel.is_cancelled() {
                    return Err(self.cancelled());
                }
                self.require_continue()?;
                eprintln!(
                    "warning: async registry version read failed ({async_error}); using legacy source fallback"
                );
                self.legacy_fallback.get_version(org, name, version)
            }
        }
    }
}

impl Registry for CancellableResolutionRegistry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        self.package_async(org, name)
    }

    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        self.version_async(org, name, version)
    }

    fn download(&self, _version: &VersionMetadata, _dest: &Path) -> Result<()> {
        bail!("resolution-only async registry cannot download artifacts")
    }

    fn publish(
        &self,
        _meta: &PublishMeta,
        _artifact: &Path,
        _token: Option<&str>,
    ) -> Result<PublishResponse> {
        bail!("resolution-only async registry cannot publish")
    }

    fn get_binary_artifact(
        &self,
        _org: &str,
        _name: &str,
        _version: &str,
        _target: &str,
        _format: BinaryArchiveFormatV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        bail!("resolution-only async registry cannot read binary artifacts")
    }

    fn download_binary_artifact(
        &self,
        _metadata: &BinaryArtifactMetadataV1,
        _dest: &Path,
    ) -> Result<()> {
        bail!("resolution-only async registry cannot download binary artifacts")
    }

    fn publish_binary_artifact(
        &self,
        _meta: &BinaryArtifactPublishMetaV1,
        _artifact: &Path,
        _token: Option<&str>,
    ) -> Result<BinaryArtifactMetadataV1> {
        bail!("resolution-only async registry cannot publish binary artifacts")
    }

    fn claim_org(&self, _slug: &str, _token: Option<&str>) -> Result<ClaimOrgResponse> {
        bail!("resolution-only async registry cannot claim orgs")
    }

    fn search(&self, _query: &str) -> Result<SearchResponse> {
        bail!("resolution-only async registry does not expose search")
    }

    fn yank(
        &self,
        _org: &str,
        _name: &str,
        _version: &str,
        _yanked: bool,
        _token: Option<&str>,
    ) -> Result<YankResponse> {
        bail!("resolution-only async registry cannot yank versions")
    }

    fn audit_log(
        &self,
        _org: &str,
        _limit: Option<u64>,
        _token: Option<&str>,
    ) -> Result<AuditLogResponse> {
        bail!("resolution-only async registry cannot read audit logs")
    }
}

pub(crate) fn prepare(
    project: &Path,
    cfg: &Config,
    cancel: CancellationToken,
    gate: ResolutionGate,
) -> Result<PreparedInstall> {
    if !matches!(cfg.registry.as_str(), url if url.starts_with("http://") || url.starts_with("https://"))
    {
        return match gate.wait() {
            GateDecision::Continue => super::resolver::prepare(project, cfg),
            GateDecision::Cancel | GateDecision::Pending => {
                bail!("speculative registry resolution cancelled after local checkout win")
            }
        };
    }

    let concurrency = install_concurrency();
    let context = cfg.mirror_context(project_trust_anchors(project));
    let registry = CancellableResolutionRegistry::new(&cfg.registry, cancel, gate)?;
    let manifest = read_manifest(project)?;
    let prepared = if manifest.dependencies.is_empty() {
        PreparedInstall::default()
    } else {
        // Workers exist speculatively but receive no artifact task until a
        // version metadata call passes ResolutionGate::Continue.
        let pool = FetchPool::new(concurrency, &context, &cfg.home)?;
        match solve_install(project, &manifest, &registry, &pool) {
            Ok(prepared) => {
                pool.shutdown(false)?;
                prepared
            }
            Err(error) => {
                let _ = pool.shutdown(true);
                return Err(error);
            }
        }
    };
    report_prefetch(prepared.report, concurrency);
    Ok(prepared)
}

fn install_concurrency() -> usize {
    super::resolver::normalize_concurrency(
        std::env::var("ZED_PKG_INSTALL_CONCURRENCY").ok().as_deref(),
    )
}

fn report_prefetch(report: PrefetchReport, concurrency: usize) {
    if report.resolved > 0 {
        eprintln!(
            "recursive install prefetch: {} package(s), up to {} concurrent, {} downloaded (cancellable Hyper metadata)",
            report.resolved, concurrency, report.downloaded
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn install_concurrency_stays_within_global_bound() {
        let value = install_concurrency();
        assert!((1..=MAX_INSTALL_CONCURRENCY).contains(&value));
        assert!(DEFAULT_INSTALL_CONCURRENCY <= MAX_INSTALL_CONCURRENCY);
    }

    #[test]
    fn resolution_gate_waits_for_continue() {
        let gate = ResolutionGate::new();
        let worker_gate = gate.clone();
        let worker = thread::spawn(move || worker_gate.wait());
        thread::sleep(Duration::from_millis(10));
        assert!(!worker.is_finished());
        gate.continue_resolution();
        assert_eq!(worker.join().unwrap(), GateDecision::Continue);
    }

    #[test]
    fn resolution_gate_cancels_waiter() {
        let gate = ResolutionGate::new();
        let worker_gate = gate.clone();
        let worker = thread::spawn(move || worker_gate.wait());
        gate.cancel_resolution();
        assert_eq!(worker.join().unwrap(), GateDecision::Cancel);
    }
}
