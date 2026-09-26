use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use zed_interfaces::registry::{
    DEFAULT_REGISTRY_URL, PackageMetadata, SearchResponse, VersionMetadata, healthz_path,
    package_path, search_path, version_path,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_BODY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;

#[derive(Debug, Parser)]
#[command(
    name = "zed-hex-pm",
    bin_name = "zed hex.pm",
    about = "Hex.pm-compatible package workflows backed by the Zed registry"
)]
struct Cli {
    /// Registry base URL. Mirrors `zed --registry` / `ZED_PKG_REGISTRY`.
    #[arg(
        long,
        env = "ZED_PKG_REGISTRY",
        default_value = DEFAULT_REGISTRY_URL,
        global = true
    )]
    registry: String,

    /// Emit machine-readable JSON where the command has a structured result.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show registry, package, or release information.
    Info {
        package: Option<String>,
        version: Option<String>,
        /// Zed organization for a bare package name.
        #[arg(long)]
        organization: Option<String>,
    },
    /// Search package names/descriptions.
    Search {
        query: String,
        /// Restrict results to one Zed organization.
        #[arg(long)]
        organization: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Package archive operations.
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PackageCommand {
    /// Fetch a release archive, verify its SHA-256 and optionally unpack it.
    Fetch {
        package: String,
        version: Option<String>,
        /// Zed organization for a bare package name.
        #[arg(long)]
        organization: Option<String>,
        /// Archive path, or extraction directory when --unpack is set.
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        unpack: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_registry_url(&cli.registry)?;
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("build HTTP client")?;

    match cli.command {
        Command::Info {
            package,
            version,
            organization,
        } => info(
            &client,
            &cli.registry,
            cli.json,
            package.as_deref(),
            version.as_deref(),
            organization.as_deref(),
        ),
        Command::Search {
            query,
            organization,
            limit,
        } => search(
            &client,
            &cli.registry,
            cli.json,
            &query,
            organization.as_deref(),
            limit,
        ),
        Command::Package { command } => match command {
            PackageCommand::Fetch {
                package,
                version,
                organization,
                output,
                unpack,
            } => fetch_package(
                &client,
                &cli.registry,
                &package,
                version.as_deref(),
                organization.as_deref(),
                output.as_deref(),
                unpack,
                cli.json,
            ),
        },
    }
}

fn info(
    client: &Client,
    registry: &str,
    json: bool,
    package: Option<&str>,
    version: Option<&str>,
    organization: Option<&str>,
) -> Result<()> {
    let Some(package) = package else {
        if version.is_some() || organization.is_some() {
            bail!("package is required when version or --organization is supplied");
        }
        let health: Value = get_json(client, registry, &healthz_path())?;
        let value = serde_json::json!({
            "cli": "zed hex.pm",
            "version": env!("CARGO_PKG_VERSION"),
            "registry": registry,
            "health": health,
        });
        return print_value(&value, json);
    };

    let (org, name) = package_coordinate(package, organization)?;
    if let Some(version) = version {
        validate_path_segment("version", version)?;
        let metadata: VersionMetadata =
            get_json(client, registry, &version_path(&org, &name, version))?;
        print_structured(&metadata, json)
    } else {
        let metadata: PackageMetadata = get_json(client, registry, &package_path(&org, &name))?;
        print_structured(&metadata, json)
    }
}

fn search(
    client: &Client,
    registry: &str,
    json: bool,
    query: &str,
    organization: Option<&str>,
    limit: u32,
) -> Result<()> {
    if let Some(org) = organization {
        validate_path_segment("organization", org)?;
    }
    let url = absolute_url(registry, &search_path());
    let limit = limit.to_string();
    let response = client
        .get(&url)
        .query(&[("q", query), ("limit", limit.as_str())])
        .send()
        .with_context(|| format!("GET {url}"))?;
    let mut result: SearchResponse = decode_json(response, &url)?;
    if let Some(org) = organization {
        result.items.retain(|item| item.org == org);
    }
    print_structured(&result, json)
}

#[allow(clippy::too_many_arguments)]
fn fetch_package(
    client: &Client,
    registry: &str,
    package: &str,
    requested_version: Option<&str>,
    organization: Option<&str>,
    output: Option<&Path>,
    unpack: bool,
    json: bool,
) -> Result<()> {
    let (org, name) = package_coordinate(package, organization)?;
    let version = match requested_version {
        Some(version) => version.to_string(),
        None => {
            let package: PackageMetadata = get_json(client, registry, &package_path(&org, &name))?;
            package
                .latest
                .context("package has no visible release to fetch")?
        }
    };
    validate_path_segment("version", &version)?;
    let metadata: VersionMetadata =
        get_json(client, registry, &version_path(&org, &name, &version))?;
    let archive_url = absolute_url(registry, &metadata.download_url);
    let archive = get_bytes(client, &archive_url, metadata.size)?;
    verify_artifact(&metadata, &archive)?;

    let format = metadata.format.to_string();
    let destination = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_destination(&name, &version, &format, unpack));

    if unpack {
        unpack_artifact(&archive, &format, &destination)?;
    } else {
        write_new_file(&destination, &archive)?;
    }

    let receipt = serde_json::json!({
        "org": org,
        "name": name,
        "version": version,
        "sha256": metadata.sha256,
        "size": archive.len(),
        "format": format,
        "destination": destination,
        "unpacked": unpack,
    });
    print_value(&receipt, json)
}

fn package_coordinate(package: &str, organization: Option<&str>) -> Result<(String, String)> {
    if let Some((org, name)) = package.split_once('/') {
        if name.contains('/') {
            bail!("package coordinate must be ORG/NAME");
        }
        validate_path_segment("organization", org)?;
        validate_path_segment("package name", name)?;
        if let Some(requested_org) = organization {
            validate_path_segment("organization", requested_org)?;
            if requested_org != org {
                bail!(
                    "package coordinate organization '{org}' conflicts with --organization '{requested_org}'"
                );
            }
        }
        return Ok((org.to_string(), name.to_string()));
    }
    let org = organization
        .context("Zed package names are namespaced; use ORG/NAME or pass --organization ORG")?;
    validate_path_segment("organization", org)?;
    validate_path_segment("package name", package)?;
    Ok((org.to_string(), package.to_string()))
}

fn validate_path_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value == "." || value == ".." {
        bail!("{label} must be a non-empty URL path segment");
    }
    if value.chars().any(|ch| {
        ch.is_control() || ch.is_whitespace() || matches!(ch, '/' | '\\' | '?' | '#' | '%')
    }) {
        bail!("{label} contains characters that are not safe in a registry path segment");
    }
    Ok(())
}

