//! Talking to language-ecosystem registries directly, over their own HTTP
//! APIs.
//!
//! `preflight.rs` drives package-manager binaries — `npm pack`, `cargo
//! package`, `dart pub publish --dry-run`. That is the right tool for
//! *validating* a package, because those commands encode rules no third party
//! should reimplement. It is the wrong tool for *reaching a registry*: it makes
//! every publish target a toolchain zed must install, pin, and keep on `PATH`,
//! and it caps zed at the subset of ecosystems whose CLI happens to be present.
//! A Haskell client cannot be published from a runner that has no GHC.
//!
//! So this module speaks to the registries instead. Every host in
//! [`NativeHost`] exposes an HTTP API — that is how its own CLI reaches it —
//! and those APIs are stable, documented, and toolchain-free. What zed needs
//! from them is narrow: list the versions of a package, fetch one, and upload
//! an artifact that has already been built and validated.
//!
//! ## Requests are built, then sent
//!
//! [`RegistryRequest`] is a complete description of one HTTP call that has not
//! happened yet. Construction is pure and total, so the whole routing surface
//! is testable without a network, and `--dry-run` prints exactly what a real
//! run would send. Credentials are carried as [`HeaderValue::Secret`] and
//! redacted on the way out, including the two hosts that put the token in the
//! URL rather than a header.
//!
//! ## What dispatches on what
//!
//! Publishing dispatches on [`RegistryProtocol`], because the upload shape is
//! genuinely shared — Clojars and an Artifactory Maven repository take the same
//! `PUT`, and PyPI and TestPyPI are one protocol at two hosts. Version
//! discovery dispatches on protocol too, except for
//! [`RegistryProtocol::VcsIndexed`], which groups hosts that agree on *how to
//! publish* (push a tag) while disagreeing on how they serve an index; those
//! fall through to a per-host arm.
//!
//! Hosts whose publish flow is more than one request — pub.dev's signed-upload
//! handshake, the Maven Central Portal's bundle-then-poll — return
//! [`NativeHostClientError::MultiStepPublish`] naming the missing step, rather
//! than a request that would half-work.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zed_interfaces::native_host::{ChannelRoute, NativeHost, RegistryAuth, RegistryProtocol};

/// The HTTP verb a registry call uses. Narrow on purpose: a registry client
/// that can `DELETE` is a registry client that can unpublish by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Put => "PUT",
            Method::Post => "POST",
        }
    }
}

/// A header value that knows whether it is a secret.
///
/// The distinction exists so `--dry-run` output, error messages, and logs can
/// print a request verbatim without leaking a publish token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderValue {
    Literal(String),
    Secret(String),
}

impl HeaderValue {
    /// The value to actually send.
    pub fn expose(&self) -> &str {
        match self {
            HeaderValue::Literal(value) | HeaderValue::Secret(value) => value,
        }
    }

    fn is_secret(&self) -> bool {
        matches!(self, HeaderValue::Secret(_))
    }
}

impl fmt::Display for HeaderValue {
    /// Renders secrets as `<redacted>`. This is the only `Display` impl, so a
    /// caller cannot accidentally print the real value by reaching for the
    /// obvious formatting.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderValue::Literal(value) => f.write_str(value),
            HeaderValue::Secret(_) => f.write_str("<redacted>"),
        }
    }
}

/// What travels in the request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    Empty,
    /// A JSON document built by zed (npm's packument-shaped publish).
    Json(String),
    /// The artifact's raw bytes, with a media type (RubyGems, Hex, Maven).
    File {
        path: PathBuf,
        content_type: &'static str,
    },
    /// A `multipart/form-data` upload; `fields` are literal text parts sent
    /// alongside the file (PyPI's `:action`, Hackage's package field).
    Multipart {
        path: PathBuf,
        file_field: &'static str,
        fields: Vec<(&'static str, String)>,
    },
    /// crates.io's framed body: 4-byte little-endian metadata length, the
    /// metadata JSON, 4-byte little-endian crate length, then the `.crate`
    /// bytes. Not multipart and not raw, so it needs its own variant.
    CargoFramed {
        path: PathBuf,
        metadata: String,
    },
}

/// One fully-described HTTP call that has not been made yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRequest {
    pub method: Method,
    pub url: String,
    /// True when the token rides in `url` rather than a header, so `url` must
    /// be redacted before it is printed. LuaRocks puts the key in the path and
    /// Packagist in the query string.
    pub url_contains_secret: bool,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: RequestBody,
}

impl RegistryRequest {
    fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            url_contains_secret: false,
            headers: Vec::new(),
            body: RequestBody::Empty,
        }
    }

    fn header(mut self, name: &str, value: HeaderValue) -> Self {
        self.headers.push((name.to_string(), value));
        self
    }

    fn body(mut self, body: RequestBody) -> Self {
        self.body = body;
        self
    }

    /// The URL as it is safe to display.
    pub fn display_url(&self) -> String {
        if !self.url_contains_secret {
            return self.url.clone();
        }
        redact_url_secret(&self.url)
    }

    /// True when any part of this request carries a credential.
    pub fn is_authenticated(&self) -> bool {
        self.url_contains_secret || self.headers.iter().any(|(_, value)| value.is_secret())
    }

    /// A stable, secret-free rendering for `--dry-run` and audit records.
    pub fn describe(&self) -> String {
        let mut out = format!("{} {}", self.method.as_str(), self.display_url());
        for (name, value) in &self.headers {
            out.push_str(&format!("\n  {name}: {value}"));
        }
        match &self.body {
            RequestBody::Empty => {}
            RequestBody::Json(json) => {
                out.push_str(&format!("\n  body: json ({} bytes)", json.len()));
            }
            RequestBody::File { path, content_type } => {
                out.push_str(&format!("\n  body: {} ({content_type})", path.display()));
            }
            RequestBody::Multipart {
                path,
                file_field,
                fields,
            } => {
                out.push_str(&format!(
                    "\n  body: multipart {file_field}={}",
                    path.display()
                ));
                for (name, value) in fields {
                    out.push_str(&format!("\n         {name}={value}"));
                }
            }
            RequestBody::CargoFramed { path, metadata } => {
                out.push_str(&format!(
                    "\n  body: cargo-framed {} + {} bytes of metadata",
                    path.display(),
                    metadata.len()
                ));
            }
        }
        out
    }
}

/// Replace the secret span of a URL that embeds one.
///
/// Both shapes are handled positionally rather than by pattern-matching the
/// token itself: a token is opaque, so anything that tried to recognize it
/// would fail open.
fn redact_url_secret(url: &str) -> String {
    if let Some((base, query)) = url.split_once('?') {
        let redacted: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, _)) if is_secret_query_key(key) => format!("{key}=<redacted>"),
                _ => pair.to_string(),
            })
            .collect();
        return format!("{base}?{}", redacted.join("&"));
    }
    // Path-embedded (LuaRocks `/api/1/{key}/upload`): redact the segment
    // before the trailing action.
    let mut segments: Vec<&str> = url.split('/').collect();
    if segments.len() >= 2 {
        let index = segments.len() - 2;
        segments[index] = "<redacted>";
        return segments.join("/");
    }
    "<redacted>".to_string()
}

fn is_secret_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token") || key.contains("key") || key.contains("password")
}

/// Why a registry route could not be turned into a request.
///
/// A plain enum with a hand-written `Display` rather than a `thiserror` derive:
/// this is the crate's only error type that is not `anyhow`, and one dependency
/// for one enum is one more thing to audit in a tool that handles publish
/// tokens.
#[derive(Debug, PartialEq, Eq)]
pub enum NativeHostClientError {
    /// The host has no upload API; releases are picked up from a VCS tag.
    VcsPublished {
        host: NativeHost,
        tag: String,
    },
    /// The host mirrors another registry and accepts nothing of its own.
    ReadOnly {
        host: NativeHost,
    },
    /// Publishing needs a request sequence, and a later request depends on an
    /// earlier response body.
    MultiStepPublish {
        host: NativeHost,
        step: &'static str,
    },
    IndexUnsupported {
        host: NativeHost,
        reason: &'static str,
    },
    MissingCredential {
        host: NativeHost,
        env: Vec<&'static str>,
    },
    NoPublishEndpoint {
        host: NativeHost,
    },
    MalformedIndex(&'static str, NativeHost),
}

impl fmt::Display for NativeHostClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VcsPublished { host, tag } => write!(
                f,
                "`{host}` publishes by pushing a VCS tag, not by uploading to a registry; \
                 tag `{tag}` and let the index pick it up"
            ),
            Self::ReadOnly { host } => write!(
                f,
                "`{host}` accepts no uploads; it mirrors another registry's contents"
            ),
            Self::MultiStepPublish { host, step } => write!(
                f,
                "publishing to `{host}` takes more than one request ({step}); \
                 run `zed release plan` for the full sequence"
            ),
            Self::IndexUnsupported { host, reason } => {
                write!(f, "zed cannot yet list versions from `{host}`: {reason}")
            }
            Self::MissingCredential { host, env } => {
                write!(f, "`{host}` needs a credential; set {}", env.join(" or "))
            }
            Self::NoPublishEndpoint { host } => {
                write!(f, "`{host}` has no publish endpoint configured")
            }
            Self::MalformedIndex(field, host) => {
                write!(f, "could not read `{field}` from the `{host}` response")
            }
        }
    }
}

