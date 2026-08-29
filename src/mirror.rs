//! Fetching from somewhere other than the canonical registry.
//!
//! The chain this module walks is what makes a registry outage survivable. A
//! locked package names a `source` — the registry it was resolved from — and,
//! since this change, a list of mirrors that hold the same bytes. When the
//! source cannot answer, the mirrors are tried in order, and the first one
//! whose bytes hash to the lockfile's pin wins.
//!
//! Three properties keep that safe:
//!
//! 1. **No credentials leave here.** Every request this module makes is
//!    anonymous. A mirror is a public transport; if it needs a token, it is
//!    not a mirror, and sending one would put a registry credential on a host
//!    the registry does not control.
//! 2. **The pin is the authority.** Bytes are verified against the lockfile's
//!    sha256 before they are handed back. A mirror that serves the wrong bytes
//!    is a mirror that failed, not a mirror that succeeded differently.
//! 3. **Failure is accumulated, not swallowed.** Each attempt records why it
//!    did not work, and the final error names all of them. An install that
//!    fell back is not silent about it either: falling back is a fact an
//!    operator wants in their build log, because it usually means something
//!    upstream is broken.
//!
//! Resolution — as opposed to fetching a pinned artifact — additionally
//! requires a publisher signature, because there is no pin yet to check the
//! answer against. See [`crate::publisher_keys`].

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::mirror::{
    MIRROR_BOOTSTRAP_PATH, MirrorBootstrapV1, MirrorCoordinateV1, MirrorDescriptorV1, MirrorKindV1,
};
use zed_interfaces::signing::{SignedIndexV1, SignedVersionV1};

/// Per-request ceiling for a metadata document. Metadata is small; a mirror
/// that wants to stream a gigabyte of JSON at us is a mirror we stop reading.
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
/// How long any single mirror gets before the chain moves on. Deliberately
/// short: the whole value of a fallback chain is lost if one black-holed host
/// can stall an install for a minute.
const MIRROR_TIMEOUT: Duration = Duration::from_secs(20);
/// Redirect budget per request.
const MAX_REDIRECTS: usize = 5;

/// One failed attempt against one candidate URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorAttempt {
    pub mirror: String,
    /// Query-stripped, so a presigned URL never reaches a log line.
    pub url: String,
    pub reason: String,
}

/// Where a fetch succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorHit {
    pub mirror: String,
    pub url: String,
    /// Attempts that failed before this one. Non-empty means degraded service
    /// upstream, which callers surface rather than discard.
    pub failed: Vec<MirrorAttempt>,
}

impl MirrorHit {
    /// A one-line note for the operator when the fetch had to fall back.
    pub fn fallback_note(&self, subject: &str) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let tried = self
            .failed
            .iter()
            .map(|attempt| format!("{} ({})", attempt.mirror, attempt.reason))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "{subject}: served by mirror `{}` after {} source(s) failed: {tried}",
            self.mirror,
            self.failed.len()
        ))
    }
}

/// Anonymous, bounded HTTP client for mirror reads.
pub struct MirrorClient {
    client: reqwest::blocking::Client,
    max_artifact_bytes: u64,
}

