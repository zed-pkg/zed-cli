//! A [`Registry`] that answers from mirrors when the canonical one cannot.
//!
//! Wrapping rather than rewriting is deliberate. Every existing call site —
//! `zed install`, `zed fetch --frozen`, the recursive prefetch workers, the
//! legacy transactional installer — already talks to a `Box<dyn Registry>`.
//! Making the fallback a decorator over that trait means the resilience is
//! uniform by construction, instead of being a thing that was remembered in
//! four places and forgotten in the fifth.
//!
//! What degrades and what does not:
//!
//! | operation | registry down |
//! |---|---|
//! | download a pinned artifact | works — mirrors, verified against the pin |
//! | read one version's metadata | works if signed — mirrors, signature checked |
//! | resolve a range | works if signed — signed index, rollback-checked |
//! | publish, yank, claim org, audit | fails, correctly — these are writes |
//!
//! Writes are not mirrored and never will be. A mirror is a copy; accepting a
//! write into a copy is how two registries end up disagreeing about what a
//! version is.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use zed_interfaces::binary_artifact::{
    BinaryArchiveFormatV1, BinaryArtifactMetadataV1, BinaryArtifactPublishMetaV1,
};
use zed_interfaces::mirror::{MirrorCoordinateV1, MirrorDescriptorV1};
use zed_interfaces::registry::{
    AuditLogResponse, ClaimOrgResponse, PackageMetadata, PublishMeta, PublishResponse,
    SearchResponse, VersionMetadata, YankResponse,
};
use zed_interfaces::signing::{PublisherKeyV1, SignedVersionV1};

use crate::mirror::{MirrorClient, MirrorHit, merge_mirrors, registry_mirror};
use crate::publisher_keys::{self, TrustCache};
use crate::registry::Registry;

/// How a request was actually served. Callers report this so a degraded
/// install is visible in a build log rather than being discovered later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The canonical registry answered.
    Canonical,
    /// A mirror answered after the canonical registry did not.
    Mirror(MirrorHit),
}

/// What a consumer already trusts about a package, from its lockfile.
#[derive(Debug, Clone, Default)]
pub struct TrustAnchors {
    /// `org/name` → the publisher key pinned on first use.
    pins: BTreeMap<String, PublisherKeyV1>,
    /// `org/name` → mirrors recorded at resolution time.
    mirrors: BTreeMap<String, Vec<MirrorDescriptorV1>>,
    /// `org/name` → the highest index sequence this project has already seen.
    ///
    /// Rollback protection: a mirror can serve a genuinely signed but stale
    /// index forever, and without a floor that is an undetectable way to hide
    /// a security release.
    sequences: BTreeMap<String, u64>,
}

impl TrustAnchors {
    /// Read what a resolved lockfile already establishes.
    pub fn from_lockfile(lock: &zed_interfaces::lockfile::Lockfile) -> Self {
        let mut anchors = Self::default();
        for package in &lock.packages {
            let key = package.full_name();
            if let Some(pinned) = package.pinned_key() {
                anchors.pins.insert(key.clone(), pinned);
            }
            if !package.mirrors.is_empty() {
                anchors.mirrors.insert(key, package.mirrors.clone());
            }
        }
        anchors
    }

    pub fn pin(&self, org: &str, name: &str) -> Option<&PublisherKeyV1> {
        self.pins.get(&format!("{org}/{name}"))
    }

    pub fn mirrors_for(&self, org: &str, name: &str) -> &[MirrorDescriptorV1] {
        self.mirrors
            .get(&format!("{org}/{name}"))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn minimum_sequence(&self, org: &str, name: &str) -> Option<u64> {
        self.sequences.get(&format!("{org}/{name}")).copied()
    }

    pub fn observe_sequence(&mut self, org: &str, name: &str, sequence: u64) {
        let entry = self.sequences.entry(format!("{org}/{name}")).or_insert(0);
        *entry = (*entry).max(sequence);
    }
}

/// Policy for how far a caller is willing to degrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Canonical registry only. What `--no-mirrors` selects, and the right
    /// choice for a publish or an audit.
    Disabled,
    /// Mirrors may serve artifact bytes, which the lockfile pin verifies.
    /// Metadata still comes from the registry.
    ArtifactsOnly,
    /// Mirrors may serve metadata too, provided it carries a publisher
    /// signature that verifies against a trusted key.
    Full,
}