impl std::error::Error for NativeHostClientError {}

/// Environment variables a host's credential is read from, in priority order.
///
/// zed reads the same variables the ecosystem's own tooling does, so a runner
/// already configured for `npm publish` is already configured for this. Nothing
/// here shells out to a credential helper: a publish token must be an explicit
/// input, not something inherited from an ambient login.
pub fn credential_env_vars(host: NativeHost) -> &'static [&'static str] {
    use NativeHost::*;
    match host {
        Npm => &["ZED_NPM_TOKEN", "NPM_TOKEN", "NODE_AUTH_TOKEN"],
        CratesIo => &["ZED_CARGO_TOKEN", "CARGO_REGISTRY_TOKEN"],
        PyPi | TestPyPi => &["ZED_PYPI_TOKEN", "TWINE_PASSWORD", "UV_PUBLISH_TOKEN"],
        MavenCentral | Clojars => &["ZED_MAVEN_TOKEN", "MAVEN_PASSWORD"],
        RubyGems => &["ZED_RUBYGEMS_TOKEN", "GEM_HOST_API_KEY"],
        NuGet | PowerShellGallery => &["ZED_NUGET_API_KEY", "NUGET_API_KEY"],
        Hex => &["ZED_HEX_API_KEY", "HEX_API_KEY"],
        PubDev => &["ZED_PUB_TOKEN", "PUB_TOKEN"],
        Hackage => &["ZED_HACKAGE_TOKEN", "HACKAGE_PASSWORD"],
        LuaRocks => &["ZED_LUAROCKS_API_KEY", "LUAROCKS_API_KEY"],
        Cpan => &["ZED_PAUSE_PASSWORD", "PAUSE_PASSWORD"],
        CocoaPods => &["ZED_COCOAPODS_TOKEN", "COCOAPODS_TRUNK_TOKEN"],
        Packagist => &["ZED_PACKAGIST_TOKEN", "PACKAGIST_API_TOKEN"],
        ConanCenter => &["ZED_CONAN_TOKEN", "CONAN_PASSWORD"],
        SwiftPackageIndex => &["ZED_SWIFT_REGISTRY_TOKEN"],
        // VCS-published and moderated hosts have no registry credential; the
        // VCS remote's own credential is what matters.
        GoProxy | Cran | Stackage | JuliaGeneral | Opam | Dub | Nimble | Shards | Racket | Zig
        | Vcs => &[],
    }
}

/// Read a host's credential from the environment.
pub fn credential_for(host: NativeHost) -> Option<String> {
    credential_env_vars(host)
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())
}

/// Attach a host's auth scheme to a request.
fn authenticate(request: RegistryRequest, host: NativeHost, credential: &str) -> RegistryRequest {
    match host.publish_auth() {
        RegistryAuth::Bearer => request.header(
            "Authorization",
            HeaderValue::Secret(format!("Bearer {credential}")),
        ),
        // crates.io, RubyGems, and Hex reject `Bearer`: the raw token goes in
        // `Authorization` with no scheme word.
        RegistryAuth::BareToken => {
            request.header("Authorization", HeaderValue::Secret(credential.to_string()))
        }
        RegistryAuth::Basic => {
            let username = host.basic_auth_username().unwrap_or("");
            request.header(
                "Authorization",
                HeaderValue::Secret(format!(
                    "Basic {}",
                    base64_encode(format!("{username}:{credential}").as_bytes())
                )),
            )
        }
        RegistryAuth::Header(header) => request.header(
            header.header_name(),
            HeaderValue::Secret(format!("{}{credential}", header.value_prefix())),
        ),
        // Already in the URL; `url_contains_secret` is set by the caller that
        // built it.
        RegistryAuth::UrlEmbedded => request,
        RegistryAuth::VcsCredential | RegistryAuth::None => request,
    }
}

/// Minimal RFC 4648 base64, for the `Authorization: Basic` header.
///
/// Hand-rolled rather than pulling a crate in: this is the only base64 the CLI
/// needs, and a dependency added for 20 lines is a dependency to audit.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Percent-encode a path segment. Package names reach these URLs from a
/// manifest, so a scoped npm name (`@acme/client`) must not silently become two
/// path segments.
fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Go module paths escape uppercase as `!` + lowercase, because the proxy is
/// served from case-insensitive storage and `Foo` and `foo` would collide.
fn go_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_uppercase() {
            out.push('!');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Build the request that lists every published version of `package`.
pub fn version_index_request(
    route: &ChannelRoute,
    package: &str,
) -> Result<RegistryRequest, NativeHostClientError> {
    use RegistryProtocol as P;
    let host = route.host;
    let index = route.endpoints.index.trim_end_matches('/');
    let name = encode_segment(package);

    let request = match route.protocol {
        P::Npm => RegistryRequest::new(Method::Get, format!("{index}/{name}")).header(
            "Accept",
            // The abbreviated packument omits per-version READMEs, which on a
            // large package is most of the payload.
            HeaderValue::Literal("application/vnd.npm.install-v1+json".to_string()),
        ),
        P::CargoSparse => RegistryRequest::new(
            Method::Get,
            format!("{index}/{}", cargo_index_path(package)),
        ),
        P::PypiLegacyUpload => RegistryRequest::new(Method::Get, format!("{index}/{name}/"))
            .header(
                "Accept",
                HeaderValue::Literal("application/vnd.pypi.simple.v1+json".to_string()),
            ),
        P::RubyGemsApi => {
            RegistryRequest::new(Method::Get, format!("{index}/versions/{name}.json"))
        }
        P::NuGetV3 => RegistryRequest::new(
            Method::Get,
            format!(
                "{}/{}/index.json",
                route.endpoints.download_base().trim_end_matches('/'),
                package.to_ascii_lowercase()
            ),
        ),
        // The PowerShell Gallery is NuGet **V2**: OData, not a V3 service
        // index, so there is no flat container to read. Reusing the V3 shape
        // 404s on every package.
        P::PowerShellGallery => RegistryRequest::new(
            Method::Get,
            format!("{index}/FindPackagesById()?id='{name}'&$select=Version"),
        ),
        P::Maven2 | P::MavenCentralPortal => {
            let (group, artifact) = split_maven(package, host)?;
            RegistryRequest::new(
                Method::Get,
                format!(
                    "{index}/{}/{artifact}/maven-metadata.xml",
                    group.replace('.', "/")
                ),
            )
        }
        P::HexApi => {
            RegistryRequest::new(Method::Get, format!("https://hex.pm/api/packages/{name}"))
        }
        P::PubDev => RegistryRequest::new(Method::Get, format!("{index}/packages/{name}")),
        P::HackageApi => RegistryRequest::new(
            Method::Get,
            format!("https://hackage.haskell.org/package/{name}.json"),
        ),
        P::GoProxy => RegistryRequest::new(
            Method::Get,
            format!("{index}/{}/@v/list", go_escape(package)),
        ),
        P::CranSubmit => {
            // CRAN itself serves only the current source tree; crandb is the
            // version history the R community actually queries.
            RegistryRequest::new(Method::Get, format!("https://crandb.r-pkg.org/{name}/all"))
        }
        // `/release/{dist}` returns only the latest release, which a
        // "list versions" command must not present as the complete set. The
        // search endpoint returns every release; `_source` trims the response
        // to the one field needed (`fields` is rejected outright by the API).
        P::CpanPause => RegistryRequest::new(
            Method::Get,
            format!(
                "https://fastapi.metacpan.org/v1/release/_search?q=distribution:{name}&_source=version&size=250"
            ),
        ),
        P::CocoapodsTrunk => RegistryRequest::new(
            Method::Get,
            format!("https://trunk.cocoapods.org/api/v1/pods/{name}"),
        ),
        P::SwiftRegistry => match host {
            // The Swift Package Index is an index of Git repositories, not an
            // SE-0292 package registry: every path under its `/api` requires
            // an API token and returns 401 anonymously. Versions come from the
            // repository's tags. A self-hosted SE-0292 registry reached
            // through a `UniversalHost` mirror does speak this protocol.
            NativeHost::SwiftPackageIndex => {
                return Err(NativeHostClientError::IndexUnsupported {
                    host,
                    reason: "the Swift Package Index indexes Git repositories and its API \
                             needs a token; resolve versions from the repository's tags",
                });
            }
            _ => {
                let (scope, package_name) = package
                    .split_once('.')
                    .ok_or(NativeHostClientError::MalformedIndex("scope.name", host))?;
                RegistryRequest::new(
                    Method::Get,
                    format!(
                        "{index}/{}/{}",
                        encode_segment(scope),
                        encode_segment(package_name)
                    ),
                )
                .header(
                    "Accept",
                    HeaderValue::Literal("application/vnd.swift.registry.v1+json".to_string()),
                )
            }
        },
        // One publish shape, several index shapes — dispatch the rest by host.
        P::VcsIndexed => match host {
            NativeHost::Packagist => {
                RegistryRequest::new(Method::Get, format!("{index}/{package}.json"))
            }
            NativeHost::Dub => RegistryRequest::new(
                Method::Get,
                format!("https://code.dlang.org/api/packages/{name}/info"),
            ),
            NativeHost::Nimble => RegistryRequest::new(
                Method::Get,
                format!("https://nimble.directory/api/v1/package/{name}"),
            ),
            _ => {
                return Err(NativeHostClientError::IndexUnsupported {
                    host,
                    reason: "this index is a flat catalogue with no per-package endpoint",
                });
            }
        },
        P::LuaRocksApi => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "the LuaRocks manifest is a Lua table rather than JSON",
            });
        }
        P::ConanRest => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "ConanCenter serves recipes through a search API that needs a revision",
            });
        }
        P::JuliaGeneral | P::OpamRepository => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "this registry is a Git repository; clone it and read the package entry",
            });
        }
        P::DirectUrl => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "there is no registry; versions come from the VCS remote's tags",
            });
        }
    };
    Ok(request)
}