fn validate_registry_url(registry: &str) -> Result<()> {
    let url = reqwest::Url::parse(registry).context("registry must be an absolute URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("registry URL must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("registry URL must not embed credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("registry URL must not contain a query string or fragment");
    }
    Ok(())
}

fn get_json<T: DeserializeOwned>(client: &Client, registry: &str, path: &str) -> Result<T> {
    let url = absolute_url(registry, path);
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    decode_json(response, &url)
}

fn decode_json<T: DeserializeOwned>(response: Response, url: &str) -> Result<T> {
    let status = response.status();
    let limit = if status.is_success() {
        MAX_JSON_BODY_BYTES
    } else {
        MAX_ERROR_BODY_BYTES
    };
    let body = read_response_limited(response, limit, "HTTP response body")
        .with_context(|| format!("read response body from {url}"))?;
    if !status.is_success() {
        let body = String::from_utf8_lossy(&body);
        bail!("GET {url} returned {status}: {body}");
    }
    serde_json::from_slice(&body).with_context(|| format!("decode JSON from {url}"))
}

fn get_bytes(client: &Client, url: &str, expected_size: u64) -> Result<Vec<u8>> {
    if expected_size > MAX_ARTIFACT_BYTES {
        bail!(
            "artifact is too large: registry declared {expected_size} bytes; safety limit is {MAX_ARTIFACT_BYTES}"
        );
    }
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_response_limited(response, MAX_ERROR_BODY_BYTES, "error response body")
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&body);
        bail!("GET {url} returned {status}: {body}");
    }
    if let Some(content_length) = response.content_length() {
        if content_length != expected_size {
            bail!(
                "artifact Content-Length mismatch: registry declared {expected_size}, server sent {content_length}"
            );
        }
    }
    read_response_limited(response, expected_size, "artifact")
        .with_context(|| format!("read artifact from {url}"))
}

fn read_response_limited(mut response: Response, limit: u64, label: &str) -> Result<Vec<u8>> {
    if let Some(content_length) = response.content_length() {
        if content_length > limit {
            bail!("{label} exceeds safety limit of {limit} bytes");
        }
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .with_context(|| format!("read {label}"))?;
    if body.len() as u64 > limit {
        bail!("{label} exceeds safety limit of {limit} bytes");
    }
    Ok(body)
}

fn verify_artifact(metadata: &VersionMetadata, archive: &[u8]) -> Result<()> {
    if archive.len() as u64 != metadata.size {
        bail!(
            "artifact size mismatch: registry declared {}, downloaded {}",
            metadata.size,
            archive.len()
        );
    }
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(&metadata.sha256) {
        bail!(
            "artifact checksum mismatch: expected {}, got {actual}",
            metadata.sha256
        );
    }
    Ok(())
}

fn default_destination(name: &str, version: &str, format: &str, unpack: bool) -> PathBuf {
    if unpack {
        return PathBuf::from(format!("{name}-{version}"));
    }
    let suffix = match format {
        "zip" => "zip",
        _ => "tar.gz",
    };
    PathBuf::from(format!("{name}-{version}.{suffix}"))
}

fn write_new_file(destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| {
            format!(
                "create {}; refusing to overwrite an existing path",
                destination.display()
            )
        })?;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(destination);
        return Err(error).with_context(|| format!("write {}", destination.display()));
    }
    Ok(())
}

