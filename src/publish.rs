use anyhow::{Result, bail};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::publish::PublishRegistry;

/// Resolve the registry subset requested on the command line against what the
/// package author declared. An empty request means every declared registry;
/// manifests written before registry fan-out declared nothing and therefore
/// still resolve to Zed only.
pub fn selected_registries(
    manifest: &Manifest,
    requested: &[PublishRegistry],
) -> Result<Vec<PublishRegistry>> {
    let declared = manifest.publish_registries();
    if requested.is_empty() {
        return Ok(declared);
    }
    for registry in requested {
        if !declared.contains(registry) {
            bail!(
                "{} does not declare `{registry}` in its publish registries (declared: {})",
                manifest.full_name(),
                declared
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let mut selected = requested.to_vec();
    selected.sort();
    selected.dedup();
    Ok(selected)
}

/// Human-readable destination used in dry-run plans. Endpoint overrides win;
/// otherwise the output identifies the standard service without manufacturing
/// project IDs, credentials, or repository paths that are provider-specific.
pub fn destination_label(
    manifest: &Manifest,
    registry: PublishRegistry,
    zed_registry: &str,
) -> String {
    if let Some(url) = manifest.publish_registry_url(registry) {
        return url.trim_end_matches('/').to_string();
    }
    let format = manifest.publish.format.as_deref().unwrap_or("zpkg");
    match registry {
        PublishRegistry::Zed => zed_registry.trim_end_matches('/').to_string(),
        PublishRegistry::Native => native_registry_label(format).to_string(),
        PublishRegistry::GithubPackages => github_registry_label(format).to_string(),
        PublishRegistry::GitlabPackages => {
            format!("GitLab Package Registry ({format})")
        }
        PublishRegistry::BitbucketPackages => {
            format!("Bitbucket Packages ({format})")
        }
    }
}

fn native_registry_label(format: &str) -> &'static str {
    match format {
        "npm" => "https://registry.npmjs.org",
        "cargo" => "https://crates.io",
        "pypi" => "https://pypi.org",
        "rubygems" => "https://rubygems.org",
        "maven" | "scala" => "https://central.sonatype.com",
        "nuget" => "https://api.nuget.org/v3/index.json",
        "composer" => "https://packagist.org",
        "dart" => "https://pub.dev",
        "hex" | "gleam" => "https://hex.pm",
        "opam" => "https://opam.ocaml.org",
        "clojure" => "https://clojars.org",
        "haskell" => "https://hackage.haskell.org",
        "julia" => "https://juliahub.com",
        "r" => "https://cran.r-project.org",
        "nim" => "https://nimble.directory",
        "crystal" => "https://shardbox.org",
        "lua" => "https://luarocks.org",
        "powershell" => "https://www.powershellgallery.com",
        // Go and Swift package discovery is anchored in VCS tags rather than
        // an uploaded package blob. Unknown future ecosystems deliberately
        // remain descriptive instead of being rejected by the native route.
        "go" | "swift" => "source VCS tag",
        _ => "native package registry",
    }
}

fn github_registry_label(format: &str) -> &'static str {
    match format {
        "npm" => "https://npm.pkg.github.com",
        "maven" => "https://maven.pkg.github.com",
        "nuget" => "https://nuget.pkg.github.com",
        "rubygems" => "https://rubygems.pkg.github.com",
        "container" => "https://ghcr.io",
        _ => "GitHub Packages",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = r#"
[package]
org = "acme"
name = "sdk"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/sdk"

[publish]
format = "npm"
registries = ["zed", "native", "github-packages"]
"#;

    #[test]
    fn no_filter_selects_every_declared_registry() {
        let manifest = Manifest::parse(PACKAGE).unwrap();
        assert_eq!(
            selected_registries(&manifest, &[]).unwrap(),
            vec![
                PublishRegistry::Zed,
                PublishRegistry::Native,
                PublishRegistry::GithubPackages,
            ]
        );
    }

    #[test]
    fn filters_are_deduplicated_and_must_be_declared() {
        let manifest = Manifest::parse(PACKAGE).unwrap();
        assert_eq!(
            selected_registries(
                &manifest,
                &[PublishRegistry::Native, PublishRegistry::Native]
            )
            .unwrap(),
            vec![PublishRegistry::Native]
        );
        let error = selected_registries(&manifest, &[PublishRegistry::GitlabPackages])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not declare `gitlab-packages`"),
            "{error}"
        );
    }

    #[test]
    fn destination_labels_never_contain_credentials() {
        let manifest = Manifest::parse(PACKAGE).unwrap();
        assert_eq!(
            destination_label(
                &manifest,
                PublishRegistry::GithubPackages,
                "https://registry.zpkg.tech"
            ),
            "https://npm.pkg.github.com"
        );
        assert_eq!(
            destination_label(
                &manifest,
                PublishRegistry::Native,
                "https://registry.zpkg.tech"
            ),
            "https://registry.npmjs.org"
        );
    }
}
