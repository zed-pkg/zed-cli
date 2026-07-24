//! `zed update self` (zed-docs issue #9): check GitHub Releases for a newer
//! `zed`, download the artifact matching this platform, and replace the
//! running binary in place. Pairs with the cross-platform release matrix
//! (`release.yml`) that publishes `zed-<target>.{tar.gz,zip}` assets.

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The CLI's own source repository, where releases are published.
const REPO: &str = "zed-pkg/zed-cli";

/// The release-asset target triple for the current platform, matching the
/// names produced by `release.yml` (e.g. `aarch64-apple-darwin`).
pub fn asset_target() -> Result<String> {
    let arch = std::env::consts::ARCH; // x86_64 | aarch64
    Ok(match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => {
            let libc = if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            };
            format!("{arch}-unknown-linux-{libc}")
        }
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => bail!("self-update is not supported on `{other}`"),
    })
}

/// The tag from a resolved `/releases/latest` URL. GitHub 302-redirects
/// `/releases/latest` to `/releases/tag/<tag>` when a release exists (and to
/// `/releases` when none do), so this needs no API token and dodges the API
/// rate limit (same trick as `install.sh`). Returns `None` when there is no
/// release to point at.
pub fn tag_from_latest_url(url: &str) -> Option<String> {
    let url = url.trim_end_matches('/');
    let marker = "/releases/tag/";
    let idx = url.find(marker)?;
    let tag = url[idx + marker.len()..].split('/').next().unwrap_or("");
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

/// Is `latest_tag` (e.g. `v0.1.1`) a newer semver than `current` (`0.1.0`)?
pub fn is_newer(current: &str, latest_tag: &str) -> bool {
    let strip = |s: &str| s.trim().trim_start_matches('v').to_string();
    match (
        semver::Version::parse(&strip(current)),
        semver::Version::parse(&strip(latest_tag)),
    ) {
        (Ok(cur), Ok(new)) => new > cur,
        _ => false,
    }
}

/// Extract the `zed` (or `zed.exe`) binary bytes from a release archive.
fn extract_binary(bytes: &[u8], bin_name: &str, is_zip: bool) -> Result<Vec<u8>> {
    if is_zip {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file.name().rsplit('/').next().unwrap_or("").to_string();
            if name == bin_name {
                let mut out = Vec::new();
                file.read_to_end(&mut out)?;
                return Ok(out);
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name == bin_name {
                let mut out = Vec::new();
                entry.read_to_end(&mut out)?;
                return Ok(out);
            }
        }
    }
    bail!("release archive did not contain a `{bin_name}` binary");
}

/// Atomically replace the executable at `exe` with `new_bytes`.
fn replace_exe(exe: &Path, new_bytes: &[u8]) -> Result<()> {
    let dir = exe.parent().context("executable has no parent directory")?;
    let tmp = dir.join(".zed-update.tmp");
    std::fs::write(&tmp, new_bytes)
        .with_context(|| format!("writing new binary to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // On Unix, renaming over the running binary is safe: the running process
    // keeps its open inode. On Windows the running image is locked, so move it
    // aside first.
    #[cfg(windows)]
    {
        let old = dir.join(".zed-update.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
    }
    std::fs::rename(&tmp, exe).with_context(|| format!("replacing {}", exe.display()))?;
    Ok(())
}

/// Run the self-update. `check` only reports; `force` reinstalls even when
/// already current.
pub fn self_update(current_version: &str, check: bool, force: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let latest_url = format!("https://github.com/{REPO}/releases/latest");
    let resp = client
        .get(&latest_url)
        .send()
        .context("querying GitHub for the latest release")?;
    let tag = tag_from_latest_url(resp.url().as_str())
        .context("could not determine the latest release tag (no releases yet?)")?;

    println!("current v{current_version}, latest {tag}");
    if !force && !is_newer(current_version, &tag) {
        println!("already up to date");
        return Ok(());
    }
    if check {
        println!("update available: {tag} — run `zed update self` to install");
        return Ok(());
    }

    let target = asset_target()?;
    let is_zip = std::env::consts::OS == "windows";
    let asset = if is_zip {
        format!("zed-{target}.zip")
    } else {
        format!("zed-{target}.tar.gz")
    };
    let download_url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    println!("downloading {download_url}");
    let bytes = client
        .get(&download_url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading release asset {asset}"))?
        .bytes()
        .context("reading release asset")?;

    let bin_name = if is_zip { "zed.exe" } else { "zed" };
    let new_bin = extract_binary(&bytes, bin_name, is_zip)?;
    let exe = std::env::current_exe().context("locating the running executable")?;
    replace_exe(&exe, &new_bin)?;
    println!("updated to {tag}: {}", exe.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_parsing_from_redirect_url() {
        assert_eq!(
            tag_from_latest_url("https://github.com/zed-pkg/zed-cli/releases/tag/v0.2.0")
                .as_deref(),
            Some("v0.2.0")
        );
        // The unresolved /latest URL yields no tag.
        assert_eq!(
            tag_from_latest_url("https://github.com/zed-pkg/zed-cli/releases/latest"),
            None
        );
        // No releases at all: GitHub lands on /releases.
        assert_eq!(
            tag_from_latest_url("https://github.com/zed-pkg/zed-cli/releases"),
            None
        );
    }

    #[test]
    fn semver_comparison_strips_v() {
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("1.0.0", "v1.0.0"));
        assert!(!is_newer("1.2.0", "v1.1.9"));
        assert!(!is_newer("0.1.0", "not-a-version"));
    }

    #[test]
    fn asset_target_is_platform_shaped() {
        let t = asset_target().unwrap();
        assert!(t.contains(std::env::consts::ARCH));
        #[cfg(target_os = "macos")]
        assert!(t.ends_with("apple-darwin"));
        #[cfg(target_os = "linux")]
        assert!(t.contains("unknown-linux-"));
    }
}