impl FallbackPolicy {
    pub fn allows_artifacts(self) -> bool {
        !matches!(self, FallbackPolicy::Disabled)
    }

    pub fn allows_metadata(self) -> bool {
        matches!(self, FallbackPolicy::Full)
    }
}

/// Everything a registry client needs to know about fallback, in a form that
/// can be cloned into a prefetch worker thread.
///
/// The prefetch workers each build their own client, so the policy has to
/// travel to them as data. Bundling it here is what keeps a worker's client
/// from quietly being the one client in the process with no fallback.
#[derive(Debug, Clone)]
pub struct MirrorContext {
    pub registry_url: String,
    pub configured: Vec<MirrorDescriptorV1>,
    pub anchors: TrustAnchors,
    pub policy: FallbackPolicy,
    pub trust_cache: TrustCache,
    pub max_artifact_bytes: u64,
}

impl MirrorContext {
    /// Open a registry client for `url`, wrapped in the fallback chain.
    ///
    /// With fallback disabled the inner client is returned unwrapped, so
    /// `--no-mirrors` is behaviourally identical to the code that existed
    /// before mirrors did — not merely similar to it.
    pub fn open(&self, url: &str) -> Result<Box<dyn Registry>> {
        let inner = crate::registry::registry_for(url)?;
        if self.policy == FallbackPolicy::Disabled {
            return Ok(inner);
        }
        Ok(Box::new(MirroredRegistry::new(
            inner,
            url,
            &self.configured,
            self.anchors.clone(),
            self.policy,
            self.trust_cache.clone(),
            self.max_artifact_bytes,
        )?))
    }
}

/// The registry record a frozen lockfile entry already implies.
///
/// A frozen restore asks the registry a question it has already answered: the
/// digest, size, format, tag, and revision are all pinned in the lock, and the
/// pin is what the store verifies against anyway. So when the registry cannot
/// answer, the lock can — this is the difference between an outage that stops
/// every CI job in the company and one nobody notices.
///
/// Deliberately *not* used when the registry is reachable. A live record can
/// report a yank, and cross-checking it against the lock is how a changed pin
/// gets caught. This path is the degraded one, and its use is reported.
pub fn metadata_from_lock(locked: &zed_interfaces::lockfile::LockedPackage) -> VersionMetadata {
    VersionMetadata {
        org: locked.org.clone(),
        name: locked.name.clone(),
        version: locked.version.clone(),
        sha256: locked.sha256.clone(),
        size: locked.size,
        format: locked.format,
        vcs_tag: locked.vcs_tag.clone(),
        vcs_commit: locked.vcs_commit.clone(),
        // No registry route: the mirror chain is the only way these bytes are
        // being fetched, and an invented URL here would be a lie a caller
        // might act on.
        download_url: String::new(),
        // The lock does not record publication time and must not invent one:
        // a fabricated timestamp would be indistinguishable from a real one to
        // everything downstream.
        published_at: String::new(),
        yanked: false,
        mirrors: locked.mirrors.clone(),
        signatures: Vec::new(),
    }
}

pub struct MirroredRegistry {
    inner: Box<dyn Registry>,
    /// Mirrors that apply to every package: the registry itself, operator
    /// configuration, and anything a bootstrap document contributed.
    ambient: Vec<MirrorDescriptorV1>,
    anchors: TrustAnchors,
    policy: FallbackPolicy,
    client: MirrorClient,
    trust_cache: TrustCache,
    /// Fallbacks that actually happened, for the caller to report once at the
    /// end rather than interleaved with progress output.
    notes: RefCell<Vec<String>>,
}

impl MirroredRegistry {
    pub fn new(
        inner: Box<dyn Registry>,
        registry_url: &str,
        configured_mirrors: &[MirrorDescriptorV1],
        anchors: TrustAnchors,
        policy: FallbackPolicy,
        trust_cache: TrustCache,
        max_artifact_bytes: u64,
    ) -> Result<Self> {
        let canonical: Vec<MirrorDescriptorV1> =
            registry_mirror(registry_url).into_iter().collect();
        let ambient = merge_mirrors(&[configured_mirrors, &canonical])?;
        Ok(Self {
            inner,
            ambient,
            anchors,
            policy,
            client: MirrorClient::new(max_artifact_bytes)?,
            trust_cache,
            notes: RefCell::new(Vec::new()),
        })
    }

