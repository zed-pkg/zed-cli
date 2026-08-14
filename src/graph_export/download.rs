use std::fs;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use reqwest::redirect::Policy;
use zed_interfaces::{DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER, DEPENDENCY_GRAPH_DIGEST_HEADER};

use super::coordinate::PackageCoordinate;
use super::format::{GraphFormat, RouteKind};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    let conditional_etag = options.etag.filter(|value| !value.is_empty());
    if let Some(etag) = conditional_etag {
        request = request.header(IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .context("requesting immutable package dependency graph")?;
    consume_response(
        response,
        options.format,
        options.max_bytes,
        conditional_etag,
    )
}

pub(super) fn graph_url(
    base: &str,
    coordinate: &PackageCoordinate,
    format: GraphFormat,
) -> Result<Url> {
    let normalized = format!("{}/", base.trim_end_matches('/'));
    let mut url = Url::parse(&normalized).context("registry URL is not a valid absolute URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "dependency graph downloads require an HTTP(S) registry URL"
    );
    ensure!(
        url.host_str().is_some(),
        "dependency graph downloads require a registry URL with a host"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "registry URL may not embed credentials; use --token or ZED_PKG_TOKEN"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "registry URL may not contain a query or fragment"
    );
    ensure!(
        url.scheme() == "https" || registry_host_is_loopback(&url),
        "dependency graph registry URLs must use HTTPS (HTTP is allowed only on loopback)"
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

fn registry_host_is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn consume_response(
    response: Response,
    format: GraphFormat,
    max_bytes: u64,
    conditional_etag: Option<&str>,
) -> Result<DownloadedGraph> {
    let status = response.status();
    ensure!(
        status == reqwest::StatusCode::OK || status == reqwest::StatusCode::NOT_MODIFIED,
        "dependency graph request failed with HTTP {status}"
    );

    let etag = require_strong_etag(header_value(response.headers(), ETAG.as_str())?)?;
    let graph_digest = require_graph_digest(header_value(
        response.headers(),
        DEPENDENCY_GRAPH_DIGEST_HEADER,
    )?)?;
    let authoritative = require_authoritative(
        header_value(response.headers(), DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER)?,
        format,
    )?;
    let content_length =
        require_content_length(header_value(response.headers(), CONTENT_LENGTH.as_str())?)?;

    if status == reqwest::StatusCode::NOT_MODIFIED {
        let condition =
            conditional_etag.context("registry returned 304 without an If-None-Match request")?;
        ensure!(
            if_none_match_matches(condition, &etag),
            "registry returned 304 with an ETag that does not match If-None-Match"
        );
        return Ok(DownloadedGraph {
            body: Vec::new(),
            not_modified: true,
            authoritative,
            etag: Some(etag),
            graph_digest: Some(graph_digest),
            content_type: None,
        });
    }
    let content_type = require_content_type(
        header_value(response.headers(), CONTENT_TYPE.as_str())?,
        format.media_type(),
    )?;
    ensure!(
        content_length <= max_bytes,
        "dependency graph body exceeds the {max_bytes}-byte client limit"
    );

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
    ensure_content_length_matches(content_length, body.len())?;
    Ok(DownloadedGraph {
        body,
        not_modified: false,
        authoritative,
        etag: Some(etag),
        graph_digest: Some(graph_digest),
        content_type: Some(content_type),
    })
}

/// GET uses weak validator comparison for If-None-Match. A 304 still has to
/// identify one of the validators sent by this client; accepting an unrelated
/// ETag would incorrectly bless stale cached graph bytes as current.
fn if_none_match_matches(condition: &str, etag: &str) -> bool {
    condition
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || weak_etag_opaque(candidate) == weak_etag_opaque(etag))
}

fn weak_etag_opaque(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix("W/").unwrap_or(value)
}

fn require_strong_etag(value: Option<String>) -> Result<String> {
    let value = value.context("registry response is missing a strong dependency graph ETag")?;
    let opaque = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'));
    ensure!(
        !value.starts_with("W/")
            && opaque.is_some_and(|opaque| {
                !opaque.is_empty()
                    && opaque
                        .bytes()
                        .all(|byte| byte == b'!' || (b'#'..=b'~').contains(&byte))
            }),
        "registry response is missing a valid strong dependency graph ETag"
    );
    Ok(value)
}

fn require_graph_digest(value: Option<String>) -> Result<String> {
    let value = value.context("registry response is missing the dependency graph digest")?;
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    ensure!(
        valid,
        "registry response carries an invalid dependency graph digest"
    );
    Ok(value)
}

fn require_authoritative(value: Option<String>, format: GraphFormat) -> Result<bool> {
    let value = value.with_context(|| {
        format!(
            "registry response is missing required {DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER} header for requested `{}` format",
            format.name()
        )
    })?;
    let authoritative = if value.eq_ignore_ascii_case("true") {
        true
    } else if value.eq_ignore_ascii_case("false") {
        false
    } else {
        anyhow::bail!(
            "registry returned an invalid {DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER} header for requested `{}` format; expected `true` or `false`",
            format.name()
        );
    };
    ensure!(
        authoritative == format.authoritative(),
        "registry response {DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER} header does not match requested `{}` format; expected `{}`",
        format.name(),
        format.authoritative()
    );
    Ok(authoritative)
}

fn require_content_length(value: Option<String>) -> Result<u64> {
    value
        .context("registry response is missing dependency graph Content-Length")?
        .parse::<u64>()
        .context("registry returned an invalid dependency graph Content-Length")
}

fn ensure_content_length_matches(content_length: u64, body_length: usize) -> Result<()> {
    ensure!(
        body_length as u64 == content_length,
        "registry response body length does not match dependency graph Content-Length"
    );
    Ok(())
}

fn require_content_type(value: Option<String>, expected: &str) -> Result<String> {
    let value = value.context("registry response is missing dependency graph Content-Type")?;
    ensure!(
        bare_content_type(&value).eq_ignore_ascii_case(bare_content_type(expected)),
        "registry response Content-Type does not match the requested dependency graph format"
    );
    Ok(value)
}

fn bare_content_type(value: &str) -> &str {
    value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim()
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
        Ok(_) => anyhow::bail!("dependency graph output already exists: {}", path.display()),
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

    fn graph_format(name: &str) -> GraphFormat {
        GraphFormat::parse(name).unwrap()
    }

    #[test]
    fn canonical_and_extended_routes_are_distinct_and_preserve_base_prefixes() {
        let canonical = graph_url(
            "https://registry.example/internal/",
            &coordinate(),
            graph_format("yaml"),
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
            graph_format("protobuf"),
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
                graph_format("json")
            )
            .is_err()
        );
        assert!(
            graph_url(
                "https://registry.example?token=secret",
                &coordinate(),
                graph_format("json")
            )
            .is_err()
        );
        assert!(graph_url("file:///tmp/registry", &coordinate(), graph_format("json")).is_err());
    }

    #[test]
    fn registry_base_requires_https_except_for_explicit_loopback_hosts() {
        assert!(
            graph_url(
                "http://registry.example",
                &coordinate(),
                graph_format("json")
            )
            .is_err()
        );
        assert!(graph_url("http://localhost:8080", &coordinate(), graph_format("json")).is_ok());
        assert!(graph_url("http://127.0.0.1", &coordinate(), graph_format("json")).is_ok());
        assert!(graph_url("http://[::1]", &coordinate(), graph_format("json")).is_ok());
        assert!(graph_url("http://127.0.0.2", &coordinate(), graph_format("json")).is_ok());
        assert!(
            graph_url(
                "http://127.0.0.1.example",
                &coordinate(),
                graph_format("json")
            )
            .is_err()
        );
    }

    #[test]
    fn response_contract_requires_strong_validators_and_requested_media_type() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            require_strong_etag(Some("\"bytes\"".into())).unwrap(),
            "\"bytes\""
        );
        assert!(require_strong_etag(Some("W/\"bytes\"".into())).is_err());
        assert!(require_strong_etag(Some("\"has space\"".into())).is_err());
        assert!(require_strong_etag(Some("\"has\ttab\"".into())).is_err());
        assert!(require_strong_etag(None).is_err());
        assert_eq!(require_graph_digest(Some(digest.clone())).unwrap(), digest);
        assert!(require_graph_digest(Some("sha256:abc".into())).is_err());
        assert_eq!(
            require_content_type(
                Some("text/csv; charset=utf-8".into()),
                "text/csv; charset=utf-8"
            )
            .unwrap(),
            "text/csv; charset=utf-8"
        );
        assert!(
            require_content_type(
                Some("text/html; charset=utf-8".into()),
                graph_format("json").media_type()
            )
            .is_err()
        );
        assert_eq!(require_content_length(Some("42".into())).unwrap(), 42);
        assert!(require_content_length(Some("-1".into())).is_err());
        assert!(require_content_length(None).is_err());
        ensure_content_length_matches(42, 42).unwrap();
        assert!(ensure_content_length_matches(42, 41).is_err());
    }

    #[test]
    fn response_authority_is_required_and_must_match_the_requested_format() {
        for format in [
            "json", "yaml", "toml", "json5", "xml", "msgpack", "protobuf",
        ] {
            assert!(require_authoritative(Some("true".into()), graph_format(format)).unwrap());
            let error = require_authoritative(Some("false".into()), graph_format(format))
                .unwrap_err()
                .to_string();
            assert!(error.contains("does not match"), "{error}");
            assert!(error.contains(format), "{error}");
        }

        for format in ["dot", "mermaid", "csv"] {
            assert!(!require_authoritative(Some("false".into()), graph_format(format)).unwrap());
            let error = require_authoritative(Some("true".into()), graph_format(format))
                .unwrap_err()
                .to_string();
            assert!(error.contains("does not match"), "{error}");
            assert!(error.contains(format), "{error}");
        }

        let missing = require_authoritative(None, graph_format("json"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains(DEPENDENCY_GRAPH_AUTHORITATIVE_HEADER));
        assert!(missing.contains("requested `json` format"));

        let invalid = require_authoritative(Some("yes".into()), graph_format("json"))
            .unwrap_err()
            .to_string();
        assert!(invalid.contains("expected `true` or `false`"));
    }

    #[test]
    fn conditional_validators_use_weak_comparison_but_reject_unrelated_304s() {
        assert!(if_none_match_matches("\"current\"", "\"current\""));
        assert!(if_none_match_matches("W/\"current\"", "\"current\""));
        assert!(if_none_match_matches(
            "\"old\", W/\"current\"",
            "\"current\""
        ));
        assert!(if_none_match_matches("*", "\"current\""));
        assert!(!if_none_match_matches("\"other\"", "\"current\""));
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