impl MirrorClient {
    pub fn new(max_artifact_bytes: u64) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(MIRROR_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("too many mirror redirects");
                }
                let destination = attempt.url();
                let clean = destination.username().is_empty()
                    && destination.password().is_none()
                    && destination.fragment().is_none();
                // Object stores answer with a signed URL on another host; that
                // is expected and fine. Plaintext is not: a mirror is public
                // by construction, so there is never a reason to accept http
                // outside loopback.
                let safe_scheme = destination.scheme() == "https" || url_is_loopback(destination);
                if clean && safe_scheme {
                    attempt.follow()
                } else {
                    attempt.error("refusing unsafe mirror redirect")
                }
            }))
            .build()
            .context("building the mirror HTTP client")?;
        Ok(Self {
            client,
            max_artifact_bytes,
        })
    }

    /// Download the artifact for `coord` from the first mirror that produces
    /// bytes matching its pinned digest.
    ///
    /// `dest` is written only on success; a mirror that produces the wrong
    /// bytes leaves nothing behind.
    pub fn fetch_artifact(
        &self,
        mirrors: &[MirrorDescriptorV1],
        coord: &MirrorCoordinateV1<'_>,
        declared_size: u64,
        dest: &Path,
    ) -> Result<MirrorHit> {
        let mut failed: Vec<MirrorAttempt> = Vec::new();
        for mirror in mirrors {
            if !mirror.serves.artifacts {
                continue;
            }
            let id = mirror.identifier();
            let urls = match mirror.artifact_urls(coord) {
                Ok(urls) => urls,
                Err(error) => {
                    failed.push(MirrorAttempt {
                        mirror: id,
                        url: String::new(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            for url in urls {
                match self.try_artifact(&url, coord.sha256, declared_size, dest) {
                    Ok(()) => {
                        return Ok(MirrorHit {
                            mirror: id,
                            url: strip_query(&url),
                            failed,
                        });
                    }
                    Err(error) => {
                        let _ = fs::remove_file(dest);
                        failed.push(MirrorAttempt {
                            mirror: id.clone(),
                            url: strip_query(&url),
                            reason: brief(&error),
                        });
                    }
                }
            }
        }
        Err(exhausted(
            &format!(
                "{}/{}@{} ({})",
                coord.org, coord.name, coord.version, coord.sha256
            ),
            &failed,
        ))
    }

    /// Fetch a version's signed metadata document from the first mirror that
    /// has one. The signature is *not* checked here — verification needs the
    /// org's key set, which the caller owns.
    pub fn fetch_signed_version(
        &self,
        mirrors: &[MirrorDescriptorV1],
        coord: &MirrorCoordinateV1<'_>,
    ) -> Result<(SignedVersionV1, MirrorHit)> {
        self.fetch_document(
            mirrors,
            |mirror| mirror.serves.metadata,
            |mirror| mirror.version_metadata_urls(coord),
            &format!("{}/{}@{}", coord.org, coord.name, coord.version),
            |document: &SignedVersionV1| {
                document.validate().map_err(|error| anyhow!(error))?;
                if document.payload.org != coord.org
                    || document.payload.name != coord.name
                    || document.payload.version != coord.version
                {
                    // A mirror answering with a *different* package's signed
                    // document would otherwise pass signature verification
                    // perfectly well. Bind the answer to the question.
                    bail!("signed metadata is for a different package coordinate");
                }
                Ok(())
            },
        )
    }

    /// Fetch a package's signed version index. This is what makes range
    /// resolution possible with the registry unreachable.
    pub fn fetch_signed_index(
        &self,
        mirrors: &[MirrorDescriptorV1],
        org: &str,
        name: &str,
    ) -> Result<(SignedIndexV1, MirrorHit)> {
        self.fetch_document(
            mirrors,
            |mirror| mirror.serves.index,
            |mirror| mirror.package_index_urls(org, name),
            &format!("{org}/{name}"),
            |document: &SignedIndexV1| {
                document.validate().map_err(|error| anyhow!(error))?;
                if document.payload.org != org || document.payload.name != name {
                    bail!("signed index is for a different package");
                }
                Ok(())
            },
        )
    }

    /// Recover the mirror set from any reachable host.
    ///
    /// Circularity-breaking: a client that cannot reach the registry cannot
    /// ask the registry where the mirrors are, so every mirror serves the map.
    pub fn fetch_bootstrap(&self, urls: &[String]) -> Result<(MirrorBootstrapV1, MirrorHit)> {
        let mut failed = Vec::new();
        for url in urls {
            match self
                .read_json::<MirrorBootstrapV1>(url)
                .and_then(|document| {
                    document.validate().map_err(|error| anyhow!(error))?;
                    Ok(document)
                }) {
                Ok(document) => {
                    return Ok((
                        document,
                        MirrorHit {
                            mirror: "bootstrap".to_owned(),
                            url: strip_query(url),
                            failed,
                        },
                    ));
                }
                Err(error) => failed.push(MirrorAttempt {
                    mirror: "bootstrap".to_owned(),
                    url: strip_query(url),
                    reason: brief(&error),
                }),
            }
        }
        Err(exhausted(MIRROR_BOOTSTRAP_PATH, &failed))
    }

    /// Probe one mirror without downloading, for `zed mirror check`.
    pub fn probe(&self, url: &str) -> Result<u16> {
        if let Some(path) = local_path(url) {
            return if path.exists() {
                Ok(200)
            } else {
                bail!("no such file: {}", path.display())
            };
        }
        validate_public_url(url)?;
        let response = self
            .client
            .head(url)
            .send()
            .map_err(|error| anyhow!("{}", error.without_url()))?;
        Ok(response.status().as_u16())
    }

    fn fetch_document<T, S, U, V>(
        &self,
        mirrors: &[MirrorDescriptorV1],
        serves: S,
        urls_for: U,
        subject: &str,
        verify_shape: V,
    ) -> Result<(T, MirrorHit)>
    where
        T: serde::de::DeserializeOwned,
        S: Fn(&MirrorDescriptorV1) -> bool,
        U: Fn(&MirrorDescriptorV1) -> Result<Vec<String>, zed_interfaces::mirror::MirrorError>,
        V: Fn(&T) -> Result<()>,
    {
        let mut failed = Vec::new();
        for mirror in mirrors {
            if !serves(mirror) {
                continue;
            }
            let id = mirror.identifier();
            let urls = match urls_for(mirror) {
                Ok(urls) => urls,
                Err(error) => {
                    failed.push(MirrorAttempt {
                        mirror: id,
                        url: String::new(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            for url in urls {
                match self
                    .read_json::<T>(&url)
                    .and_then(|document| verify_shape(&document).map(|()| document))
                {
                    Ok(document) => {
                        return Ok((
                            document,
                            MirrorHit {
                                mirror: id,
                                url: strip_query(&url),
                                failed,
                            },
                        ));
                    }
                    Err(error) => failed.push(MirrorAttempt {
                        mirror: id.clone(),
                        url: strip_query(&url),
                        reason: brief(&error),
                    }),
                }
            }
        }
        Err(exhausted(subject, &failed))
    }

    fn try_artifact(
        &self,
        url: &str,
        expected_sha256: &str,
        declared_size: u64,
        dest: &Path,
    ) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        // The write bound is the declared size plus slack, under the global
        // cap: a mirror cannot make us fill a disk before we discover the
        // digest is wrong.
        let limit = if declared_size > 0 {
            declared_size
                .saturating_add(1024 * 1024)
                .min(self.max_artifact_bytes)
        } else {
            self.max_artifact_bytes
        };

        let mut reader: Box<dyn Read> = if let Some(path) = local_path(url) {
            Box::new(
                fs::File::open(&path)
                    .with_context(|| format!("opening local mirror artifact {}", path.display()))?,
            )
        } else {
            validate_public_url(url)?;
            let response = self
                .client
                .get(url)
                .send()
                .map_err(|error| anyhow!("{}", error.without_url()))?;
            let status = response.status();
            if !status.is_success() {
                bail!("HTTP {status}");
            }
            Box::new(response)
        };

        let mut file = fs::File::create(dest)?;
        let mut limited = (&mut reader).take(limit.saturating_add(1));
        let copied = std::io::copy(&mut limited, &mut file)?;
        drop(file);
        if copied > limit {
            bail!("exceeded the declared artifact size ({copied} > {limit} bytes)");
        }

        let actual = sha256_of(dest)?;
        if actual != expected_sha256 {
            // Not fatal to the chain: the pin did its job. It is still worth
            // saying loudly, because a mirror serving the wrong bytes for a
            // content-addressed key is either corrupt or hostile.
            bail!("digest mismatch: expected {expected_sha256}, got {actual}");
        }
        Ok(())
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let bytes = if let Some(path) = local_path(url) {
            let metadata =
                fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
            if metadata.len() > MAX_METADATA_BYTES {
                bail!("metadata document is larger than {MAX_METADATA_BYTES} bytes");
            }
            fs::read(&path).with_context(|| format!("reading {}", path.display()))?
        } else {
            validate_public_url(url)?;
            let response = self
                .client
                .get(url)
                .send()
                .map_err(|error| anyhow!("{}", error.without_url()))?;
            let status = response.status();
            if !status.is_success() {
                bail!("HTTP {status}");
            }
            let mut buffer = Vec::new();
            response
                .take(MAX_METADATA_BYTES.saturating_add(1))
                .read_to_end(&mut buffer)?;
            if buffer.len() as u64 > MAX_METADATA_BYTES {
                bail!("metadata document is larger than {MAX_METADATA_BYTES} bytes");
            }
            buffer
        };
        serde_json::from_slice(&bytes).context("mirror served a document zed cannot parse")
    }
}

/// Merge mirror sets from several sources into one ordered, deduplicated list.
///
/// Order matters and is not simply concatenation: local configuration should
/// be able to put a corporate cache in front of everything, and a locked entry
/// should not be silently displaced by whatever the registry says today. So
/// every candidate keeps its own priority and the merged list re-sorts, with
/// the first occurrence of an id winning on conflict.
pub fn merge_mirrors(sources: &[&[MirrorDescriptorV1]]) -> Result<Vec<MirrorDescriptorV1>> {
    let mut merged: Vec<MirrorDescriptorV1> = Vec::new();
    for source in sources {
        for mirror in source.iter() {
            let id = mirror.identifier();
            if merged.iter().any(|existing| existing.identifier() == id) {
                continue;
            }
            if mirror.validate().is_err() {
                // A single malformed entry from one source must not sink an
                // install that other sources can satisfy. Skipping is right
                // here; `zed mirror check` is where a human sees the detail.
                continue;
            }
            merged.push(mirror.clone());
        }
    }
    merged.sort_by_key(MirrorDescriptorV1::order_key);
    merged.truncate(zed_interfaces::mirror::MAX_MIRRORS);
    Ok(merged)
}

/// The mirror describing the registry a package was locked against, so the
/// canonical source participates in the same ordered chain as everything else
/// instead of being a special case in every caller.
pub fn registry_mirror(base_url: &str) -> Option<MirrorDescriptorV1> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if let Some(path) = trimmed.strip_prefix("file://") {
        let mut mirror = MirrorDescriptorV1::object_store(trimmed);
        mirror.kind = MirrorKindV1::Directory;
        mirror.url = None;
        mirror.path = Some(path.to_owned());
        mirror.priority = Some(0);
        return Some(mirror);
    }
    let mut mirror = MirrorDescriptorV1::object_store(trimmed);
    mirror.kind = MirrorKindV1::ZedRegistry;
    // Priority zero: the canonical source is always tried first. Everything
    // else in this module exists for when that attempt fails.
    mirror.priority = Some(0);
    Some(mirror)
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn local_path(url: &str) -> Option<std::path::PathBuf> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    // Same rule as the file registry: a non-local authority would become a
    // UNC path on Windows and reach the network from a "local" mirror.
    match parsed.host_str() {
        None | Some("") => {}
        Some(host) if host.eq_ignore_ascii_case("localhost") => {}
        Some(_) => return None,
    }
    parsed.to_file_path().ok()
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    matches!(url.host_str(), Some(host) if host.eq_ignore_ascii_case("localhost"))
        || url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback())
}

/// A mirror URL must be public and anonymous. Userinfo is rejected outright:
/// a credential in a mirror URL is a credential in a lockfile, and lockfiles
/// get committed.
fn validate_public_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("mirror produced an invalid URL")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("mirror URL must not carry credentials");
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if url_is_loopback(&parsed) => Ok(()),
        other => bail!("refusing a mirror fetch over `{other}` from a non-loopback host"),
    }
}

fn strip_query(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let _ = parsed.set_password(None);
            let _ = parsed.set_username("");
            parsed.to_string()
        }
        Err(_) => url.split(['?', '#']).next().unwrap_or("").to_owned(),
    }
}

/// Flatten an error chain onto one line. Mirror diagnostics list many
/// attempts; a multi-line cause chain per attempt would bury the one that
/// matters.
fn brief(error: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in error.chain() {
        let text = cause.to_string();
        if !parts.contains(&text) {
            parts.push(text);
        }
    }
    parts.join(": ")
}

fn exhausted(subject: &str, failed: &[MirrorAttempt]) -> anyhow::Error {
    if failed.is_empty() {
        return anyhow!("no mirror is able to serve {subject}");
    }
    let detail = failed
        .iter()
        .map(|attempt| {
            if attempt.url.is_empty() {
                format!("  - {}: {}", attempt.mirror, attempt.reason)
            } else {
                format!(
                    "  - {} [{}]: {}",
                    attempt.mirror, attempt.url, attempt.reason
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow!("every source for {subject} failed:\n{detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::artifact::ArtifactFormat;

    fn coord() -> MirrorCoordinateV1<'static> {
        MirrorCoordinateV1 {
            org: "acme",
            name: "http-kit",
            version: "1.2.0",
            sha256: "aa".repeat(32).leak(),
            format: ArtifactFormat::TarGz,
            vcs_tag: "v1.2.0",
        }
    }

    #[test]
    fn github_release_artifact_url_needs_no_api_call() {
        let mirror = MirrorDescriptorV1::github_release_of("https://github.com/acme/http-kit");
        let urls = mirror.artifact_urls(&coord()).expect("urls");
        assert!(
            urls.iter().any(|url| url
                .starts_with("https://github.com/acme/http-kit/releases/download/v1.2.0/zpkg-")),
            "expected a public release-asset URL, got {urls:?}"
        );
    }

    #[test]
    fn object_store_key_matches_the_server_layout() {
        let mirror = MirrorDescriptorV1::object_store("https://cdn.zpkg.net");
        let urls = mirror.artifact_urls(&coord()).expect("urls");
        assert_eq!(
            urls,
            vec![format!(
                "https://cdn.zpkg.net/artifacts/{}.tar.gz",
                "aa".repeat(32)
            )]
        );
    }

    #[test]
    fn alternate_urls_are_tried_before_the_next_mirror() {
        let mut mirror = MirrorDescriptorV1::object_store("https://cdn.zpkg.net");
        mirror.alternate_urls = vec!["https://zpkg-cdn.example.workers.dev".to_owned()];
        let urls = mirror.artifact_urls(&coord()).expect("urls");
        assert_eq!(urls.len(), 2);
        assert!(urls[1].starts_with("https://zpkg-cdn.example.workers.dev/artifacts/"));
    }

    #[test]
    fn registry_is_first_in_a_merged_chain() {
        let registry = registry_mirror("https://registry.zpkg.net").expect("mirror");
        let cdn = MirrorDescriptorV1::object_store("https://cdn.zpkg.net");
        let merged = merge_mirrors(&[&[cdn], &[registry]]).expect("merge");
        assert_eq!(merged[0].kind, MirrorKindV1::ZedRegistry);
    }

    #[test]
    fn credentials_in_a_mirror_url_are_refused() {
        let error = validate_public_url("https://user:secret@example.com/x").unwrap_err();
        assert!(error.to_string().contains("credentials"), "{error}");
    }

    #[test]
    fn plaintext_is_refused_off_loopback() {
        assert!(validate_public_url("http://example.com/x").is_err());
        assert!(validate_public_url("http://127.0.0.1:8080/x").is_ok());
    }

    #[test]
    fn strip_query_removes_presigned_signatures() {
        let stripped = strip_query("https://cdn.example.com/a?X-Amz-Signature=deadbeef");
        assert_eq!(stripped, "https://cdn.example.com/a");
    }

    #[test]
    fn file_mirror_rejects_a_remote_authority() {
        assert!(local_path("file://evil.example.com/etc/passwd").is_none());
        assert!(local_path("file:///tmp/mirror/artifacts/x.tar.gz").is_some());
    }
}