/// crates.io's sparse index shards by name length: `a`, `2/b`, `3/c/cc`, then
/// `ab/cd/abcd`.
fn cargo_index_path(package: &str) -> String {
    let lower = package.to_ascii_lowercase();
    match lower.len() {
        0 => lower,
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => format!("3/{}/{lower}", &lower[0..1]),
        _ => format!("{}/{}/{lower}", &lower[0..2], &lower[2..4]),
    }
}

/// Split Maven coordinates into group and artifact.
///
/// Accepts both separators on purpose. Maven Central writes
/// `com.google.guava:guava`, but Clojars and Leiningen write
/// `ring/ring-core`, and the manifest carries whichever spelling the author
/// used — `NativeRegistry::canonical_package` normalizes only for collision
/// detection, so the route still holds the original. Rejecting `/` here made
/// every real Clojars route fail at request-construction time.
fn split_maven(package: &str, host: NativeHost) -> Result<(&str, &str), NativeHostClientError> {
    package
        .split_once(':')
        .or_else(|| package.split_once('/'))
        .ok_or(NativeHostClientError::MalformedIndex(
            "group:artifact or group/artifact",
            host,
        ))
}

/// Build the request that downloads one published version.
pub fn download_request(
    route: &ChannelRoute,
    package: &str,
    version: &str,
) -> Result<RegistryRequest, NativeHostClientError> {
    use RegistryProtocol as P;
    let host = route.host;
    let base = route.endpoints.download_base().trim_end_matches('/');
    let name = encode_segment(package);
    let url = match route.protocol {
        // npm tarball names drop the scope: `@acme/client` -> `client-1.0.0.tgz`.
        P::Npm => {
            let bare = package.rsplit('/').next().unwrap_or(package);
            format!("{base}/{name}/-/{}-{version}.tgz", encode_segment(bare))
        }
        P::CargoSparse => format!("{base}/{name}/{name}-{version}.crate"),
        P::RubyGemsApi => format!("{base}/{name}-{version}.gem"),
        P::NuGetV3 => {
            let lower = package.to_ascii_lowercase();
            format!("{base}/{lower}/{version}/{lower}.{version}.nupkg")
        }
        // V2 serves the package straight off `/package/{id}/{version}`.
        P::PowerShellGallery => format!("{base}/package/{name}/{version}"),
        P::Maven2 | P::MavenCentralPortal => {
            let (group, artifact) = split_maven(package, host)?;
            format!(
                "{base}/{}/{artifact}/{version}/{artifact}-{version}.jar",
                group.replace('.', "/")
            )
        }
        P::HexApi => format!("{base}/{name}-{version}.tar"),
        P::HackageApi => format!("{base}/{name}-{version}/{name}-{version}.tar.gz"),
        P::GoProxy => format!("{base}/{}/@v/{version}.zip", go_escape(package)),
        P::CranSubmit => format!("{base}/{name}_{version}.tar.gz"),
        // These serve a per-version URL that only the index response knows,
        // so a download needs the index first rather than a template.
        P::PypiLegacyUpload | P::PubDev | P::SwiftRegistry | P::CocoapodsTrunk => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "the download URL is carried in the index response; resolve first",
            });
        }
        P::VcsIndexed | P::DirectUrl | P::JuliaGeneral | P::OpamRepository => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "artifacts come from the VCS remote, not this registry",
            });
        }
        P::LuaRocksApi => format!("{base}/{name}-{version}.src.rock"),
        P::CpanPause | P::ConanRest => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "the artifact path depends on metadata only the index carries",
            });
        }
    };
    Ok(RegistryRequest::new(Method::Get, url))
}

