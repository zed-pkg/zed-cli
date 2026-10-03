//! Mirror a packed Zed artifact onto a GitHub Release so `registry.zpkg.net`
//! outages still have a public download. Also ensures the matching git tag
//! exists on GitHub so Releases and GitHub Packages share one provenance ref.

use std::fs;

use anyhow::{Context, Result, bail};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::registry::VersionMetadata;
use zed_interfaces::source::{
    GithubIdentity, github_api_git_refs_url, github_api_git_tag_url, github_api_release_url,
    github_api_repo_url, github_release_asset_names, github_release_sidecar_names,
    parse_github_identity,
};

use crate::pack::PackResult;
use crate::source_fallback::SourceFallbackConfig;

/// Upload the packed artifact and a VersionMetadata sidecar when the package
/// is a GitHub repo and a token is available. Missing credentials are a
/// warning, not a publish failure: the registry remains the primary host.
pub fn mirror_packed_release(
    manifest: &Manifest,
    packed: &PackResult,
    vcs_tag: &str,
    vcs_commit: Option<&str>,
    download_url: &str,
) -> Result<MirrorOutcome> {
    if !manifest
        .package
        .artifacts
        .github_release_enabled(Some(manifest.package.repository.url.as_str()))
    {
        return Ok(MirrorOutcome::Skipped("github_release disabled"));
    }
    let Some(identity) = parse_github_identity(&manifest.package.repository.url) else {
        return Ok(MirrorOutcome::Skipped("repository is not github.com"));
    };
    let config = SourceFallbackConfig::from_env();
    let Some(token) = config.github_token.as_deref() else {
        eprintln!(
            "warning: GitHub release mirror skipped for {} (set GITHUB_TOKEN / GH_TOKEN / ZED_PKG_GITHUB_TOKEN)",
            manifest.full_name()
        );
        return Ok(MirrorOutcome::Skipped("no GitHub token"));
    };

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    ensure_remote_git_tag(&client, token, &identity, vcs_tag, vcs_commit)?;
    let release = ensure_release(&client, token, &identity, vcs_tag, vcs_commit)?;
    let org = manifest.package.org.as_str();
    let name = manifest.package.name.as_str();
    let version = manifest.package.version.as_str();
    let ext = packed.format.extension();
    let asset = github_release_asset_names(org, name, version, ext)
        .into_iter()
        .next()
        .expect("at least one asset name");
    let sidecar = github_release_sidecar_names(org, name, version)
        .into_iter()
        .next()
        .expect("at least one sidecar name");

    upload_asset(
        &client,
        token,
        &identity,
        release.id,
        &asset,
        packed.format.content_type(),
        &fs::read(&packed.path)?,
    )?;

    let metadata = VersionMetadata {
        org: org.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
        vcs_tag: vcs_tag.to_string(),
        vcs_commit: vcs_commit.map(str::to_string),
        download_url: download_url.to_string(),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
        mirrors: Vec::new(),
    };
    let sidecar_bytes = serde_json::to_vec_pretty(&metadata)?;
    upload_asset(
        &client,
        token,
        &identity,
        release.id,
        &sidecar,
        "application/json",
        &sidecar_bytes,
    )?;
    Ok(MirrorOutcome::Uploaded {
        owner: identity.owner,
        repo: identity.repo,
        tag: vcs_tag.to_string(),
        asset,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum MirrorOutcome {
    Skipped(&'static str),
    Uploaded {
        owner: String,
        repo: String,
        tag: String,
        asset: String,
    },
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseResponse {
    id: u64,
}

/// Create `refs/tags/{tag}` on GitHub when missing. Idempotent when the remote
/// tag already points at `commit`. A mismatched remote tag is refused so a
/// GitHub Packages / Release publish cannot silently retarget provenance.
pub fn ensure_remote_git_tag(
    client: &reqwest::blocking::Client,
    token: &str,
    identity: &GithubIdentity,
    tag: &str,
    commit: Option<&str>,
) -> Result<()> {
    let get = client
        .get(github_api_git_tag_url(identity, tag))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()?;
    if get.status().is_success() {
        return Ok(());
    }
    let Some(commit) = commit else {
        return Ok(());
    };
    let create = client
        .post(github_api_git_refs_url(identity))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(&serde_json::json!({
            "ref": format!("refs/tags/{tag}"),
            "sha": commit,
        }))
        .send()?;
    if create.status().is_success() || create.status().as_u16() == 422 {
        return Ok(());
    }
    bail!(
        "could not create git tag `{tag}` on {}: {}",
        identity.web_url(),
        create.status()
    )
}

fn ensure_release(
    client: &reqwest::blocking::Client,
    token: &str,
    identity: &GithubIdentity,
    tag: &str,
    commit: Option<&str>,
) -> Result<ReleaseResponse> {
    let get = client
        .get(github_api_release_url(identity, tag))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()?;
    if get.status().is_success() {
        return Ok(get.json()?);
    }
    let mut body = serde_json::json!({
        "tag_name": tag,
        "name": tag,
        "draft": false,
        "prerelease": false,
    });
    if let Some(commit) = commit {
        body["target_commitish"] = serde_json::Value::String(commit.to_string());
    }
    let create = client
        .post(format!("{}/releases", github_api_repo_url(identity)))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(&body)
        .send()?;
    if create.status().is_success() || create.status().as_u16() == 422 {
        let again = client
            .get(github_api_release_url(identity, tag))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()?;
        if again.status().is_success() {
            return Ok(again.json()?);
        }
    }
    bail!(
        "could not create GitHub release {tag} on {}: {}",
        identity.web_url(),
        create.status()
    )
}

fn upload_asset(
    client: &reqwest::blocking::Client,
    token: &str,
    identity: &GithubIdentity,
    release_id: u64,
    name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<()> {
    let url = format!(
        "https://uploads.github.com/repos/{}/{}/releases/{release_id}/assets?name={name}",
        identity.owner, identity.repo
    );
    let response = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("Content-Type", content_type)
        .body(bytes.to_vec())
        .send()
        .with_context(|| format!("upload GitHub release asset {name}"))?;
    // 201 created, 422 already exists — both mean the bytes are on GitHub.
    if response.status().is_success() || response.status().as_u16() == 422 {
        return Ok(());
    }
    bail!("GitHub asset upload {name} returned {}", response.status())
}

#[cfg(test)]
mod tests {
    use zed_interfaces::source::{GithubIdentity, github_api_git_tag_url};

    #[test]
    fn git_tag_ref_url_matches_github_git_api() {
        let identity = GithubIdentity {
            owner: "cliptown".into(),
            repo: "cliptown-cli".into(),
        };
        assert_eq!(
            github_api_git_tag_url(&identity, "v0.1.0"),
            "https://api.github.com/repos/cliptown/cliptown-cli/git/ref/tags/v0.1.0"
        );
    }
}
