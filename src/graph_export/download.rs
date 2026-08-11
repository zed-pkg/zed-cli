use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use reqwest::redirect::Policy;

use super::coordinate::PackageCoordinate;
use super::format::{GraphFormat, RouteKind};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GRAPH_DIGEST_HEADER: &str = "x-zpkg-graph-digest";
const GRAPH_AUTHORITATIVE_HEADER: &str = "x-zpkg-graph-authoritative";

pub(super) struct DownloadRequest<'a> {
    pub(super) registry: &'a str,
    pub(super) token: Option<&'a str>,
    pub(super) coordinate: &'a PackageCoordinate,
    pub(super) format: GraphFormat,
    pub(super) etag: Option<&'a str>,
    pub(super) max_bytes: u64,
}

pub(super) struct DownloadedGraph {
    pub(super) body: Vec<u8>,
    pub(super) not_modified: bool,
    pub(super) authoritative: bool,
    pub(super) etag: Option<String>,
    pub(super) graph_digest: Option<String>,
    pub(super) content_type: Option<String>,
}

pub(super) fn download(options: DownloadRequest<'_>) -> Result<DownloadedGraph> {
    let url = graph_url(options.registry, options.coordinate, options.format)?;
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building dependency graph HTTP client")?;
    let mut request = client.get(url).header(ACCEPT, options.format.media_type());
    if let Some(token) = options.token.filter(|value| !value.is_empty()) {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(etag) = options.etag.filter(|value| !value.is_empty()) {
        request = request.header(IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .context("requesting immutable package dependency graph")?;
    consume_response(response, options.format, options.max_bytes)
}

pub(super) fn graph_url(
    base: &str,
    coordinate: &PackageCoordinate,
    format: GraphFormat,
) -> Result<Url> {
    let normalized = format!("{}/", base.trim_end_matches('/'));
    let mut url = Url::parse(&normalized)
        .with_context(|| format!("registry URL `{base}` is not a valid absolute URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "dependency graph downloads require an HTTP(S) registry URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "registry URL may not embed credentials; use --token or ZED_PKG_TOKEN"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "registry URL may not contain a query or fragment"
    );

    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("registry URL cannot be used as a base URL"))?;
        path.pop_if_empty();
        path.extend([
            "v1",
            "packages",
            &coordinate.org,
            &coordinate.name,
            "versions",
            &coordinate.version,
            "dependency-graph",
        ]);
        if format.route_kind() == RouteKind::Extended {
            path.extend(["export", format.name()]);
        }
    }

    if format.route_kind() == RouteKind::Canonical {
        url.query_pairs_mut()
            .append_pair("view", "declared")
            .append_pair("format", format.name());
    }
    Ok(url)
}

fn consume_response(
    response: Response,
    format: GraphFormat,
    max_bytes: u64,
) -> Result<DownloadedGraph> {
    let status = response.status();
    let etag = header_value(response.headers(), ETAG.as_str())?;
    let graph_digest = header_value(response.headers(), GRAPH_DIGEST_HEADER)?;
    let content_type = header_value(response.headers(), CONTENT_TYPE.as_str())?;
    let authoritative = match header_value(response.headers(), GRAPH_AUTHORITATIVE_HEADER)? {
        Some(value) if value.eq_ignore_ascii_case("true") => true,
        Some(value) if value.eq_ignore_ascii_case("false") => false,
        Some(value) => anyhow::bail!(
            "registry returned invalid {GRAPH_AUTHORITATIVE_HEADER} header `{value}`"
        ),
        None => format.authoritative(),
    };

    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(DownloadedGraph {
            body: Vec::new(),
            not_modified: true,
            authoritative,
            etag,
            graph_digest,
            content_type,
        });
    }
    ensure!(
        status == reqwest::StatusCode::OK,
        "dependency graph request failed with HTTP {status}"
    );
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        ensure!(
            length <= max_bytes,
            "dependency graph body exceeds the {max_bytes}-byte client limit"
        );
    }

    let limit = max_bytes
        .checked_add(1)
        .context("dependency graph byte limit overflow")?;
    let mut body = Vec::new();
    response
        .take(limit)
        .read_to_end(&mut body)
        .context("reading dependency graph response")?;
    ensure!(
        body.len() as u64 <= max_bytes,
        "dependency graph body exceeds the {max_bytes}-byte client limit"
    );
    Ok(DownloadedGraph {
        body,
        not_modified: false,
        authoritative,
        etag,
        graph_digest,
        content_type,
    })
}

fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .with_context(|| format!("registry returned non-ASCII `{name}` header"))
        })
        .transpose()
}

pub(super) fn write_body(
    output: Option<&Path>,
    format: GraphFormat,
    body: &[u8],
) -> Result<String> {
    match output {
        None => {
            ensure!(!format.binary(), "binary graph output requires --output");
            write_stdout(body)
        }
        Some(path) if path == Path::new("-") => write_stdout(body),
        Some(path) => write_atomic_file(path, body),
    }
}

fn write_stdout(body: &[u8]) -> Result<String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(body)
        .context("writing dependency graph to stdout")?;
    stdout.flush().context("flushing dependency graph stdout")?;
    Ok("stdout".to_string())
}

fn write_atomic_file(path: &Path, body: &[u8]) -> Result<String> {
    ensure!(
        !path.as_os_str().is_empty(),
        "dependency graph output path may not be empty"
    );
    match fs::symlink_metadata(path) {
        Ok(_) => anyhow::bail!(
            "dependency graph output already exists: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("checking dependency graph output {}", path.display()));
        }
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::metadata(parent)
        .with_context(|| format!("reading output directory {}", parent.display()))?;
    ensure!(
        metadata.is_dir(),
        "dependency graph output parent is not a directory: {}",
        parent.display()
    );

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating atomic output beside {}", path.display()))?;
    temporary
        .write_all(body)
        .with_context(|| format!("writing temporary graph output for {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("syncing temporary graph output for {}", path.display()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::anyhow!(
            "publishing dependency graph output {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinate() -> PackageCoordinate {
        PackageCoordinate::parse("acme/http-kit@2.0.0-beta.1+build.7").unwrap()
    }

    #[test]
    fn canonical_and_extended_routes_are_distinct_and_preserve_base_prefixes() {
        let canonical = graph_url(
            "https://registry.example/internal/",
            &coordinate(),
            GraphFormat::Yaml,
        )
        .unwrap();
        assert_eq!(
            canonical.path(),
            "/internal/v1/packages/acme/http-kit/versions/2.0.0-beta.1+build.7/dependency-graph"
        );
        assert_eq!(canonical.query(), Some("view=declared&format=yaml"));

        let binary = graph_url(
            "https://registry.example/internal/",
            &coordinate(),
            GraphFormat::Protobuf,
        )
        .unwrap();
        assert_eq!(
            binary.path(),
            "/internal/v1/packages/acme/http-kit/versions/2.0.0-beta.1+build.7/dependency-graph/export/protobuf"
        );
        assert!(binary.query().is_none());
    }

    #[test]
    fn registry_base_rejects_credentials_queries_and_non_http_schemes() {
        assert!(
            graph_url(
                "https://user:secret@registry.example",
                &coordinate(),
                GraphFormat::Json
            )
            .is_err()
        );
        assert!(
            graph_url(
                "https://registry.example?token=secret",
                &coordinate(),
                GraphFormat::Json
            )
            .is_err()
        );
        assert!(graph_url("file:///tmp/registry", &coordinate(), GraphFormat::Json).is_err());
    }

    #[test]
    fn atomic_output_refuses_existing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("graph.json");
        assert_eq!(
            write_atomic_file(&output, b"graph").unwrap(),
            output.display().to_string()
        );
        assert_eq!(fs::read(&output).unwrap(), b"graph");
        assert!(write_atomic_file(&output, b"replacement").is_err());
        assert_eq!(fs::read(&output).unwrap(), b"graph");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_output_refuses_dangling_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("graph.json");
        symlink(directory.path().join("missing"), &output).unwrap();
        assert!(write_atomic_file(&output, b"graph").is_err());
    }
}