/// Build the request that uploads `artifact` to the route's channel.
///
/// `credential` is required for every host whose auth scheme is not
/// [`RegistryAuth::None`] or [`RegistryAuth::VcsCredential`]; callers get
/// [`NativeHostClientError::MissingCredential`] naming the environment
/// variables rather than a 401 from the registry.
pub fn publish_request(
    route: &ChannelRoute,
    package: &str,
    artifact: &Path,
    credential: Option<&str>,
) -> Result<RegistryRequest, NativeHostClientError> {
    use RegistryProtocol as P;
    let host = route.host;

    if !route.protocol.uploads_artifact() {
        return Err(NativeHostClientError::VcsPublished {
            host,
            tag: route.version.clone(),
        });
    }
    let Some(publish_endpoint) = route.endpoints.publish.as_deref() else {
        return Err(NativeHostClientError::ReadOnly { host });
    };
    // Two readings of the same endpoint. `publish_base` is a prefix a path is
    // appended to, so its trailing slash must go or the join doubles it.
    // `publish_endpoint` is posted to verbatim, and there the trailing slash is
    // load-bearing: `https://upload.pypi.org/legacy` (no slash) 404s.
    let publish_base = publish_endpoint.trim_end_matches('/');

    let needs_credential = !matches!(
        host.publish_auth(),
        RegistryAuth::None | RegistryAuth::VcsCredential
    );
    let credential = match (needs_credential, credential) {
        (true, None) => {
            return Err(NativeHostClientError::MissingCredential {
                host,
                env: credential_env_vars(host).to_vec(),
            });
        }
        (_, value) => value.unwrap_or_default(),
    };

    let name = encode_segment(package);
    let version = &route.version;

    let request = match route.protocol {
        P::Npm => {
            // The packument body is assembled by the caller that knows the
            // package.json; here it is the transport that matters.
            RegistryRequest::new(Method::Put, format!("{publish_base}/{name}"))
                .header(
                    "Content-Type",
                    HeaderValue::Literal("application/json".to_string()),
                )
                .body(RequestBody::Json(npm_publish_envelope(
                    package,
                    version,
                    route.dist_tag.as_deref().unwrap_or("latest"),
                )))
        }
        P::CargoSparse => RegistryRequest::new(Method::Put, format!("{publish_base}/crates/new"))
            .body(RequestBody::CargoFramed {
                path: artifact.to_path_buf(),
                metadata: format!(r#"{{"name":"{package}","vers":"{version}"}}"#),
            }),
        P::PypiLegacyUpload => RegistryRequest::new(Method::Post, publish_endpoint.to_string())
            .body(RequestBody::Multipart {
                path: artifact.to_path_buf(),
                file_field: "content",
                fields: vec![
                    (":action", "file_upload".to_string()),
                    ("protocol_version", "1".to_string()),
                    ("name", package.to_string()),
                    ("version", version.clone()),
                ],
            }),
        P::RubyGemsApi => RegistryRequest::new(Method::Post, format!("{publish_base}/gems")).body(
            RequestBody::File {
                path: artifact.to_path_buf(),
                content_type: "application/octet-stream",
            },
        ),
        P::NuGetV3 | P::PowerShellGallery => {
            RegistryRequest::new(Method::Put, publish_endpoint.to_string()).body(
                RequestBody::Multipart {
                    path: artifact.to_path_buf(),
                    file_field: "package",
                    fields: Vec::new(),
                },
            )
        }
        P::Maven2 => {
            let (group, artifact_id) = split_maven(package, host)?;
            RegistryRequest::new(
                Method::Put,
                format!(
                    "{publish_base}/{}/{artifact_id}/{version}/{artifact_id}-{version}.jar",
                    group.replace('.', "/")
                ),
            )
            .body(RequestBody::File {
                path: artifact.to_path_buf(),
                content_type: "application/java-archive",
            })
        }
        P::HexApi => RegistryRequest::new(Method::Post, format!("{publish_base}/publish")).body(
            RequestBody::File {
                path: artifact.to_path_buf(),
                content_type: "application/octet-stream",
            },
        ),
        P::HackageApi => RegistryRequest::new(Method::Post, format!("{publish_base}/")).body(
            RequestBody::Multipart {
                path: artifact.to_path_buf(),
                file_field: "package",
                fields: Vec::new(),
            },
        ),
        P::CpanPause => RegistryRequest::new(
            Method::Post,
            format!("{publish_base}?ACTION=add_uri&pause99_add_uri_httpupload=1"),
        )
        .body(RequestBody::Multipart {
            path: artifact.to_path_buf(),
            file_field: "pause99_add_uri_httpupload",
            fields: vec![("HIDDENNAME", package.to_string())],
        }),
        P::LuaRocksApi => {
            let mut request =
                RegistryRequest::new(Method::Post, format!("{publish_base}/{credential}/upload"))
                    .body(RequestBody::Multipart {
                        path: artifact.to_path_buf(),
                        file_field: "rockspec_file",
                        fields: Vec::new(),
                    });
            request.url_contains_secret = true;
            request
        }
        P::CocoapodsTrunk => RegistryRequest::new(Method::Post, format!("{publish_base}/pods"))
            .header(
                "Content-Type",
                HeaderValue::Literal("application/json; charset=utf-8".to_string()),
            )
            .body(RequestBody::File {
                path: artifact.to_path_buf(),
                content_type: "application/json",
            }),
        P::SwiftRegistry => {
            let (scope, package_name) = package
                .split_once('.')
                .ok_or(NativeHostClientError::MalformedIndex("scope.name", host))?;
            RegistryRequest::new(
                Method::Put,
                format!(
                    "{publish_base}/{}/{}/{version}",
                    encode_segment(scope),
                    encode_segment(package_name)
                ),
            )
            .body(RequestBody::Multipart {
                path: artifact.to_path_buf(),
                file_field: "source-archive",
                fields: Vec::new(),
            })
        }
        P::CranSubmit => RegistryRequest::new(Method::Post, publish_endpoint.to_string()).body(
            RequestBody::Multipart {
                path: artifact.to_path_buf(),
                file_field: "uploaded_file",
                fields: vec![("name", package.to_string())],
            },
        ),
        // Two requests minimum, and the second depends on the first's body.
        P::PubDev => {
            return Err(NativeHostClientError::MultiStepPublish {
                host,
                step: "GET /api/packages/versions/new returns a signed upload form",
            });
        }
        P::MavenCentralPortal => {
            return Err(NativeHostClientError::MultiStepPublish {
                host,
                step: "the bundle upload returns a deployment id that must then be polled",
            });
        }
        P::ConanRest => {
            return Err(NativeHostClientError::MultiStepPublish {
                host,
                step: "a recipe revision must be created before its files are uploaded",
            });
        }
        P::VcsIndexed | P::GoProxy | P::DirectUrl | P::JuliaGeneral | P::OpamRepository => {
            return Err(NativeHostClientError::VcsPublished {
                host,
                tag: version.clone(),
            });
        }
    };

    Ok(authenticate(request, host, credential))
}

/// Parse the version list out of an index response.
pub fn parse_versions(
    route: &ChannelRoute,
    body: &str,
) -> Result<Vec<String>, NativeHostClientError> {
    use RegistryProtocol as P;
    let host = route.host;
    let malformed = |field: &'static str| NativeHostClientError::MalformedIndex(field, host);

    let mut versions: Vec<String> = match route.protocol {
        P::Npm => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("packument"))?;
            json.get("versions")
                .and_then(|v| v.as_object())
                .ok_or_else(|| malformed("versions"))?
                .keys()
                .cloned()
                .collect()
        }
        // The sparse index is newline-delimited JSON, one object per version.
        P::CargoSparse => body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()?
                    .get("vers")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect(),
        P::PypiLegacyUpload => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("simple index"))?;
            json.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| malformed("versions"))?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        }
        P::RubyGemsApi => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("versions"))?;
            json.as_array()
                .ok_or_else(|| malformed("versions"))?
                .iter()
                .filter_map(|v| v.get("number")?.as_str().map(str::to_string))
                .collect()
        }
        // `<d:Version>1.2.3</d:Version>` repeated once per entry. A full XML
        // parser is not warranted for one machine-generated element.
        P::PowerShellGallery => body
            .split("<d:Version>")
            .skip(1)
            .filter_map(|chunk| chunk.split_once("</d:Version>"))
            .map(|(version, _)| version.trim().to_string())
            .collect(),
        P::NuGetV3 => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("flat container index"))?;
            json.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| malformed("versions"))?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        }
        // maven-metadata.xml. A full XML parser is not warranted for one
        // repeated element, and a scan cannot be fooled here: the document is
        // machine-generated and the tag is unambiguous.
        P::Maven2 | P::MavenCentralPortal => body
            .split("<version>")
            .skip(1)
            .filter_map(|chunk| chunk.split_once("</version>"))
            .map(|(version, _)| version.trim().to_string())
            .collect(),
        P::HexApi => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("package"))?;
            json.get("releases")
                .and_then(|v| v.as_array())
                .ok_or_else(|| malformed("releases"))?
                .iter()
                .filter_map(|v| v.get("version")?.as_str().map(str::to_string))
                .collect()
        }
        P::PubDev => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("package"))?;
            json.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| malformed("versions"))?
                .iter()
                .filter_map(|v| v.get("version")?.as_str().map(str::to_string))
                .collect()
        }
        // Hackage returns `{"1.0.0": "normal", "1.1.0": "deprecated"}`.
        P::HackageApi => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("package"))?;
            json.as_object()
                .ok_or_else(|| malformed("package"))?
                .keys()
                .cloned()
                .collect()
        }
        P::GoProxy => body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        P::CranSubmit => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("crandb record"))?;
            json.get("versions")
                .and_then(|v| v.as_object())
                .ok_or_else(|| malformed("versions"))?
                .keys()
                .cloned()
                .collect()
        }
        P::CpanPause => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("release search"))?;
            json.get("hits")
                .and_then(|hits| hits.get("hits"))
                .and_then(|hits| hits.as_array())
                .ok_or_else(|| malformed("hits.hits"))?
                .iter()
                .filter_map(|hit| {
                    hit.get("_source")?
                        .get("version")?
                        .as_str()
                        .map(str::to_string)
                })
                .collect()
        }
        P::CocoapodsTrunk => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("pod"))?;
            json.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| malformed("versions"))?
                .iter()
                .filter_map(|v| v.get("name")?.as_str().map(str::to_string))
                .collect()
        }
        P::SwiftRegistry => {
            let json: serde_json::Value =
                serde_json::from_str(body).map_err(|_| malformed("releases"))?;
            json.get("releases")
                .and_then(|v| v.as_object())
                .ok_or_else(|| malformed("releases"))?
                .keys()
                .cloned()
                .collect()
        }
        P::VcsIndexed => match host {
            NativeHost::Packagist => {
                let json: serde_json::Value =
                    serde_json::from_str(body).map_err(|_| malformed("p2 document"))?;
                json.get("packages")
                    .and_then(|v| v.as_object())
                    .and_then(|packages| packages.values().next())
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| malformed("packages"))?
                    .iter()
                    .filter_map(|v| v.get("version")?.as_str().map(str::to_string))
                    .collect()
            }
            _ => {
                let json: serde_json::Value =
                    serde_json::from_str(body).map_err(|_| malformed("package info"))?;
                json.get("versions")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| malformed("versions"))?
                    .iter()
                    .filter_map(|v| {
                        v.get("version")
                            .or(Some(v))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            }
        },
        P::LuaRocksApi | P::ConanRest | P::JuliaGeneral | P::OpamRepository | P::DirectUrl => {
            return Err(NativeHostClientError::IndexUnsupported {
                host,
                reason: "no machine-readable per-package index",
            });
        }
    };

    versions.sort();
    versions.dedup();
    Ok(versions)
}

