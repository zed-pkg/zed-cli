use std::fs::{self, File};
use std::io::Cursor;
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
    let metadata: VersionMetadata =
        get_json(client, registry, &version_path(&org, &name, &version))?;
    let archive_url = absolute_url(registry, &metadata.download_url);
    let archive = get_bytes(client, &archive_url)?;
    verify_artifact(&metadata, &archive)?;

    let format = metadata.format.to_string();
    let destination = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_destination(&name, &version, &format, unpack));

    if unpack {
        unpack_artifact(&archive, &format, &destination)?;
    } else {
        fs::write(&destination, &archive)
            .with_context(|| format!("write {}", destination.display()))?;
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
        if org.is_empty() || name.is_empty() || name.contains('/') {
            bail!("package coordinate must be ORG/NAME");
        }
        if let Some(requested_org) = organization {
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
    if package.is_empty() || org.is_empty() {
        bail!("organization and package name must not be empty");
    }
    Ok((org.to_string(), package.to_string()))
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
    let body = response
        .text()
        .with_context(|| format!("read response body from {url}"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("decode JSON from {url}"))
}

fn get_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("GET {url} returned {status}: {body}");
    }
    Ok(response
        .bytes()
        .with_context(|| format!("read artifact from {url}"))?
        .to_vec())
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

fn unpack_artifact(bytes: &[u8], format: &str, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| format!("create {}", destination.display()))?;
    match format {
        "tar.gz" => unpack_tar_gz(bytes, destination),
        "zip" => unpack_zip(bytes, destination),
        other => bail!("unsupported artifact format for --unpack: {other}"),
    }
}

fn unpack_tar_gz(bytes: &[u8], destination: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
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
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("read zip entry")?;
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
}
