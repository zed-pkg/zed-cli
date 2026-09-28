//! Fail-closed classification for artifacts returned by the audited public
//! native-registry edge fallback.
//!
//! Filesystem normalization is more permissive than canonical Zed extraction,
//! so callers must never select it based on archive shape alone. This module
//! recognizes only exact HTTPS artifact hosts used by protocol-audited native
//! adapters. Hostname suffixes, credentials, explicit ports, query strings,
//! and fragments are rejected.

use zed_interfaces::registry::VersionMetadata;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeArtifactSource {
    Npm,
    CratesIo,
    PyPi,
    MavenCentral,
    NuGet,
    GoProxy,
    Hackage,
    Clojars,
    Cpan,
    Cran,
    Jsr,
    PackagistGithub,
}

pub fn audited_native_source(metadata: &VersionMetadata) -> Option<NativeArtifactSource> {
    audited_native_download_url(&metadata.download_url)
}

pub fn audited_native_download_url(raw_url: &str) -> Option<NativeArtifactSource> {
    let url = reqwest::Url::parse(raw_url).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }

    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    match host.as_str() {
        "registry.npmjs.org" if path.ends_with(".tgz") => Some(NativeArtifactSource::Npm),
        "static.crates.io" if path.starts_with("/crates/") && path.ends_with(".crate") => {
            Some(NativeArtifactSource::CratesIo)
        }
        "crates.io" if path.starts_with("/api/v1/crates/") && path.ends_with("/download") => {
            Some(NativeArtifactSource::CratesIo)
        }
        "files.pythonhosted.org"
            if path.starts_with("/packages/")
                && (path.ends_with(".tar.gz") || path.ends_with(".zip")) =>
        {
            Some(NativeArtifactSource::PyPi)
        }
        "repo1.maven.org" if path.starts_with("/maven2/") && path.ends_with(".jar") => {
            Some(NativeArtifactSource::MavenCentral)
        }
        "api.nuget.org"
            if path.starts_with("/v3-flatcontainer/") && path.ends_with(".nupkg") =>
        {
            Some(NativeArtifactSource::NuGet)
        }
        "proxy.golang.org" if path.contains("/@v/") && path.ends_with(".zip") => {
            Some(NativeArtifactSource::GoProxy)
        }
        "hackage.haskell.org" if path.starts_with("/package/") && path.ends_with(".tar.gz") => {
            Some(NativeArtifactSource::Hackage)
        }
        "repo.clojars.org" if path.ends_with(".jar") => Some(NativeArtifactSource::Clojars),
        "cpan.metacpan.org"
            if path.starts_with("/authors/id/") && path.ends_with(".tar.gz") =>
        {
            Some(NativeArtifactSource::Cpan)
        }
        "cran.r-project.org"
            if path.starts_with("/src/contrib/") && path.ends_with(".tar.gz") =>
        {
            Some(NativeArtifactSource::Cran)
        }
        "npm.jsr.io" if path.ends_with(".tgz") => Some(NativeArtifactSource::Jsr),
        "codeload.github.com" if is_packagist_codeload_path(path) => {
            Some(NativeArtifactSource::PackagistGithub)
        }
        _ => None,
    }
}

fn is_packagist_codeload_path(path: &str) -> bool {
    let segments = path.split('/').filter(|part| !part.is_empty()).collect::<Vec<_>>();
    if segments.len() != 4 || segments[2] != "legacy.zip" {
        return false;
    }
    is_safe_github_component(segments[0])
        && is_safe_github_component(segments[1])
        && is_git_commit(segments[3])
}

fn is_safe_github_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.contains("..")
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

fn is_git_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_protocol_audited_native_artifact_hosts() {
        let cases = [
            ("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz", NativeArtifactSource::Npm),
            ("https://static.crates.io/crates/serde/serde-1.0.0.crate", NativeArtifactSource::CratesIo),
            ("https://files.pythonhosted.org/packages/aa/bb/hash/requests-2.32.5.tar.gz", NativeArtifactSource::PyPi),
            ("https://repo1.maven.org/maven2/com/google/guava/guava/33.4.8/guava-33.4.8.jar", NativeArtifactSource::MavenCentral),
            ("https://api.nuget.org/v3-flatcontainer/newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg", NativeArtifactSource::NuGet),
            ("https://proxy.golang.org/github.com/owner/repo/@v/v1.2.3.zip", NativeArtifactSource::GoProxy),
            ("https://hackage.haskell.org/package/aeson-2.2.3.0/aeson-2.2.3.0.tar.gz", NativeArtifactSource::Hackage),
            ("https://repo.clojars.org/org/example/demo/1.0.0/demo-1.0.0.jar", NativeArtifactSource::Clojars),
            ("https://cpan.metacpan.org/authors/id/A/AB/ABC/Demo-1.0.tar.gz", NativeArtifactSource::Cpan),
            ("https://cran.r-project.org/src/contrib/jsonlite_2.0.0.tar.gz", NativeArtifactSource::Cran),
            ("https://npm.jsr.io/~/123/@jsr/scope__name/1.0.0.tgz", NativeArtifactSource::Jsr),
            (&format!("https://codeload.github.com/acme/widget/legacy.zip/{}", "a".repeat(40)), NativeArtifactSource::PackagistGithub),
        ];
        for (url, expected) in cases {
            assert_eq!(audited_native_download_url(url), Some(expected), "{url}");
        }
    }

    #[test]
    fn rejects_host_confusion_credentials_ports_queries_and_wrong_paths() {
        for url in [
            "http://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "https://registry.npmjs.org.evil.test/lodash/-/lodash-4.17.21.tgz",
            "https://user:pass@registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "https://registry.npmjs.org:443/lodash/-/lodash-4.17.21.tgz",
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz?x=1",
            "https://repo1.maven.org/maven2/com/acme/demo/1.0.0/demo-1.0.0.pom",
            "https://proxy.golang.org/github.com/owner/repo/@v/v1.2.3.info",
            "https://cran.r-project.org/web/packages/jsonlite/DESCRIPTION",
            "https://codeload.github.com/acme/widget/legacy.zip/not-a-commit",
            "https://github.com/acme/widget/archive/refs/tags/v1.0.0.zip",
        ] {
            assert_eq!(audited_native_download_url(url), None, "{url}");
        }
    }
}