/// Hard ceiling on a registry response body (bytes).
///
/// A registry's own `Content-Length` is advisory and, for a hostile or
/// compromised mirror, attacker-controlled — so the cap has to be the client's
/// and has to be enforced while reading. The largest packuments in the wild are
/// a few MiB; this is sized to stop an OOM, not to reject npm. Mirrors the Zed
/// registry client's `ZED_PKG_MAX_ARTIFACT_BYTES`.
const DEFAULT_MAX_REGISTRY_BYTES: u64 = 128 * 1024 * 1024;

/// How long to wait on one registry request.
///
/// Set explicitly rather than inherited: a release job that blocks forever on
/// one unresponsive mirror is a stuck pipeline, and a stuck pipeline is much
/// harder to diagnose than a failed one.
const REGISTRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Bounds applied to one registry request.
///
/// A struct rather than two globals so a test can pin them without mutating
/// process environment — `cargo test` runs tests in parallel threads within one
/// process, so an env-var override set by one test silently applies to every
/// other test in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryLimits {
    pub max_bytes: u64,
    pub timeout: std::time::Duration,
}

impl Default for RegistryLimits {
    /// Reads `ZED_PKG_MAX_REGISTRY_BYTES` so an operator behind a mirror that
    /// serves unusually large indexes can raise the ceiling.
    fn default() -> Self {
        Self {
            max_bytes: std::env::var("ZED_PKG_MAX_REGISTRY_BYTES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_MAX_REGISTRY_BYTES),
            timeout: REGISTRY_TIMEOUT,
        }
    }
}

/// Read a response body, refusing anything past the cap.
///
/// Reads one byte more than the limit so "exactly at the cap" is accepted and
/// "over" is detected without trusting the declared length.
fn read_capped(response: reqwest::blocking::Response, url: &str, cap: u64) -> Result<String> {
    use std::io::Read;

    let mut buffer = Vec::new();
    response
        .take(cap.saturating_add(1))
        .read_to_end(&mut buffer)
        .with_context(|| format!("read response body from {url}"))?;
    if buffer.len() as u64 > cap {
        anyhow::bail!("response from {url} exceeds the {cap}-byte registry response limit");
    }
    String::from_utf8(buffer).with_context(|| format!("response from {url} is not valid UTF-8"))
}

/// Send a built request and return its body.
///
/// Kept separate from construction so every routing decision above is testable
/// without a socket, and so a dry run exercises the identical code path up to
/// the point of sending.
pub fn execute(request: &RegistryRequest) -> Result<String> {
    execute_with_limits(request, RegistryLimits::default())
}

/// [`execute`] with explicit bounds.
pub fn execute_with_limits(request: &RegistryRequest, limits: RegistryLimits) -> Result<String> {
    execute_detailed(request, limits).map(|response| response.body)
}

/// What a registry answered, beyond the body.
///
/// `location` is carried because two publish flows need it: pub.dev answers a
/// successful upload with `204` plus a `Location` pointing at the finalize
/// step, so a body-only client cannot complete a Dart publish at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryResponse {
    pub status: u16,
    pub location: Option<String>,
    pub body: String,
}

/// [`execute_with_limits`], keeping the status line and `Location`.
pub fn execute_detailed(
    request: &RegistryRequest,
    limits: RegistryLimits,
) -> Result<RegistryResponse> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(limits.timeout)
        .build()
        .context("build registry HTTP client")?;

    let mut builder = match request.method {
        Method::Get => client.get(&request.url),
        Method::Put => client.put(&request.url),
        Method::Post => client.post(&request.url),
    };
    for (name, value) in &request.headers {
        builder = builder.header(name, value.expose());
    }
    builder = match &request.body {
        RequestBody::Empty => builder,
        RequestBody::Json(json) => builder.body(json.clone()),
        RequestBody::File { path, content_type } => {
            let bytes =
                std::fs::read(path).with_context(|| format!("read artifact {}", path.display()))?;
            builder.header("Content-Type", *content_type).body(bytes)
        }
        RequestBody::Multipart {
            path,
            file_field,
            fields,
        } => {
            let mut form = reqwest::blocking::multipart::Form::new();
            for (name, value) in fields {
                form = form.text(*name, value.clone());
            }
            form = form
                .file(*file_field, path)
                .with_context(|| format!("attach artifact {}", path.display()))?;
            builder.multipart(form)
        }
        RequestBody::CargoFramed { path, metadata } => {
            let crate_bytes =
                std::fs::read(path).with_context(|| format!("read artifact {}", path.display()))?;
            let mut body = Vec::with_capacity(metadata.len() + crate_bytes.len() + 8);
            body.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
            body.extend_from_slice(metadata.as_bytes());
            body.extend_from_slice(&(crate_bytes.len() as u32).to_le_bytes());
            body.extend_from_slice(&crate_bytes);
            builder.body(body)
        }
    };

    let response = builder
        .send()
        .with_context(|| format!("{} {}", request.method.as_str(), request.display_url()))?;
    let status = response.status();
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    // The URL is printed through `display_url`, never `url`: a failed LuaRocks
    // or Packagist request must not put its key in a log.
    let display = request.display_url();
    let body = read_capped(response, &display, limits.max_bytes);

    if !status.is_success() {
        // On a failure status the body is diagnostic, so a body that is itself
        // unreadable must not mask the status the caller actually needs.
        let detail = body
            .as_deref()
            .unwrap_or("<unreadable response body>")
            .chars()
            .take(500)
            .collect::<String>();
        anyhow::bail!(
            "{} {display} failed with {status}: {detail}",
            request.method.as_str()
        );
    }
    // On success an unreadable body is a hard error. Returning an empty string
    // would read downstream as "this package has no versions", which is a
    // wrong answer rather than a failure.
    Ok(RegistryResponse {
        status: status.as_u16(),
        location,
        body: body?,
    })
}

/// One completed step of a publish, for reporting and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishStep {
    pub description: String,
    pub status: u16,
}

/// Drive a publish to completion, however many requests that takes.
///
/// Most hosts are one `PUT` or `POST`, and for those this is a thin wrapper
/// over [`publish_request`]. Two are not, and cannot be expressed as a single
/// request because a later step's URL is only known once an earlier one has
/// answered:
///
/// * **pub.dev** — ask for a signed upload form, post the archive to wherever
///   that form points (object storage, not pub.dev), then fetch the finalize
///   URL the upload returns in `Location`.
/// * **Maven Central Portal** — upload a bundle, receive a deployment id, then
///   poll until it leaves `PENDING`/`VALIDATING`.
///
/// `send` is a parameter so the sequence can be tested against a mock without
/// credentials or a network. That matters more here than elsewhere: the thing
/// worth verifying about a three-step publish is the *ordering and plumbing*,
/// which is exactly what a live-credential test would obscure.
pub fn publish_sequence(
    route: &ChannelRoute,
    package: &str,
    artifact: &Path,
    credential: Option<&str>,
    send: &mut dyn FnMut(&RegistryRequest) -> Result<RegistryResponse>,
) -> Result<Vec<PublishStep>> {
    publish_sequence_paced(
        route,
        package,
        artifact,
        credential,
        PublishPacing::default(),
        send,
    )
}

/// How long to wait between polls of a deployment that validates asynchronously.
///
/// A parameter so a test can drive the Maven Central Portal sequence without
/// sleeping through real validation intervals — the thing under test is the
/// state machine, not the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishPacing {
    pub poll_attempts: usize,
    pub poll_interval: std::time::Duration,
}

impl Default for PublishPacing {
    fn default() -> Self {
        Self {
            poll_attempts: 40,
            // Central validation is minutes, not seconds.
            poll_interval: std::time::Duration::from_secs(15),
        }
    }
}

