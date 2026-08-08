//! `zed update self` (zed-docs issue #9): check GitHub Releases for a newer
//! `zed`, download the artifact matching this platform, and replace the
//! running binary in place. Pairs with the cross-platform release matrix
//! (`release.yml`) that publishes `zed-<target>.{tar.gz,zip}` assets.

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::is_sha256_hex;

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

/// Parse a `SHA256SUMS` file (the `sha256sum` output format, one entry per
/// line: `<hex>␠␠<filename>`, or `<hex>␠*<filename>` in binary mode) and
/// return the expected lowercase digest for `filename`, if present and well
/// formed. Comment/blank lines and entries for other assets are ignored.
fn expected_sha256_for(sums: &str, filename: &str) -> Option<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hex, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // Binary-mode entries prefix the name with `*`; strip it plus any
        // surrounding whitespace before comparing.
        let name = name.trim().trim_start_matches('*').trim();
        if name == filename {
            let hex = hex.trim().to_ascii_lowercase();
            return is_sha256_hex(&hex).then_some(hex);
        }
    }
    None
}

/// Verify a downloaded release asset against the release's published
/// SHA256SUMS before anything is extracted or installed. A corrupted download
/// or swapped asset is caught here — before it can replace the running
/// binary. Failing to FETCH the sums refuses the update (there is nothing to
/// verify against); `skip_checksum` bypasses the whole check for local
/// testing only.
fn verify_asset_checksum(
    client: &reqwest::blocking::Client,
    tag: &str,
    asset: &str,
    bytes: &[u8],
) -> Result<()> {
    let sums_url = format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS");
    let resp = client
        .get(&sums_url)
        .send()
        .with_context(|| format!("fetching {sums_url}"))?;
    if !resp.status().is_success() {
        bail!(
            "refusing to self-update: could not fetch {sums_url} ({}); there is no \
             checksum to verify {asset} against (pass --skip-checksum to override, unsafe)",
            resp.status()
        );
    }
    let sums = resp.text().context("reading SHA256SUMS")?;
    let expected = expected_sha256_for(&sums, asset).with_context(|| {
        format!("SHA256SUMS from the release has no entry for {asset}; refusing to self-update")
    })?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!(
            "checksum mismatch for {asset}: expected {expected}, got {actual}; \
             refusing to replace the binary"
        );
    }
    println!("verified {asset} sha256 {actual}");
    Ok(())
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
    #[cfg(windows)]
    {
        // A live Windows executable can keep the renamed image locked until
        // process exit. Remove the backup whenever Windows permits it, while
        // preserving the successful replacement if the live-image deletion is
        // deferred by the operating system.
        let _ = std::fs::remove_file(dir.join(".zed-update.old"));
    }
    Ok(())
}

/// Run the self-update. `check` only reports; `force` reinstalls even when
/// already current; `skip_checksum` bypasses SHA256SUMS verification (unsafe,
/// local testing only).
pub fn self_update(
    current_version: &str,
    check: bool,
    force: bool,
    skip_checksum: bool,
) -> Result<()> {
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

    if skip_checksum {
        eprintln!(
            "WARNING: --skip-checksum set; installing {asset} WITHOUT verifying its \
             sha256. This defeats self-update integrity checking and is intended \
             only for local testing."
        );
    } else {
        verify_asset_checksum(&client, &tag, &asset, &bytes)?;
    }

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

    const DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn sha256sums_matches_asset_line() {
        let sums = format!(
            "# release checksums\n\
             {DIGEST}  zed-aarch64-apple-darwin.tar.gz\n\
             1111111111111111111111111111111111111111111111111111111111111111  zed-x86_64-unknown-linux-musl.tar.gz\n"
        );
        assert_eq!(
            expected_sha256_for(&sums, "zed-aarch64-apple-darwin.tar.gz").as_deref(),
            Some(DIGEST)
        );
        assert_eq!(
            expected_sha256_for(&sums, "zed-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("1111111111111111111111111111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn sha256sums_handles_binary_mode_and_uppercase() {
        // `sha256sum -b` writes `<hex> *<name>`; digests may be uppercase.
        let sums = format!(
            "{}  *zed-x86_64-pc-windows-msvc.zip\n",
            DIGEST.to_uppercase()
        );
        assert_eq!(
            expected_sha256_for(&sums, "zed-x86_64-pc-windows-msvc.zip").as_deref(),
            Some(DIGEST),
            "expected lowercased digest with the `*` binary-mode marker stripped"
        );
    }

    #[test]
    fn sha256sums_rejects_missing_or_malformed() {
        let sums = format!("{DIGEST}  zed-aarch64-apple-darwin.tar.gz\n");
        // No entry for the requested asset -> None, so the caller aborts.
        assert_eq!(
            expected_sha256_for(&sums, "zed-x86_64-apple-darwin.tar.gz"),
            None
        );
        // A non-hex "digest" for the asset is not accepted.
        let bad = "nothexnothexnothex  zed-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            expected_sha256_for(bad, "zed-aarch64-apple-darwin.tar.gz"),
            None
        );
        // Empty file yields nothing.
        assert_eq!(
            expected_sha256_for("", "zed-aarch64-apple-darwin.tar.gz"),
            None
        );
    }

    #[test]
    fn sha256sums_mismatch_is_detectable() {
        // Mirrors the self_update comparison: a differing digest must not
        // equal the archive's actual hash, so the update is refused.
        let sums = format!("{DIGEST}  zed-aarch64-apple-darwin.tar.gz\n");
        let expected = expected_sha256_for(&sums, "zed-aarch64-apple-darwin.tar.gz").unwrap();
        let actual = "1111111111111111111111111111111111111111111111111111111111111111";
        assert_ne!(expected, actual);
    }

    /// Build an in-memory `.tar.gz` mirroring a release layout: a versioned
    /// top-level directory holding the binary plus decoy files.
    fn release_tar_gz(bin_name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            let mut add = |path: String, bytes: &[u8]| {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, path, bytes).unwrap();
            };
            add("zed-test-target/README.md".to_string(), b"decoy docs");
            add(format!("zed-test-target/{bin_name}"), payload);
            builder.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    #[test]
    fn extract_binary_finds_zed_inside_a_tar_gz() {
        let payload = b"#!fake-zed-binary".as_slice();
        let archive = release_tar_gz("zed", payload);
        let extracted = extract_binary(&archive, "zed", false).unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn extract_binary_finds_zed_exe_inside_a_zip() {
        use std::io::Write as _;
        let payload = b"MZ-fake-windows-binary".as_slice();
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default();
            writer
                .start_file("zed-test-target/README.md", opts)
                .unwrap();
            writer.write_all(b"decoy docs").unwrap();
            writer.start_file("zed-test-target/zed.exe", opts).unwrap();
            writer.write_all(payload).unwrap();
            writer.finish().unwrap();
        }
        let extracted = extract_binary(&cursor.into_inner(), "zed.exe", true).unwrap();
        assert_eq!(extracted, payload);
    }

    #[test]
    fn extract_binary_rejects_an_archive_without_the_binary() {
        let archive = release_tar_gz("not-zed", b"wrong tool");
        let err = extract_binary(&archive, "zed", false).unwrap_err();
        assert!(
            err.to_string().contains("did not contain"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn replace_exe_swaps_contents_atomically_and_keeps_exec_bit() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("zed");
        std::fs::write(&exe, b"old-binary").unwrap();

        replace_exe(&exe, b"new-binary").unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
        // No staging temp file left behind next to the exe.
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the replaced exe remains"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "replaced binary must stay executable");
        }
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
