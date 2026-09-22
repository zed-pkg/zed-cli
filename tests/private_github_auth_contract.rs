//! Source-level regression contract for private GitHub package authentication.
//!
//! These checks intentionally do not need a live secret. They lock the reviewed
//! transport boundary in place so private-package support cannot regress into
//! tokenized URLs, argv credentials, or anonymous-only tag archives.

const FALLBACK: &str = include_str!("../src/source_fallback.rs");
const FETCH: &str = include_str!("../src/fetch.rs");

#[test]
fn private_github_token_has_one_canonical_environment_boundary() {
    assert!(FALLBACK.contains("env_nonempty(\"ZED_PKG_GITHUB_TOKEN\")"));
    assert!(FALLBACK.contains("env_nonempty(\"GITHUB_TOKEN\")"));
    assert!(FALLBACK.contains("env_nonempty(\"GH_TOKEN\")"));

    let zed = FALLBACK.find("env_nonempty(\"ZED_PKG_GITHUB_TOKEN\")").unwrap();
    let github = FALLBACK.find("env_nonempty(\"GITHUB_TOKEN\")").unwrap();
    let gh = FALLBACK.find("env_nonempty(\"GH_TOKEN\")").unwrap();
    assert!(zed < github && github < gh, "token precedence changed");
}

#[test]
fn private_tag_archives_use_authenticated_github_rest() {
    assert!(FALLBACK.contains("github_api_tarball_url(identity, &version.vcs_tag)"));
    assert!(FALLBACK.contains("download_url(&self.client, &api_url, raw.path(), 0, Some(token))"));
    assert!(FALLBACK.contains("https://api.github.com/repos/{}/{}/tarball/{tag}"));
}

#[test]
fn fallback_download_bearer_is_scoped_to_api_github_com() {
    let guard = "matches!(parsed.host_str(), Some(\"api.github.com\"))";
    assert!(FALLBACK.contains(guard), "GitHub bearer host guard disappeared");
    assert!(FALLBACK.contains("request = request.bearer_auth(token)"));
    assert!(FALLBACK.contains("reqwest drops Authorization on the cross-host redirect"));
}

#[test]
fn frozen_sources_forbid_embedded_credentials() {
    assert!(FETCH.contains("frozen registry sources may not embed credentials"));
    assert!(FETCH.contains("use an explicit secret-delivery mechanism"));
    assert!(FETCH.contains("!url.username().is_empty()"));
    assert!(FETCH.contains("url.password().is_some()"));
}

#[test]
fn private_repo_failures_name_the_environment_fix_without_echoing_a_token() {
    assert!(FALLBACK.contains(
        "private repositories need ZED_PKG_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN"
    ));
    for forbidden in ["{token}@github.com", "?token={token}", "access_token={token}"] {
        assert!(!FALLBACK.contains(forbidden), "credentialized URL pattern returned: {forbidden}");
    }
}
