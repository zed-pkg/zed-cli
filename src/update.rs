//! `zed update self` (zed-docs issue #9): check GitHub Releases for a newer
//! `zed`, download the artifact matching this platform, and replace the
//! running binary in place. Pairs with the cross-platform release matrix
//! (`release.yml`) that publishes `zed-<target>.{tar.gz,zip}` assets.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::is_sha256_hex;

/// The CLI's own source repository, where releases are published.
const REPO: &str = "zed-pkg/zed-cli";
const MAX_CHECKSUM_BYTES: u64 = 16 * 1024;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TAR_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;

fn read_bounded(reader: impl Read, limit: u64, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{label} exceeds the {limit}-byte limit");
    }
    Ok(bytes)
}

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
    let url = reqwest::Url::parse(url).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let tag = url.path().strip_prefix(&format!("/{REPO}/releases/tag/"))?;
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    Some(tag.to_owned())
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
    let mut matches = sums.lines().filter_map(|line| {
        let (hex, name) = line.split_once(' ')?;
        let name = name.strip_prefix(' ').or_else(|| name.strip_prefix('*'))?;
        (name == filename).then_some(hex)
    });
    let digest = matches.next()?.to_ascii_lowercase();
    (matches.next().is_none() && is_sha256_hex(&digest)).then_some(digest)
}

/// Verify a downloaded release asset against its published `.sha256` sidecar
/// before anything is extracted or installed. A corrupted download
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
    let sums_url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}.sha256");
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
    let sums = String::from_utf8(read_bounded(resp, MAX_CHECKSUM_BYTES, "checksum file")?)
        .context("checksum file is not UTF-8")?;
    let expected = expected_sha256_for(&sums, asset).with_context(|| {
        format!("release checksum must contain exactly one valid entry for {asset}; refusing to self-update")
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
    let selected = if is_zip {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        if archive.len() > MAX_ARCHIVE_ENTRIES {
            bail!("release archive has too many entries");
        }
        (0..archive.len()).try_fold(None, |selected, i| -> Result<Option<Vec<u8>>> {
            let file = archive.by_index(i)?;
            if file.name().rsplit('/').next() != Some(bin_name) {
                return Ok(selected);
            }
            let mode = file.unix_mode().unwrap_or(0) & 0o170000;
            if !file.is_file() || !matches!(mode, 0 | 0o100000) {
                bail!("release executable must be a regular file");
            }
            select_binary(selected, file.size(), file)
        })?
    } else {
        let unpacked = read_bounded(
            flate2::read::GzDecoder::new(Cursor::new(bytes)),
            MAX_TAR_BYTES,
            "expanded release archive",
        )?;
        let mut archive = tar::Archive::new(Cursor::new(unpacked));
        archive.entries()?.enumerate().try_fold(
            None,
            |selected, (index, entry)| -> Result<Option<Vec<u8>>> {
                if index >= MAX_ARCHIVE_ENTRIES {
                    bail!("release archive has too many entries");
                }
                let entry = entry?;
                if entry.path()?.file_name() != Some(std::ffi::OsStr::new(bin_name)) {
                    return Ok(selected);
                }
                if !entry.header().entry_type().is_file() {
                    bail!("release executable must be a regular file");
                }
                select_binary(selected, entry.size(), entry)
            },
        )?
    };
    selected.with_context(|| format!("release archive did not contain a `{bin_name}` binary"))
}

fn select_binary(
    selected: Option<Vec<u8>>,
    size: u64,
    reader: impl Read,
) -> Result<Option<Vec<u8>>> {
    if selected.is_some() {
        bail!("release archive contains duplicate executable entries");
    }
    if size == 0 || size > MAX_ASSET_BYTES {
        bail!("release executable size is outside the supported bounds");
    }
    let bytes = read_bounded(reader, size, "release executable")?;
    if bytes.len() as u64 != size {
        bail!("release executable size does not match its archive header");
    }
    Ok(Some(bytes))
}

/// Atomically replace the executable at `exe` with `new_bytes`.
fn replace_exe(exe: &Path, new_bytes: &[u8]) -> Result<()> {
    if !std::fs::symlink_metadata(exe)?.is_file() {
        bail!("update destination must be a regular executable file");
    }
    let dir = exe.parent().context("executable has no parent directory")?;
    let tmp = tempfile::NamedTempFile::new_in(dir).context("staging update beside executable")?;
    tmp.as_file()
        .write_all(new_bytes)
        .context("writing staged update")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    tmp.as_file().sync_all().context("syncing staged update")?;
    // On Unix, renaming over the running binary is safe: the running process
    // keeps its open inode. On Windows the running image is locked, so move it
    // aside first.
    #[cfg(not(windows))]
    tmp.persist(exe)
        .with_context(|| format!("replacing {}", exe.display()))?;
    #[cfg(windows)]
    {
        // Keep the backup directory on rollback failure; never let a cleanup
        // destructor delete the last working executable.
        let backup = tempfile::Builder::new()
            .prefix(".zed-update-")
            .tempdir_in(dir)?
            .keep();
        let old = backup.join("zed.exe");
        std::fs::rename(exe, &old).context("backing up running executable")?;
        if let Err(error) = tmp.persist(exe) {
            std::fs::rename(&old, exe).with_context(|| {
                format!(
                    "update failed ({error}); rollback failed; original remains at {}",
                    old.display()
                )
            })?;
            let _ = std::fs::remove_dir(&backup);
            return Err(error).context("replacing executable; original restored");
        }
        // Windows may hold the renamed running image open until process exit.
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_dir(&backup);
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
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let latest_url = format!("https://github.com/{REPO}/releases/latest");
    let resp = client
        .get(&latest_url)
        .send()
        .and_then(|r| r.error_for_status())
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
    let response = client
        .get(&download_url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading release asset {asset}"))?;
    let bytes = read_bounded(response, MAX_ASSET_BYTES, "release asset")?;

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
mod tests;