    /// Everything that could serve this package, in try order.
    fn mirrors_for(
        &self,
        org: &str,
        name: &str,
        from_metadata: &[MirrorDescriptorV1],
    ) -> Vec<MirrorDescriptorV1> {
        // Locked mirrors first among the package-specific sources: they were
        // recorded when resolution last succeeded, which is a better claim
        // than whatever a possibly-degraded metadata document says now.
        merge_mirrors(&[
            self.anchors.mirrors_for(org, name),
            from_metadata,
            &self.ambient,
        ])
        .unwrap_or_default()
    }

    fn note(&self, note: Option<String>) {
        if let Some(note) = note {
            self.notes.borrow_mut().push(note);
        }
    }

    /// Fallbacks that occurred, for the caller to print once.
    pub fn degradation_notes(&self) -> Vec<String> {
        self.notes.borrow().clone()
    }

    /// Keys trusted for this org: the lockfile pin if there is one, otherwise
    /// whatever the last successful registry contact cached locally.
    fn trusted_keys(&self, org: &str) -> Vec<PublisherKeyV1> {
        self.trust_cache.keys_for(org)
    }

    fn verify_signed_version(
        &self,
        document: &SignedVersionV1,
        org: &str,
        name: &str,
    ) -> Result<VersionMetadata> {
        let preimage = document
            .preimage()
            .map_err(|error| anyhow!("cannot reconstruct the signed payload: {error}"))?;
        let trusted = self.trusted_keys(org);
        let verified = publisher_keys::verify(
            &preimage,
            &document.signatures,
            &trusted,
            self.anchors.pin(org, name),
        )
        .with_context(|| {
            format!(
                "refusing mirror-served metadata for {org}/{name}@{}",
                document.payload.version
            )
        })?;
        let payload = &document.payload;
        self.note(Some(format!(
            "{org}/{name}@{}: metadata verified against publisher key `{}`{}",
            payload.version,
            verified.key.key_id,
            if verified.was_pinned {
                " (pinned in .zpkg.lock)"
            } else {
                ""
            }
        )));
        Ok(VersionMetadata {
            org: payload.org.clone(),
            name: payload.name.clone(),
            version: payload.version.clone(),
            sha256: payload.sha256.clone(),
            size: payload.size,
            format: payload.format,
            vcs_tag: payload.vcs_tag.clone(),
            vcs_commit: Some(payload.vcs_commit.clone()),
            // Empty on purpose: a mirror-served document has no registry
            // download route, and the mirror chain is how the bytes are
            // fetched. A plausible-looking URL here would invite a caller to
            // use it and quietly bypass that chain.
            download_url: String::new(),
            published_at: payload.published_at.clone(),
            yanked: false,
            mirrors: payload.mirrors.clone(),
            signatures: document.signatures.clone(),
        })
    }
}