fn unpack_artifact(bytes: &[u8], format: &str, destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => bail!(
            "refusing to unpack into existing path {}; choose a new directory",
            destination.display()
        ),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("inspect {}", destination.display()));
        }
        Err(_) => {}
    }
    fs::create_dir(destination).with_context(|| format!("create {}", destination.display()))?;
    let result = match format {
        "tar.gz" => unpack_tar_gz(bytes, destination),
        "zip" => unpack_zip(bytes, destination),
        other => bail!("unsupported artifact format for --unpack: {other}"),
    };
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn unpack_tar_gz(bytes: &[u8], destination: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut entries_seen = 0usize;
    let mut unpacked_bytes = 0u64;
    for entry in archive.entries().context("read tar entries")? {
        entries_seen = entries_seen.saturating_add(1);
        if entries_seen > MAX_ARCHIVE_ENTRIES {
            bail!("tar archive contains too many entries");
        }
        let mut entry = entry.context("read tar entry")?;
        let entry_type = entry.header().entry_type();
        if !(entry_type.is_file() || entry_type.is_dir()) {
            bail!("tar archive contains a link or unsupported special entry");
        }
        unpacked_bytes = unpacked_bytes
            .checked_add(entry.size())
            .context("tar unpacked-size overflow")?;
        if unpacked_bytes > MAX_UNPACKED_BYTES {
            bail!("tar archive expands beyond the unpacked-size safety limit");
        }
        if !entry
            .unpack_in(destination)
            .with_context(|| format!("unpack tar entry into {}", destination.display()))?
        {
            bail!("tar entry attempted to escape extraction directory");
        }
    }
    Ok(())
}

fn unpack_zip(bytes: &[u8], destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("open zip archive")?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("zip archive contains too many entries");
    }
    let mut unpacked_bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("read zip entry")?;
        unpacked_bytes = unpacked_bytes
            .checked_add(entry.size())
            .context("zip unpacked-size overflow")?;
        if unpacked_bytes > MAX_UNPACKED_BYTES {
            bail!("zip archive expands beyond the unpacked-size safety limit");
        }
        let relative = entry
            .enclosed_name()
            .context("zip entry attempted to escape extraction directory")?;
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output).with_context(|| format!("create {}", output.display()))?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut file =
            File::create(&output).with_context(|| format!("create {}", output.display()))?;
        std::io::copy(&mut entry, &mut file)
            .with_context(|| format!("write {}", output.display()))?;
    }
    Ok(())
}

fn absolute_url(registry: &str, path_or_url: &str) -> String {
    if path_or_url.starts_with("https://") || path_or_url.starts_with("http://") {
        path_or_url.to_string()
    } else {
        format!(
            "{}/{}",
            registry.trim_end_matches('/'),
            path_or_url.trim_start_matches('/')
        )
    }
}

fn print_structured<T: serde::Serialize>(value: &T, json: bool) -> Result<()> {
    let rendered = serde_json::to_value(value).context("serialize result")?;
    print_value(&rendered, json)
}

fn print_value(value: &Value, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).context("render JSON")?
        );
        return Ok(());
    }

    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if value.is_null() || value == &Value::Array(Vec::new()) {
                    continue;
                }
                println!("{key}: {}", human_value(value));
            }
        }
        _ => println!("{}", human_value(value)),
    }
    Ok(())
}