/// [`publish_sequence`] with explicit pacing.
pub fn publish_sequence_paced(
    route: &ChannelRoute,
    package: &str,
    artifact: &Path,
    credential: Option<&str>,
    pacing: PublishPacing,
    send: &mut dyn FnMut(&RegistryRequest) -> Result<RegistryResponse>,
) -> Result<Vec<PublishStep>> {
    match route.protocol {
        RegistryProtocol::PubDev => publish_pub_dev(route, artifact, credential, send),
        RegistryProtocol::MavenCentralPortal => {
            publish_maven_portal(route, artifact, credential, pacing, send)
        }
        _ => {
            let request = publish_request(route, package, artifact, credential)?;
            let response = send(&request)?;
            Ok(vec![PublishStep {
                description: format!("{} {}", request.method.as_str(), request.display_url()),
                status: response.status,
            }])
        }
    }
}

fn bearer(request: RegistryRequest, credential: Option<&str>) -> RegistryRequest {
    match credential {
        Some(token) => request.header(
            "Authorization",
            HeaderValue::Secret(format!("Bearer {token}")),
        ),
        None => request,
    }
}

/// pub.dev's three-step upload.
///
/// The archive never goes to pub.dev itself — step 1 hands back a signed form
/// for object storage, and the credential must **not** be replayed to it. That
/// is the whole reason this is three requests instead of one.
fn publish_pub_dev(
    route: &ChannelRoute,
    artifact: &Path,
    credential: Option<&str>,
    send: &mut dyn FnMut(&RegistryRequest) -> Result<RegistryResponse>,
) -> Result<Vec<PublishStep>> {
    let host = route.host;
    let credential = credential.ok_or(NativeHostClientError::MissingCredential {
        host,
        env: credential_env_vars(host).to_vec(),
    })?;
    let base = route
        .endpoints
        .publish
        .as_deref()
        .ok_or(NativeHostClientError::ReadOnly { host })?
        .trim_end_matches('/');

    let mut steps = Vec::new();

    // 1. Ask pub.dev where to put the archive.
    let ask = bearer(
        RegistryRequest::new(Method::Get, format!("{base}/packages/versions/new")),
        Some(credential),
    );
    let granted = send(&ask)?;
    steps.push(PublishStep {
        description: "request a signed upload form".to_string(),
        status: granted.status,
    });

    let form: serde_json::Value =
        serde_json::from_str(&granted.body).context("pub.dev upload grant is not JSON")?;
    let upload_url = form
        .get("url")
        .and_then(|value| value.as_str())
        .context("pub.dev upload grant has no `url`")?;
    let fields = form
        .get("fields")
        .and_then(|value| value.as_object())
        .context("pub.dev upload grant has no `fields`")?;

    // 2. Post the archive to storage. Deliberately unauthenticated: the signed
    //    form *is* the authorization, and replaying the pub.dev token to a
    //    third-party storage origin would hand it to whoever that grant names.
    let mut upload = RegistryRequest::new(Method::Post, upload_url.to_string());
    upload.body = RequestBody::Multipart {
        path: artifact.to_path_buf(),
        file_field: "file",
        fields: fields
            .iter()
            .filter_map(|(name, value)| {
                Some((leak_static(name.clone()), value.as_str()?.to_string()))
            })
            .collect(),
    };
    let uploaded = send(&upload)?;
    steps.push(PublishStep {
        description: "upload the archive to the granted storage URL".to_string(),
        status: uploaded.status,
    });

    // 3. Finalize, at the URL the upload pointed us to.
    let finalize_url = uploaded
        .location
        .clone()
        .context("pub.dev upload returned no `Location` to finalize at")?;
    let finalize = bearer(
        RegistryRequest::new(Method::Get, finalize_url),
        Some(credential),
    );
    let finalized = send(&finalize)?;
    steps.push(PublishStep {
        description: "finalize the version".to_string(),
        status: finalized.status,
    });

    Ok(steps)
}

