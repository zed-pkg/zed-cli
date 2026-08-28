//! Publishing a copy of every artifact to the forge the package already lives
//! on.
//!
//! zed already requires a tag on the source repository that points at the
//! published commit — that tag is the provenance anchor. This module notices
//! that the tag is one upload away from being a complete, public, free,
//! independently-operated artifact source, and takes that upload.
//!
//! What lands on a release:
//!
//! ```text
//! github.com/acme/http-kit
//!   releases/tag/v1.2.0            (immutable, one per version)
//!     zpkg-<sha256>.tar.gz         the artifact, named by its digest
//!     zpkg-version.json            signed version metadata
//!   releases/tag/zpkg-index        (rolling, one per repository)
//!     zpkg-index-acme-http-kit.json    signed version index
//! ```
//!
//! The split is not arbitrary. Version metadata is immutable and belongs on
//! the immutable release; the index changes on every publish and therefore
//! cannot live there. One rolling release per repository keeps the mutable
//! part to exactly one well-known place.
//!
//! Asset names are digests, not versions, so the artifact object is addressed
//! the same way it is in the bucket, in the store, and in the lockfile — one
//! naming scheme across every transport, and the name itself is the integrity
//! check.
//!
//! Everything here is best-effort by design. A forge outage must not fail a
//! publish that the registry already accepted: the registry is the canonical
//! destination, and the mirror is the copy.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use zed_interfaces::mirror::{
    DEFAULT_ASSET_PREFIX, DEFAULT_INDEX_TAG, DEFAULT_RAW_BRANCH, DEFAULT_RAW_PREFIX, GITHUB_HOST,
    MirrorDescriptorV1, MirrorKindV1, RepoRefV1, parse_repo_ref,
};
use zed_interfaces::signing::{SignedIndexV1, SignedVersionV1};

/// Environment variables checked for a forge token, in order.
///
/// `ZED_PKG_GITHUB_TOKEN` first so a publish can use a narrowly-scoped token
/// without disturbing whatever `gh` or Actions put in the environment for
/// everything else in the job.
const TOKEN_ENV: &[&str] = &["ZED_PKG_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// One planned or completed upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeUpload {
    pub repository: String,
    pub tag: String,
    pub asset: String,
    pub outcome: ForgeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeOutcome {
    Uploaded,
    /// The asset was already there with the same size. For a digest-named
    /// artifact that is conclusive; re-uploading identical bytes would burn
    /// bandwidth to change nothing.
    AlreadyPresent,
    Replaced,
    Planned,
    Skipped(String),
}

impl ForgeOutcome {
    pub fn as_str(&self) -> &str {
        match self {
            ForgeOutcome::Uploaded => "uploaded",
            ForgeOutcome::AlreadyPresent => "already present",
            ForgeOutcome::Replaced => "replaced",
            ForgeOutcome::Planned => "planned",
            ForgeOutcome::Skipped(_) => "skipped",
        }
    }
}

pub struct ForgeClient {
    http: reqwest::blocking::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    id: u64,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    id: u64,
    name: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Serialize)]
struct CreateRelease<'a> {
    tag_name: &'a str,
    name: &'a str,
    body: &'a str,
    draft: bool,
    prerelease: bool,
}

impl ForgeClient {
    /// Build a client, or explain why the forge mirror will be skipped.
    ///
    /// Returning `Ok(None)` rather than an error is deliberate: a publisher
    /// with no forge token still gets a successful registry publish, and a
    /// clear line saying the mirror was not written.
    pub fn discover() -> Result<Option<Self>> {
        let Some(token) = forge_token() else {
            return Ok(None);
        };
        Ok(Some(Self {
            http: reqwest::blocking::Client::builder()
                .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
                .timeout(UPLOAD_TIMEOUT)
                // Never replay a bearer token through a redirect to a host the
                // response chose.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            token,
        }))
    }