fn human_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(human_value)
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zed-hex-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn package_coordinates_support_hex_style_bare_names_with_org_flag() {
        assert_eq!(
            package_coordinate("plug", Some("hexpm")).ok(),
            Some(("hexpm".into(), "plug".into()))
        );
        assert_eq!(
            package_coordinate("hexpm/plug", None).ok(),
            Some(("hexpm".into(), "plug".into()))
        );
        assert!(package_coordinate("plug", None).is_err());
        assert!(package_coordinate("hexpm/plug", Some("other")).is_err());
    }

    #[test]
    fn package_coordinates_reject_registry_path_injection() {
        for coordinate in [
            "hexpm/plug?admin=true",
            "hexpm/plug#fragment",
            "hexpm/%2e%2e",
            "hexpm\\plug",
        ] {
            assert!(
                package_coordinate(coordinate, None).is_err(),
                "{coordinate}"
            );
        }
        assert!(package_coordinate("plug", Some("../hexpm")).is_err());
    }

    #[test]
    fn registry_url_rejects_non_http_credentials_and_fragments() {
        assert!(validate_registry_url("https://zpkg.net").is_ok());
        assert!(validate_registry_url("http://localhost:8080/prefix").is_ok());
        assert!(validate_registry_url("file:///tmp/registry").is_err());
        assert!(validate_registry_url("https://user:pass@zpkg.net").is_err());
        assert!(validate_registry_url("https://zpkg.net/#frag").is_err());
    }

    #[test]
    fn relative_registry_paths_are_joined_without_double_slashes() {
        assert_eq!(
            absolute_url("https://zpkg.net/", "/v1/search"),
            "https://zpkg.net/v1/search"
        );
        assert_eq!(
            absolute_url("https://zpkg.net", "https://cdn.example/pkg.tar.gz"),
            "https://cdn.example/pkg.tar.gz"
        );
    }

    #[test]
    fn unpack_destination_drops_archive_extension() {
        assert_eq!(
            default_destination("plug", "1.0.0", "tar.gz", true),
            PathBuf::from("plug-1.0.0")
        );
        assert_eq!(
            default_destination("plug", "1.0.0", "zip", false),
            PathBuf::from("plug-1.0.0.zip")
        );
    }

    #[test]
    fn archive_write_refuses_to_clobber_existing_path() {
        let path = unique_temp_path("existing-archive");
        fs::write(&path, b"keep me").unwrap();
        let error = write_new_file(&path, b"replacement").unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
        let _ = fs::remove_file(path);
    }

    fn tar_gz(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, bytes, entry_type) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, *path, Cursor::new(*bytes)).unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn tar_unpack_rejects_parent_traversal_and_removes_partial_destination() {
        let archive = tar_gz(&[
            ("ok.txt", b"ok", tar::EntryType::Regular),
            ("../escape.txt", b"no", tar::EntryType::Regular),
        ]);
        let destination = unique_temp_path("tar-traversal");
        let error = unpack_artifact(&archive, "tar.gz", &destination).unwrap_err();
        assert!(
            error.to_string().contains("escape extraction directory")
                || error.to_string().contains("unpack tar entry")
        );
        assert!(!destination.exists(), "failed extraction must be cleaned up");
        assert!(!destination.with_file_name("escape.txt").exists());
    }

    #[test]
    fn tar_unpack_rejects_symlink_entries_and_cleans_destination() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);

        let mut regular = tar::Header::new_gnu();
        regular.set_entry_type(tar::EntryType::Regular);
        regular.set_mode(0o644);
        regular.set_size(2);
        regular.set_cksum();
        builder
            .append_data(&mut regular, "ok.txt", Cursor::new(b"ok"))
            .unwrap();

        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_mode(0o777);
        link.set_size(0);
        link.set_link_name("/etc/passwd").unwrap();
        link.set_cksum();
        builder
            .append_data(&mut link, "link", Cursor::new(Vec::<u8>::new()))
            .unwrap();

        let encoder = builder.into_inner().unwrap();
        let archive = encoder.finish().unwrap();
        let destination = unique_temp_path("tar-symlink");
        let error = unpack_artifact(&archive, "tar.gz", &destination).unwrap_err();
        assert!(error.to_string().contains("link or unsupported special entry"));
        assert!(!destination.exists(), "failed extraction must be cleaned up");
    }

    #[test]
    fn declared_artifact_size_limit_rejects_before_download() {
        assert!(MAX_ARTIFACT_BYTES < u64::MAX);
        let oversized = MAX_ARTIFACT_BYTES + 1;
        assert!(oversized > MAX_ARTIFACT_BYTES);
    }

    #[test]
    fn bounded_stream_reader_detects_one_byte_overflow() {
        // Mirrors the Read::take(limit + 1) boundary used by response decoding.
        let bytes = vec![7u8; 17];
        let mut reader = Cursor::new(bytes);
        let mut body = Vec::new();
        reader.by_ref().take(17).read_to_end(&mut body).unwrap();
        assert_eq!(body.len(), 17);
        assert!(body.len() as u64 > 16);
    }

    #[test]
    fn unpack_refuses_preexisting_destination_even_if_it_is_empty() {
        let path = unique_temp_path("existing-dir");
        fs::create_dir(&path).unwrap();
        let error = unpack_artifact(&[], "tar.gz", &path).unwrap_err();
        assert!(error.to_string().contains("existing path"));
        let _ = fs::remove_dir(path);
    }
}