/// Maven Central Portal: upload a bundle, then poll the deployment.
///
/// The upload answering 201 means "accepted for validation", not "published".
/// Reporting success there would tell a release job a version exists that
/// Central may still reject minutes later.
fn publish_maven_portal(
    route: &ChannelRoute,
    artifact: &Path,
    credential: Option<&str>,
    pacing: PublishPacing,
    send: &mut dyn FnMut(&RegistryRequest) -> Result<RegistryResponse>,
) -> Result<Vec<PublishStep>> {
    const TERMINAL_FAILURES: &[&str] = &["FAILED", "REJECTED"];
    let host = route.host;
    let credential = credential.ok_or(NativeHostClientError::MissingCredential {
        host,
        env: credential_env_vars(host).to_vec(),
    })?;
    let base = route
        .endpoints
        .publish
        .as_deref()
        .ok_or(NativeHostClientError::ReadOnly { host })?
        .trim_end_matches('/');

    let mut steps = Vec::new();

    let upload = RegistryRequest::new(Method::Post, format!("{base}/upload"))
        .header(
            "Authorization",
            // The Portal takes a base64 user token, not Bearer.
            HeaderValue::Secret(format!("UserToken {credential}")),
        )
        .body(RequestBody::Multipart {
            path: artifact.to_path_buf(),
            file_field: "bundle",
            fields: Vec::new(),
        });
    let accepted = send(&upload)?;
    steps.push(PublishStep {
        description: "upload the deployment bundle".to_string(),
        status: accepted.status,
    });

    // The body is the bare deployment id, sometimes quoted.
    let deployment = accepted.body.trim().trim_matches('"').to_string();
    if deployment.is_empty() {
        anyhow::bail!("Maven Central Portal accepted the bundle but returned no deployment id");
    }

    for attempt in 1..=pacing.poll_attempts {
        let status_request = RegistryRequest::new(
            Method::Post,
            format!("{base}/status?id={}", encode_segment(&deployment)),
        )
        .header(
            "Authorization",
            HeaderValue::Secret(format!("UserToken {credential}")),
        );
        let polled = send(&status_request)?;
        let state = serde_json::from_str::<serde_json::Value>(&polled.body)
            .ok()
            .and_then(|value| {
                value
                    .get("deploymentState")
                    .and_then(|state| state.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        steps.push(PublishStep {
            description: format!("poll deployment {deployment} (state: {state})"),
            status: polled.status,
        });

        if TERMINAL_FAILURES.contains(&state.as_str()) {
            anyhow::bail!("Maven Central Portal deployment {deployment} ended in {state}");
        }
        if state == "PUBLISHED" || state == "VALIDATED" {
            return Ok(steps);
        }
        if attempt < pacing.poll_attempts {
            std::thread::sleep(pacing.poll_interval);
        }
    }

    anyhow::bail!(
        "Maven Central Portal deployment {deployment} did not settle after {} polls; \
         it may still complete — check the Portal UI",
        pacing.poll_attempts
    )
}

/// Multipart field names are `&'static str` throughout this module because
/// every other caller has a fixed set. pub.dev's come from the server, so they
/// are leaked once per publish — bounded by the field count of a single upload
/// grant, and freed when the process exits.
fn leak_static(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

/// The npm publish envelope, minus the tarball attachment the caller adds.
fn npm_publish_envelope(package: &str, version: &str, dist_tag: &str) -> String {
    serde_json::json!({
        "_id": package,
        "name": package,
        "dist-tags": { dist_tag: version },
        "versions": { version: { "name": package, "version": version } },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::native_host::ReleaseChannel;

    fn route(host: NativeHost, channel: ReleaseChannel) -> ChannelRoute {
        host.channel_route("1.4.0", channel, 1).unwrap()
    }

    #[test]
    fn a_candidate_publish_targets_the_channel_version_not_the_base() {
        // The whole chain: manifest version -> host channel rules -> URL.
        let npm = route(NativeHost::Npm, ReleaseChannel::Rc);
        let request =
            publish_request(&npm, "@acme/client", Path::new("client.tgz"), Some("tok")).unwrap();
        assert_eq!(request.method, Method::Put);
        assert_eq!(request.url, "https://registry.npmjs.org/%40acme%2Fclient");
        match &request.body {
            RequestBody::Json(json) => {
                assert!(json.contains("1.4.0-rc.1"), "{json}");
                // And the dist-tag must be `rc`, not `latest`, or every
                // unpinned consumer moves to the candidate.
                assert!(json.contains(r#""rc":"1.4.0-rc.1""#), "{json}");
                assert!(!json.contains("latest"), "{json}");
            }
            other => panic!("expected a json body, got {other:?}"),
        }
    }

    #[test]
    fn pypi_uploads_the_pep440_version_with_the_token_username() {
        let request = publish_request(
            &route(NativeHost::PyPi, ReleaseChannel::Rc),
            "acme-client",
            Path::new("dist/acme_client-1.4.0rc1.whl"),
            Some("pypi-secret"),
        )
        .unwrap();
        assert_eq!(request.url, "https://upload.pypi.org/legacy/");
        match &request.body {
            RequestBody::Multipart { fields, .. } => {
                assert!(fields.contains(&(":action", "file_upload".to_string())));
                assert!(fields.contains(&("version", "1.4.0rc1".to_string())));
            }
            other => panic!("expected multipart, got {other:?}"),
        }
        // PyPI mandates the literal username `__token__`; the account name
        // silently 403s.
        let auth = request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .unwrap();
        let decoded = auth.1.expose().strip_prefix("Basic ").unwrap();
        assert_eq!(decoded, base64_encode(b"__token__:pypi-secret"));
    }

    #[test]
    fn credentials_never_reach_a_printable_string() {
        // Every publish request is rendered somewhere — dry runs, errors,
        // audit records — so redaction has to hold for all three shapes.
        let header_auth = publish_request(
            &route(NativeHost::CratesIo, ReleaseChannel::Stable),
            "acme-client",
            Path::new("acme.crate"),
            Some("cio-secret"),
        )
        .unwrap();
        assert!(header_auth.is_authenticated());
        assert!(!header_auth.describe().contains("cio-secret"));
        assert!(header_auth.describe().contains("<redacted>"));

        let url_auth = publish_request(
            &route(NativeHost::LuaRocks, ReleaseChannel::Stable),
            "acme-client",
            Path::new("acme.rockspec"),
            Some("lr-secret"),
        )
        .unwrap();
        assert!(url_auth.url_contains_secret);
        assert!(
            url_auth.url.contains("lr-secret"),
            "the real URL still has it"
        );
        assert!(!url_auth.describe().contains("lr-secret"));
        assert_eq!(
            url_auth.display_url(),
            "https://luarocks.org/api/1/<redacted>/upload"
        );
    }

    #[test]
    fn a_missing_credential_names_the_variables_to_set() {
        let error = publish_request(
            &route(NativeHost::Hex, ReleaseChannel::Stable),
            "acme_client",
            Path::new("acme.tar"),
            None,
        )
        .unwrap_err();
        match error {
            NativeHostClientError::MissingCredential { env, .. } => {
                assert!(env.contains(&"HEX_API_KEY"));
            }
            other => panic!("expected MissingCredential, got {other:?}"),
        }
        // A 401 from the registry would be the alternative, after the artifact
        // was already built and the release half-run.
        assert!(
            publish_request(
                &route(NativeHost::GoProxy, ReleaseChannel::Stable),
                "github.com/acme/client",
                Path::new("ignored"),
                None,
            )
            .is_err(),
            "a proxy-only host must not accept an upload either"
        );
    }

    #[test]
    fn vcs_published_hosts_say_to_tag_rather_than_failing_obscurely() {
        for host in [
            NativeHost::GoProxy,
            NativeHost::Packagist,
            NativeHost::Zig,
            NativeHost::Vcs,
            NativeHost::JuliaGeneral,
        ] {
            let error = publish_request(
                &route(host, ReleaseChannel::Stable),
                "github.com/acme/client",
                Path::new("ignored"),
                Some("tok"),
            )
            .unwrap_err();
            assert!(
                matches!(error, NativeHostClientError::VcsPublished { .. }),
                "{host}: {error}"
            );
            assert!(error.to_string().contains("1.4.0"));
        }
    }

    #[test]
    fn multi_step_hosts_name_the_step_they_are_missing() {
        for (host, needle) in [
            (NativeHost::PubDev, "signed upload form"),
            (NativeHost::MavenCentral, "deployment id"),
        ] {
            let error = publish_request(
                &route(host, ReleaseChannel::Stable),
                "com.acme:client",
                Path::new("acme.jar"),
                Some("tok"),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(needle),
                "{host} error should name the step: {error}"
            );
        }

        // ConanCenter is the case where "read-only" wins over "multi-step":
        // its recipes arrive by pull request, so there is no upload to
        // sequence. The multi-step message belongs to the Conan protocol as an
        // Artifactory mirror serves it, not to ConanCenter itself.
        assert!(matches!(
            publish_request(
                &route(NativeHost::ConanCenter, ReleaseChannel::Stable),
                "acme-client",
                Path::new("acme.tgz"),
                Some("tok"),
            ),
            Err(NativeHostClientError::ReadOnly { .. })
        ));
    }

    #[test]
    fn read_only_hosts_refuse_uploads_before_reading_a_credential() {
        let error = publish_request(
            &route(NativeHost::Stackage, ReleaseChannel::Stable),
            "acme-client",
            Path::new("acme.tar.gz"),
            Some("tok"),
        )
        .unwrap_err();
        assert!(matches!(error, NativeHostClientError::ReadOnly { .. }));
    }

    #[test]
    fn hackage_candidates_upload_to_the_candidate_endpoint() {
        let stable = publish_request(
            &route(NativeHost::Hackage, ReleaseChannel::Stable),
            "acme-client",
            Path::new("acme.tar.gz"),
            Some("tok"),
        )
        .unwrap();
        assert_eq!(stable.url, "https://hackage.haskell.org/packages/");

        let candidate = publish_request(
            &route(NativeHost::Hackage, ReleaseChannel::Rc),
            "acme-client",
            Path::new("acme.tar.gz"),
            Some("tok"),
        )
        .unwrap();
        assert_eq!(
            candidate.url,
            "https://hackage.haskell.org/packages/candidates/"
        );
    }

    #[test]
    fn maven_snapshots_publish_to_the_snapshot_repository() {
        // Same protocol, same request shape, different repository — which is
        // exactly the case a version-suffix-only model gets wrong.
        let snapshot = route(NativeHost::MavenCentral, ReleaseChannel::Snapshot);
        assert_eq!(snapshot.version, "1.4.0-SNAPSHOT");
        assert_eq!(snapshot.protocol, RegistryProtocol::MavenCentralPortal);

        let clojars = route(NativeHost::Clojars, ReleaseChannel::Snapshot);
        let request =
            publish_request(&clojars, "com.acme:client", Path::new("c.jar"), Some("t")).unwrap();
        assert_eq!(
            request.url,
            "https://repo.clojars.org/com/acme/client/1.4.0-SNAPSHOT/client-1.4.0-SNAPSHOT.jar"
        );
    }

    #[test]
    fn cargo_index_paths_shard_the_way_the_sparse_index_does() {
        assert_eq!(cargo_index_path("a"), "1/a");
        assert_eq!(cargo_index_path("ab"), "2/ab");
        assert_eq!(cargo_index_path("abc"), "3/a/abc");
        assert_eq!(cargo_index_path("serde"), "se/rd/serde");
        // Uppercase would resolve to a path the index does not serve.
        assert_eq!(cargo_index_path("Serde"), "se/rd/serde");
    }

    #[test]
    fn go_module_paths_escape_uppercase_for_case_insensitive_storage() {
        // Without this, `github.com/Acme/Client` and `github.com/acme/client`
        // collide in the proxy's storage.
        let request = version_index_request(
            &route(NativeHost::GoProxy, ReleaseChannel::Stable),
            "github.com/Acme/Client",
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://proxy.golang.org/github.com/!acme/!client/@v/list"
        );
    }

    #[test]
    fn scoped_npm_names_stay_one_path_segment() {
        let request = version_index_request(
            &route(NativeHost::Npm, ReleaseChannel::Stable),
            "@acme/client",
        )
        .unwrap();
        assert_eq!(request.url, "https://registry.npmjs.org/%40acme%2Fclient");
        assert!(!request.is_authenticated(), "reads are anonymous");
    }

    #[test]
    fn npm_tarball_urls_drop_the_scope_the_way_npm_does() {
        let request = download_request(
            &route(NativeHost::Npm, ReleaseChannel::Stable),
            "@acme/client",
            "1.4.0",
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://registry.npmjs.org/%40acme%2Fclient/-/client-1.4.0.tgz"
        );
    }

    #[test]
    fn every_index_shape_parses_back_to_a_sorted_version_list() {
        let cases: Vec<(NativeHost, &str, Vec<&str>)> = vec![
            (
                NativeHost::Npm,
                r#"{"versions":{"1.0.0":{},"1.1.0":{}}}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::CratesIo,
                "{\"name\":\"a\",\"vers\":\"0.1.0\"}\n{\"name\":\"a\",\"vers\":\"0.2.0\"}\n",
                vec!["0.1.0", "0.2.0"],
            ),
            (
                NativeHost::PyPi,
                r#"{"versions":["1.0.0","1.1.0rc1"]}"#,
                vec!["1.0.0", "1.1.0rc1"],
            ),
            (
                NativeHost::RubyGems,
                r#"[{"number":"1.0.0"},{"number":"1.1.0"}]"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::NuGet,
                r#"{"versions":["1.0.0","2.0.0-rc.1"]}"#,
                vec!["1.0.0", "2.0.0-rc.1"],
            ),
            (
                NativeHost::Clojars,
                "<metadata><versioning><versions><version>1.0.0</version>\
                 <version>1.1.0-SNAPSHOT</version></versions></versioning></metadata>",
                vec!["1.0.0", "1.1.0-SNAPSHOT"],
            ),
            (
                NativeHost::Hex,
                r#"{"releases":[{"version":"1.0.0"},{"version":"1.1.0"}]}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::PubDev,
                r#"{"versions":[{"version":"1.0.0"},{"version":"1.1.0"}]}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::Hackage,
                r#"{"1.0.0":"normal","1.1.0":"deprecated"}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::GoProxy,
                "v1.0.0\nv1.1.0\n\n",
                vec!["v1.0.0", "v1.1.0"],
            ),
            (
                NativeHost::Cran,
                r#"{"versions":{"1.0":{},"1.1":{}}}"#,
                vec!["1.0", "1.1"],
            ),
            (
                NativeHost::Packagist,
                r#"{"packages":{"acme/client":[{"version":"1.0.0"},{"version":"1.1.0"}]}}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::SwiftPackageIndex,
                r#"{"releases":{"1.0.0":{},"1.1.0":{}}}"#,
                vec!["1.0.0", "1.1.0"],
            ),
            (
                NativeHost::CocoaPods,
                r#"{"versions":[{"name":"1.0.0"},{"name":"1.1.0"}]}"#,
                vec!["1.0.0", "1.1.0"],
            ),
        ];
        for (host, body, expected) in cases {
            let parsed = parse_versions(&route(host, ReleaseChannel::Stable), body)
                .unwrap_or_else(|error| panic!("{host}: {error}"));
            assert_eq!(parsed, expected, "{host}");
        }
    }

    #[test]
    fn a_truncated_index_response_is_an_error_not_an_empty_list() {
        // An empty list would read as "no such version published", which is
        // exactly the wrong conclusion to draw from a broken response.
        for host in [NativeHost::Npm, NativeHost::Hex, NativeHost::RubyGems] {
            let error =
                parse_versions(&route(host, ReleaseChannel::Stable), "{\"oops\":1}").unwrap_err();
            assert!(
                matches!(error, NativeHostClientError::MalformedIndex(..)),
                "{host}: {error}"
            );
        }
    }

    #[test]
    fn hosts_without_a_machine_readable_index_say_why() {
        for host in [
            NativeHost::LuaRocks,
            NativeHost::ConanCenter,
            NativeHost::Opam,
            NativeHost::Zig,
        ] {
            let error =
                version_index_request(&route(host, ReleaseChannel::Stable), "acme").unwrap_err();
            match error {
                NativeHostClientError::IndexUnsupported { reason, .. } => {
                    assert!(!reason.is_empty(), "{host} needs a reason");
                }
                other => panic!("{host}: expected IndexUnsupported, got {other}"),
            }
        }
    }

    // The cases below were all found by probing the live public registries
    // rather than by reasoning about their docs. Each one shipped broken.

    #[test]
    fn clojars_coordinates_use_a_slash_and_maven_central_uses_a_colon() {
        // Leiningen writes `ring/ring-core`; Maven writes `com.google.guava:guava`.
        // The manifest carries whichever the author used, so rejecting `/`
        // made every real Clojars route fail at request construction.
        let request = version_index_request(
            &route(NativeHost::Clojars, ReleaseChannel::Stable),
            "ring/ring-core",
        )
        .unwrap();
        assert_eq!(
            request.url,
            "https://repo.clojars.org/ring/ring-core/maven-metadata.xml"
        );
        let central = version_index_request(
            &route(NativeHost::MavenCentral, ReleaseChannel::Stable),
            "com.google.guava:guava",
        )
        .unwrap();
        assert_eq!(
            central.url,
            "https://repo.maven.apache.org/maven2/com/google/guava/guava/maven-metadata.xml"
        );
        // Neither separator present is still an error.
        assert!(
            version_index_request(&route(NativeHost::Clojars, ReleaseChannel::Stable), "bare")
                .is_err()
        );
    }

    #[test]
    fn cpan_lists_every_release_not_just_the_latest() {
        // `/release/{dist}` returns one record. Reporting that as the complete
        // version list is a wrong answer, not a partial one.
        let request =
            version_index_request(&route(NativeHost::Cpan, ReleaseChannel::Stable), "Try-Tiny")
                .unwrap();
        assert!(request.url.contains("/release/_search"), "{}", request.url);
        assert!(request.url.contains("q=distribution:Try-Tiny"));
        // `fields` is rejected outright by the MetaCPAN API; `_source` is not.
        assert!(request.url.contains("_source=version"));
        assert!(!request.url.contains("fields="));

        let body = r#"{"hits":{"hits":[
            {"_source":{"version":"0.30"}},
            {"_source":{"version":"0.31"}},
            {"_source":{"version":"0.32"}}]}}"#;
        assert_eq!(
            parse_versions(&route(NativeHost::Cpan, ReleaseChannel::Stable), body).unwrap(),
            vec!["0.30", "0.31", "0.32"]
        );
    }

    #[test]
    fn the_powershell_gallery_is_nuget_v2_not_v3() {
        // It has no V3 flat container; the V3 URL shape 404s on every package.
        let psg = version_index_request(
            &route(NativeHost::PowerShellGallery, ReleaseChannel::Stable),
            "Pester",
        )
        .unwrap();
        assert!(
            psg.url.contains("FindPackagesById()?id='Pester'"),
            "{}",
            psg.url
        );
        assert!(!psg.url.contains("index.json"));

        // NuGet proper keeps the V3 flat container, lowercased.
        let nuget = version_index_request(
            &route(NativeHost::NuGet, ReleaseChannel::Stable),
            "Newtonsoft.Json",
        )
        .unwrap();
        assert_eq!(
            nuget.url,
            "https://api.nuget.org/v3-flatcontainer/newtonsoft.json/index.json"
        );

        // OData returns an Atom feed, not JSON.
        let body = "<feed><entry><m:properties><d:Version>3.0.2</d:Version>                    </m:properties></entry><entry><m:properties>                    <d:Version>5.7.1</d:Version></m:properties></entry></feed>";
        assert_eq!(
            parse_versions(
                &route(NativeHost::PowerShellGallery, ReleaseChannel::Stable),
                body
            )
            .unwrap(),
            vec!["3.0.2", "5.7.1"]
        );
    }

    #[test]
    fn the_swift_package_index_is_an_index_not_a_registry() {
        // Every path under its `/api` returns 401 anonymously: it catalogues
        // Git repositories rather than serving SE-0292 releases. Emitting a
        // request that always 401s is worse than saying so.
        let error = version_index_request(
            &route(NativeHost::SwiftPackageIndex, ReleaseChannel::Stable),
            "apple.swift-argument-parser",
        )
        .unwrap_err();
        match error {
            NativeHostClientError::IndexUnsupported { reason, .. } => {
                assert!(reason.contains("tags"), "{reason}");
            }
            other => panic!("expected IndexUnsupported, got {other}"),
        }
    }

    #[test]
    fn base64_matches_rfc_4648_including_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64_encode(b"__token__:hunter2"),
            "X190b2tlbl9fOmh1bnRlcjI="
        );
    }

    #[test]
    fn query_string_secrets_are_redacted_by_key_not_by_value() {
        // A token is opaque, so anything matching on the value itself would
        // fail open.
        assert_eq!(
            redact_url_secret("https://packagist.org/api/update-package?username=a&apiToken=zzz"),
            "https://packagist.org/api/update-package?username=a&apiToken=<redacted>"
        );
    }

    #[test]
    fn credential_lookup_prefers_the_zed_specific_variable() {
        // A repo that publishes to two npm registries needs to override the
        // ecosystem-standard variable without unsetting it.
        assert_eq!(
            credential_env_vars(NativeHost::Npm).first(),
            Some(&"ZED_NPM_TOKEN")
        );
        assert!(credential_env_vars(NativeHost::Npm).contains(&"NPM_TOKEN"));
        assert!(
            credential_env_vars(NativeHost::Vcs).is_empty(),
            "a VCS route has no registry credential to leak"
        );
    }
}