    /// Mirror one published version onto its forge.
    pub fn publish_version(
        &self,
        mirror: &MirrorDescriptorV1,
        artifact: &Path,
        signed_version: &SignedVersionV1,
        signed_index: Option<&SignedIndexV1>,
        dry_run: bool,
    ) -> Result<Vec<ForgeUpload>> {
        if mirror.kind != MirrorKindV1::GithubRelease {
            bail!(
                "mirror `{}` is not a github-release mirror",
                mirror.identifier()
            );
        }
        let repo = mirror.repo_ref()?;
        let payload = &signed_version.payload;
        let tag = if payload.vcs_tag.is_empty() {
            format!("v{}", payload.version)
        } else {
            payload.vcs_tag.clone()
        };
        let prefix = mirror
            .asset_prefix
            .as_deref()
            .unwrap_or(DEFAULT_ASSET_PREFIX);

        let artifact_asset = format!("{prefix}{}.{}", payload.sha256, payload.format.extension());
        let metadata_asset = format!("{prefix}version.json");
        let metadata_bytes = serde_json::to_vec_pretty(signed_version)?;

        let mut uploads = Vec::new();
        if dry_run {
            uploads.push(planned(&repo, &tag, &artifact_asset));
            uploads.push(planned(&repo, &tag, &metadata_asset));
            if signed_index.is_some() {
                uploads.push(planned(
                    &repo,
                    DEFAULT_INDEX_TAG,
                    &index_asset_name(prefix, &payload.org, &payload.name),
                ));
            }
            return Ok(uploads);
        }

        let release = self.ensure_release(&repo, &tag, &release_notes(&payload.version))?;
        uploads.push(self.upload_asset(
            &repo,
            &release,
            &tag,
            &artifact_asset,
            &fs::read(artifact).with_context(|| format!("reading {}", artifact.display()))?,
            "application/octet-stream",
            // Digest-named: identical name plus identical size means identical
            // bytes, so an existing asset is already correct.
            Replace::OnlyIfDifferentSize,
        )?);
        uploads.push(self.upload_asset(
            &repo,
            &release,
            &tag,
            &metadata_asset,
            &metadata_bytes,
            "application/json",
            // Not digest-named, and a re-publish may add mirrors or a second
            // signature. Always take the newer document.
            Replace::Always,
        )?);

        if let Some(index) = signed_index {
            let index_release = self.ensure_release(
                &repo,
                DEFAULT_INDEX_TAG,
                "Rolling zed package indexes. Generated by `zed publish`; do not edit.",
            )?;
            uploads.push(self.upload_asset(
                &repo,
                &index_release,
                DEFAULT_INDEX_TAG,
                &index_asset_name(prefix, &index.payload.org, &index.payload.name),
                &serde_json::to_vec_pretty(index)?,
                "application/json",
                Replace::Always,
            )?);
        }
        Ok(uploads)
    }

    /// Write the raw-served mirror tree for a `github-raw` mirror.
    ///
    /// A branch, not a release: raw content is what makes an index cheap to
    /// read — cacheable, no API call, no token — which is exactly the property
    /// a resolver wants when it is already having a bad day.
    pub fn publish_raw(
        &self,
        mirror: &MirrorDescriptorV1,
        signed_version: &SignedVersionV1,
        signed_index: Option<&SignedIndexV1>,
        dry_run: bool,
    ) -> Result<Vec<ForgeUpload>> {
        if mirror.kind != MirrorKindV1::GithubRaw {
            bail!(
                "mirror `{}` is not a github-raw mirror",
                mirror.identifier()
            );
        }
        let repo = mirror.repo_ref()?;
        let branch = mirror.branch.as_deref().unwrap_or(DEFAULT_RAW_BRANCH);
        let payload = &signed_version.payload;

        let mut files: Vec<(String, Vec<u8>)> = vec![(
            format!(
                "metadata/{}/{}/versions/{}.json",
                payload.org, payload.name, payload.version
            ),
            serde_json::to_vec_pretty(signed_version)?,
        )];
        if let Some(index) = signed_index {
            files.push((
                format!(
                    "metadata/{}/{}/index.json",
                    index.payload.org, index.payload.name
                ),
                serde_json::to_vec_pretty(index)?,
            ));
        }

        let mut uploads = Vec::new();
        for (path, bytes) in files {
            if dry_run {
                uploads.push(planned(&repo, branch, &path));
                continue;
            }
            uploads.push(self.put_content(&repo, branch, &path, &bytes)?);
        }
        Ok(uploads)
    }