impl Registry for MirroredRegistry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let canonical = self.inner.get_package(org, name);
        match canonical {
            Ok(metadata) => {
                // A successful contact is the moment to refresh the local
                // trust cache: the keys arrive over TLS from the canonical
                // registry, which is the strongest anchor available, and they
                // are exactly what a later degraded resolution will need.
                if !metadata.signing_keys.is_empty() {
                    self.trust_cache.remember(org, &metadata.signing_keys);
                }
                Ok(metadata)
            }
            Err(registry_error) if self.policy.allows_metadata() => {
                let mirrors = self.mirrors_for(org, name, &[]);
                let (document, hit) = self
                    .client
                    .fetch_signed_index(&mirrors, org, name)
                    .map_err(|mirror_error| {
                        anyhow!("{registry_error:#}\n\nfalling back to mirrors also failed:\n{mirror_error:#}")
                    })?;
                let preimage = document
                    .preimage()
                    .map_err(|error| anyhow!("cannot reconstruct the signed index: {error}"))?;
                let trusted = self.trusted_keys(org);
                publisher_keys::verify(
                    &preimage,
                    &document.signatures,
                    &trusted,
                    self.anchors.pin(org, name),
                )
                .with_context(|| format!("refusing mirror-served index for {org}/{name}"))?;
                if let Some(floor) = self.anchors.minimum_sequence(org, name)
                    && document.payload.sequence < floor
                {
                    bail!(
                        "mirror served index sequence {} for {org}/{name}, but this project has \
                         already seen {floor}; refusing a rollback",
                        document.payload.sequence
                    );
                }
                self.note(hit.fallback_note(&format!("{org}/{name} index")));
                let versions = document
                    .payload
                    .versions
                    .iter()
                    .filter(|entry| !entry.yanked)
                    .map(|entry| entry.version.clone())
                    .collect::<Vec<_>>();
                Ok(PackageMetadata {
                    org: org.to_owned(),
                    name: name.to_owned(),
                    description: None,
                    vcs: zed_interfaces::vcs::Vcs::default(),
                    repo_url: String::new(),
                    version_scheme: Default::default(),
                    latest: versions.first().cloned(),
                    tags: Vec::new(),
                    versions,
                    mirrors: document.payload.mirrors.clone(),
                    signing_keys: trusted,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        match self.inner.get_version(org, name, version) {
            Ok(metadata) => Ok(metadata),
            Err(registry_error) if self.policy.allows_metadata() => {
                let mirrors = self.mirrors_for(org, name, &[]);
                // The coordinate is not fully known yet — the digest is what
                // we are asking for — so only the coordinate-addressed mirror
                // routes can answer, which is exactly what
                // `version_metadata_urls` uses.
                let coord = MirrorCoordinateV1 {
                    org,
                    name,
                    version,
                    sha256: "",
                    format: Default::default(),
                    vcs_tag: "",
                };
                let (document, hit) = self
                    .client
                    .fetch_signed_version(&mirrors, &coord)
                    .map_err(|mirror_error| {
                        anyhow!("{registry_error:#}\n\nfalling back to mirrors also failed:\n{mirror_error:#}")
                    })?;
                self.note(hit.fallback_note(&format!("{org}/{name}@{version} metadata")));
                self.verify_signed_version(&document, org, name)
            }
            Err(error) => Err(error),
        }
    }

    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        let registry_error = match self.inner.download(version, dest) {
            Ok(()) => return Ok(()),
            Err(error) if self.policy.allows_artifacts() => error,
            Err(error) => return Err(error),
        };
        let mirrors = self.mirrors_for(&version.org, &version.name, &version.mirrors);
        if mirrors.is_empty() {
            return Err(registry_error);
        }
        let coord = MirrorCoordinateV1 {
            org: &version.org,
            name: &version.name,
            version: &version.version,
            sha256: &version.sha256,
            format: version.format,
            vcs_tag: &version.vcs_tag,
        };
        let hit = self
            .client
            .fetch_artifact(&mirrors, &coord, version.size, dest)
            .map_err(|mirror_error| {
                anyhow!(
                    "{registry_error:#}\n\nfalling back to mirrors also failed:\n{mirror_error:#}"
                )
            })?;
        self.note(hit.fallback_note(&format!(
            "{}/{}@{}",
            version.org, version.name, version.version
        )));
        Ok(())
    }

    fn publish(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<PublishResponse> {
        self.inner.publish(meta, artifact, token)
    }

    fn get_binary_artifact(
        &self,
        org: &str,
        name: &str,
        version: &str,
        target: &str,
        format: BinaryArchiveFormatV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        self.inner
            .get_binary_artifact(org, name, version, target, format)
    }

    fn download_binary_artifact(
        &self,
        metadata: &BinaryArtifactMetadataV1,
        dest: &Path,
    ) -> Result<()> {
        self.inner.download_binary_artifact(metadata, dest)
    }

    fn publish_binary_artifact(
        &self,
        meta: &BinaryArtifactPublishMetaV1,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<BinaryArtifactMetadataV1> {
        self.inner.publish_binary_artifact(meta, artifact, token)
    }

    fn claim_org(&self, slug: &str, token: Option<&str>) -> Result<ClaimOrgResponse> {
        self.inner.claim_org(slug, token)
    }

    fn search(&self, query: &str) -> Result<SearchResponse> {
        self.inner.search(query)
    }

    fn yank(
        &self,
        org: &str,
        name: &str,
        version: &str,
        yanked: bool,
        token: Option<&str>,
    ) -> Result<YankResponse> {
        self.inner.yank(org, name, version, yanked, token)
    }

    fn audit_log(
        &self,
        org: &str,
        limit: Option<u64>,
        token: Option<&str>,
    ) -> Result<AuditLogResponse> {
        self.inner.audit_log(org, limit, token)
    }
}