    /// Upload only the package index, onto the rolling index release.
    ///
    /// Separate from [`Self::publish_version`] because the index is written on
    /// its own schedule: after a publish the registry has already accepted,
    /// and again after a yank, which adds no version at all.
    pub fn publish_index_only(
        &self,
        mirror: &MirrorDescriptorV1,
        signed_index: &SignedIndexV1,
        dry_run: bool,
    ) -> Result<Vec<ForgeUpload>> {
        if mirror.kind != MirrorKindV1::GithubRelease {
            bail!(
                "mirror `{}` is not a github-release mirror",
                mirror.identifier()
            );
        }
        let repo = mirror.repo_ref()?;
        let prefix = mirror
            .asset_prefix
            .as_deref()
            .unwrap_or(DEFAULT_ASSET_PREFIX);
        let tag = mirror.index_tag.as_deref().unwrap_or(DEFAULT_INDEX_TAG);
        let asset = index_asset_name(
            prefix,
            &signed_index.payload.org,
            &signed_index.payload.name,
        );
        if dry_run {
            return Ok(vec![planned(&repo, tag, &asset)]);
        }
        let release = self.ensure_release(
            &repo,
            tag,
            "Rolling zed package indexes. Generated by `zed mirror publish-index`; do not edit.",
        )?;
        Ok(vec![self.upload_asset(
            &repo,
            &release,
            tag,
            &asset,
            &serde_json::to_vec_pretty(signed_index)?,
            "application/json",
            Replace::Always,
        )?])
    }

    /// Write only the package index into a raw-served mirror tree.
    pub fn publish_raw_index(
        &self,
        mirror: &MirrorDescriptorV1,
        signed_index: &SignedIndexV1,
        dry_run: bool,
    ) -> Result<Vec<ForgeUpload>> {
        if mirror.kind != MirrorKindV1::GithubRaw {
            bail!(
                "mirror `{}` is not a github-raw mirror",
                mirror.identifier()
            );
        }
        let repo = mirror.repo_ref()?;
        let branch = mirror.branch.as_deref().unwrap_or(DEFAULT_RAW_BRANCH);
        let path = format!(
            "metadata/{}/{}/index.json",
            signed_index.payload.org, signed_index.payload.name
        );
        if dry_run {
            return Ok(vec![planned(&repo, branch, &path)]);
        }
        Ok(vec![self.put_content(
            &repo,
            branch,
            &path,
            &serde_json::to_vec_pretty(signed_index)?,
        )?])
    }

    fn ensure_release(&self, repo: &RepoRefV1, tag: &str, body: &str) -> Result<Release> {
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            api_base(repo),
            repo.owner,
            repo.repo,
            tag
        );
        let response = self.get(&url)?;
        if response.status().is_success() {
            return Ok(response.json::<Release>()?);
        }
        if response.status() != reqwest::StatusCode::NOT_FOUND {
            bail!(
                "reading release `{tag}` on {}/{} failed with HTTP {}",
                repo.owner,
                repo.repo,
                response.status()
            );
        }
        // The tag itself already exists — `zed publish` verified it points at
        // the published commit before any of this ran. Creating the release is
        // creating the container for assets, not creating provenance.
        let created = self
            .http
            .post(format!(
                "{}/repos/{}/{}/releases",
                api_base(repo),
                repo.owner,
                repo.repo
            ))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(API_TIMEOUT)
            .json(&CreateRelease {
                tag_name: tag,
                name: tag,
                body,
                draft: false,
                prerelease: false,
            })
            .send()?;
        if !created.status().is_success() {
            let status = created.status();
            bail!(
                "creating release `{tag}` on {}/{} failed with HTTP {status}: {}",
                repo.owner,
                repo.repo,
                truncate(&created.text().unwrap_or_default())
            );
        }
        Ok(created.json::<Release>()?)
    }

    fn upload_asset(
        &self,
        repo: &RepoRefV1,
        release: &Release,
        tag: &str,
        name: &str,
        bytes: &[u8],
        content_type: &str,
        replace: Replace,
    ) -> Result<ForgeUpload> {
        let existing = release.assets.iter().find(|asset| asset.name == name);
        let mut outcome = ForgeOutcome::Uploaded;
        if let Some(asset) = existing {
            match replace {
                Replace::OnlyIfDifferentSize if asset.size == bytes.len() as u64 => {
                    return Ok(ForgeUpload {
                        repository: repo_label(repo),
                        tag: tag.to_owned(),
                        asset: name.to_owned(),
                        outcome: ForgeOutcome::AlreadyPresent,
                    });
                }
                _ => {
                    self.delete_asset(repo, asset.id)?;
                    outcome = ForgeOutcome::Replaced;
                }
            }
        }

        let url = format!(
            "{}/repos/{}/{}/releases/{}/assets?name={}",
            upload_base(repo),
            repo.owner,
            repo.repo,
            release.id,
            urlencode(name)
        );
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Content-Type", content_type)
            .body(bytes.to_vec())
            .send()?;
        if !response.status().is_success() {
            let status = response.status();
            bail!(
                "uploading `{name}` to {}@{tag} failed with HTTP {status}: {}",
                repo_label(repo),
                truncate(&response.text().unwrap_or_default())
            );
        }
        Ok(ForgeUpload {
            repository: repo_label(repo),
            tag: tag.to_owned(),
            asset: name.to_owned(),
            outcome,
        })
    }

    fn delete_asset(&self, repo: &RepoRefV1, asset_id: u64) -> Result<()> {
        let response = self
            .http
            .delete(format!(
                "{}/repos/{}/{}/releases/assets/{asset_id}",
                api_base(repo),
                repo.owner,
                repo.repo
            ))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(API_TIMEOUT)
            .send()?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            bail!(
                "replacing an existing asset on {} failed with HTTP {}",
                repo_label(repo),
                response.status()
            );
        }
        Ok(())
    }

    fn put_content(
        &self,
        repo: &RepoRefV1,
        branch: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<ForgeUpload> {
        let full_path = format!("{DEFAULT_RAW_PREFIX}/{path}");
        let url = format!(
            "{}/repos/{}/{}/contents/{}",
            api_base(repo),
            repo.owner,
            repo.repo,
            full_path
        );

        // The Contents API needs the blob sha of what it is replacing, or it
        // refuses the write. Reading it first is also the only concurrency
        // control available here: a stale sha fails loudly instead of silently
        // clobbering a parallel publish.
        #[derive(Deserialize)]
        struct Existing {
            sha: String,
        }
        let existing = self
            .get(&format!("{url}?ref={}", urlencode(branch)))?
            .json::<Existing>()
            .ok();

        #[derive(Serialize)]
        struct Put<'a> {
            message: &'a str,
            content: String,
            branch: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            sha: Option<String>,
        }

        let response = self
            .http
            .put(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(API_TIMEOUT)
            .json(&Put {
                message: &format!("zed: mirror {path}"),
                content: base64_standard(bytes),
                branch,
                sha: existing.map(|existing| existing.sha),
            })
            .send()?;
        if !response.status().is_success() {
            let status = response.status();
            bail!(
                "writing `{full_path}` on {}#{branch} failed with HTTP {status}: {}",
                repo_label(repo),
                truncate(&response.text().unwrap_or_default())
            );
        }
        Ok(ForgeUpload {
            repository: repo_label(repo),
            tag: branch.to_owned(),
            asset: full_path,
            outcome: ForgeOutcome::Uploaded,
        })
    }

    fn get(&self, url: &str) -> Result<reqwest::blocking::Response> {
        self.http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(API_TIMEOUT)
            .send()
            .map_err(|error| anyhow!("{}", error.without_url()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replace {
    Always,
    OnlyIfDifferentSize,
}

fn planned(repo: &RepoRefV1, tag: &str, asset: &str) -> ForgeUpload {
    ForgeUpload {
        repository: repo_label(repo),
        tag: tag.to_owned(),
        asset: asset.to_owned(),
        outcome: ForgeOutcome::Planned,
    }
}

fn index_asset_name(prefix: &str, org: &str, name: &str) -> String {
    format!("{prefix}index-{org}-{name}.json")
}

fn release_notes(version: &str) -> String {
    format!(
        "Release {version}.\n\n\
         Assets prefixed `zpkg-` are a mirror of the zed package registry, \
         written by `zed publish`. The `.tar.gz` is named by its sha256 and is \
         byte-identical to what the registry serves; `zpkg-version.json` carries \
         the publisher-signed metadata."
    )
}

fn repo_label(repo: &RepoRefV1) -> String {
    format!("{}/{}/{}", repo.host, repo.owner, repo.repo)
}

fn api_base(repo: &RepoRefV1) -> String {
    if repo.host == GITHUB_HOST {
        "https://api.github.com".to_owned()
    } else {
        // GitHub Enterprise Server, and the shape Gitea and Forgejo also use
        // for their GitHub-compatible surface.
        format!("https://{}/api/v3", repo.host)
    }
}

fn upload_base(repo: &RepoRefV1) -> String {
    if repo.host == GITHUB_HOST {
        "https://uploads.github.com".to_owned()
    } else {
        format!("https://{}/api/uploads", repo.host)
    }
}

fn forge_token() -> Option<String> {
    for name in TOKEN_ENV {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    // `gh` keeps its token in a keyring the environment does not expose, and
    // asking it is the difference between "works on a developer laptop" and
    // "works only in CI".
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

/// Which of a package's mirrors this module can write to.
pub fn writable(mirrors: &[MirrorDescriptorV1]) -> Vec<&MirrorDescriptorV1> {
    mirrors
        .iter()
        .filter(|mirror| {
            matches!(
                mirror.kind,
                MirrorKindV1::GithubRelease | MirrorKindV1::GithubRaw
            ) && mirror
                .repository
                .as_deref()
                .and_then(|value| parse_repo_ref(value).ok())
                .is_some()
        })
        .collect()
}

/// Standard base64 with padding, as the Contents API expects. The signing
/// module's base64url is a different alphabet and would be rejected here.
fn base64_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn truncate(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= 400 {
        return trimmed.to_owned();
    }
    format!("{}…", &trimmed[..400])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64_standard(b""), "");
        assert_eq!(base64_standard(b"f"), "Zg==");
        assert_eq!(base64_standard(b"fo"), "Zm8=");
        assert_eq!(base64_standard(b"foo"), "Zm9v");
        assert_eq!(base64_standard(b"foob"), "Zm9vYg==");
        assert_eq!(base64_standard(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_standard(&[0xff, 0xef, 0xbe]), "/+++");
    }

    #[test]
    fn enterprise_hosts_get_the_v3_api_and_upload_paths() {
        let repo = RepoRefV1 {
            host: "ghe.example.com".to_owned(),
            owner: "acme".to_owned(),
            repo: "http-kit".to_owned(),
        };
        assert_eq!(api_base(&repo), "https://ghe.example.com/api/v3");
        assert_eq!(upload_base(&repo), "https://ghe.example.com/api/uploads");
    }

    #[test]
    fn github_dot_com_gets_the_public_hosts() {
        let repo = parse_repo_ref("git@github.com:acme/http-kit.git").expect("repo");
        assert_eq!(api_base(&repo), "https://api.github.com");
        assert_eq!(upload_base(&repo), "https://uploads.github.com");
    }

    #[test]
    fn only_forge_mirrors_are_writable() {
        let cdn = MirrorDescriptorV1::object_store("https://cdn.zpkg.net");
        let forge = MirrorDescriptorV1::github_release_of("https://github.com/acme/http-kit");
        let writable = writable(&[cdn, forge]);
        assert_eq!(writable.len(), 1);
        assert_eq!(writable[0].kind, MirrorKindV1::GithubRelease);
    }
}
