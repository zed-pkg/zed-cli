use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::{Manifest, NativeRegistry};
use zed_interfaces::native_host::{ChannelRoute, RegistryProtocol, ReleaseChannel};

use crate::native_host_client::{self, NativeHostClientError, RegistryLimits, RegistryRequest};

use crate::config::read_manifest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleasePlan {
    pub release_set: String,
    pub source: ReleaseSource,
    pub zed: Vec<ZedReleaseArtifact>,
    pub native: Vec<NativeReleaseArtifact>,
    pub forge: Vec<ForgeReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseSource {
    pub package: String,
    pub version: String,
    pub vcs_tag: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZedReleaseArtifact {
    pub target: Option<String>,
    pub package: String,
    pub version: String,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeReleaseArtifact {
    pub target: String,
    pub registry: String,
    pub package: String,
    /// The release set's version — the stable version every artifact in the
    /// set shares, regardless of channel.
    pub version: String,
    pub vcs_tag: String,
    pub dir: String,
    /// The resolved destination for this release's channel: the version as the
    /// host will store it, its dist-tag, and the endpoint it goes to. Spelled
    /// out per route because no two ecosystems agree on any of the three.
    pub channel: ChannelRoute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ForgeReleaseArtifact {
    pub target: String,
    pub registry: String,
    pub format: String,
    pub package: String,
    pub version: String,
    pub vcs_tag: String,
    pub dir: String,
}

#[derive(Debug, Deserialize)]
struct NpmPackageManifest {
    name: String,
    version: String,
    #[serde(default)]
    private: bool,
}

#[derive(Debug, Deserialize)]
struct CargoPackageManifest {
    package: CargoPackageSection,
}

#[derive(Debug, Deserialize)]
struct CargoPackageSection {
    name: String,
    version: String,
    #[serde(default)]
    publish: Option<CargoPublishPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CargoPublishPolicy {
    Enabled(bool),
    Registries(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct PythonProjectManifest {
    project: Option<PythonProjectSection>,
}

#[derive(Debug, Deserialize)]
struct PythonProjectSection {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    dynamic: Vec<String>,
}

fn normalize_pypi_name(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    for byte in value.bytes() {
        if matches!(byte, b'.' | b'_' | b'-') {
            separator = true;
            continue;
        }
        if separator && !normalized.is_empty() {
            normalized.push('-');
        }
        separator = false;
        normalized.push((byte as char).to_ascii_lowercase());
    }
    normalized
}

/// Build the deterministic release set.
///
/// `channel` and `iteration` select the release track. They are a plan input
/// rather than a manifest field because the same reviewed commit is what
/// becomes `1.4.0-rc.1`, then `1.4.0-rc.2`, then `1.4.0` — the source does not
/// change between them, only the destination does.
pub fn build_plan(
    manifest: &Manifest,
    channel: ReleaseChannel,
    iteration: u32,
) -> Result<ReleasePlan> {
    let source_package = manifest.full_name();
    let version = manifest.package.version.clone();
    let vcs_tag = manifest.vcs_tag();

    let zed = if manifest.targets.is_empty() {
        vec![ZedReleaseArtifact {
            target: None,
            package: source_package.clone(),
            version: version.clone(),
            dir: ".".to_string(),
        }]
    } else {
        manifest
            .target_package_names()
            .into_iter()
            .map(|(target, package_name)| {
                let section = manifest
                    .targets
                    .get(&target)
                    .expect("target_package_names only returns declared targets");
                ZedReleaseArtifact {
                    target: Some(target),
                    package: format!("{}/{}", manifest.package.org, package_name),
                    version: version.clone(),
                    dir: section.dir.clone(),
                }
            })
            .collect()
    };

    let native = manifest
        .native_release_routes()
        .into_iter()
        .map(|route| {
            // A single-language package declares its route at
            // `[publish.native]`; a polyglot one declares it per target.
            let section = manifest
                .targets
                .get(&route.target)
                .and_then(|target| target.native.as_ref())
                .or(manifest.publish.native.as_ref());
            // A route may pin its own track (a client generated from an
            // unstable API surface); an explicit `--channel` overrides it.
            let declared = section.map(|native| native.channel).unwrap_or_default();
            let effective = if channel.is_default() {
                declared
            } else {
                channel
            };
            let resolved = route
                .registry
                .host()
                .channel_route(&version, effective, iteration)
                .map_err(|error| anyhow!("target `{}`: {error}", route.target))?;
            // Re-render the tag against the channel version rather than
            // reusing the stable one. For a host that publishes by VCS tag —
            // Go's proxy, Packagist, Zig, a plain remote — the tag *is* the
            // publication, so a candidate tagged `v0.1.0` would either
            // collide with the eventual release or simply never exist.
            let vcs_tag = section
                .and_then(|native| native.tag_format.clone())
                .unwrap_or_else(|| manifest.publish.tag_format.clone())
                .replace("{version}", &resolved.version);
            Ok(NativeReleaseArtifact {
                target: route.target,
                registry: route.registry.as_str().to_string(),
                package: route.package,
                version: version.clone(),
                vcs_tag,
                dir: route.dir,
                channel: resolved,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let forge = manifest
        .forge_release_routes()
        .into_iter()
        .map(|route| ForgeReleaseArtifact {
            target: route.target,
            registry: route.registry.as_str().to_string(),
            format: route.format.as_str().to_string(),
            package: route.package,
            version: version.clone(),
            vcs_tag: route.vcs_tag,
            dir: route.dir,
        })
        .collect();

    Ok(ReleasePlan {
        release_set: format!("{source_package}@{version}#{vcs_tag}"),
        source: ReleaseSource {
            package: source_package,
            version,
            vcs_tag,
            repository: manifest.package.repository.url.clone(),
        },
        zed,
        native,
        forge,
    })
}

/// Cross-check every native route against the native manifest in its target
/// root, returning the routes that could not be checked and why.
///
/// A skip is returned rather than swallowed. "Nothing was verified" and
/// "everything matched" must not look the same to a release operator, and the
/// ecosystems below genuinely cannot be checked: their build definitions are
/// executable code (Groovy, Kotlin, Elixir, Erlang), not data.
pub fn validate_native_manifests(project: &Path, manifest: &Manifest) -> Result<Vec<String>> {
    let mut unchecked = Vec::new();
    for route in manifest.native_release_routes() {
        let target_root = project.join(&route.dir);
        match route.registry {
            NativeRegistry::Npm => {
                validate_npm_manifest(
                    &target_root.join("package.json"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::CratesIo => {
                validate_cargo_manifest(
                    &target_root.join("Cargo.toml"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::PubDev => {
                validate_pubspec_manifest(
                    &target_root.join("pubspec.yaml"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::PyPi => {
                validate_pyproject_manifest(
                    &target_root.join("pyproject.toml"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::MavenCentral | NativeRegistry::Clojars => {
                let pom = target_root.join("pom.xml");
                if pom.exists() {
                    validate_maven_manifest(
                        &pom,
                        &route.target,
                        &route.package,
                        &manifest.package.version,
                    )?;
                } else if let Some(build) = jvm_build_file(&target_root) {
                    // Gradle, sbt, Leiningen, and deps.edn hold coordinates in
                    // executable build scripts, so there is nothing to parse
                    // without running them. Demanding `pom.xml` instead would
                    // reject the majority of modern JVM projects outright.
                    unchecked.push(format!(
                        "{} [{}]: {build} declares its coordinates in build code; \
                         package identity and version are not cross-checked",
                        route.target,
                        route.registry.as_str()
                    ));
                } else {
                    bail!(
                        "native JVM target `{}` has no pom.xml or recognized build file in {}",
                        route.target,
                        target_root.display()
                    );
                }
            }
            NativeRegistry::RubyGems => {
                validate_rubygems_manifest(
                    &target_root,
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::NuGet => {
                validate_nuget_manifest(
                    &target_root,
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::Packagist => {
                validate_composer_manifest(
                    &target_root.join("composer.json"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
            NativeRegistry::GoModules => {
                validate_go_manifest(&target_root.join("go.mod"), &route.target, &route.package)?;
            }
            NativeRegistry::Hex => {
                let gleam = target_root.join("gleam.toml");
                if gleam.exists() {
                    validate_gleam_manifest(
                        &gleam,
                        &route.target,
                        &route.package,
                        &manifest.package.version,
                    )?;
                } else if let Some(build) = hex_build_file(&target_root) {
                    unchecked.push(format!(
                        "{} [hex]: {build} is executable code; package identity \
                         and version are not cross-checked",
                        route.target
                    ));
                } else {
                    bail!(
                        "native Hex target `{}` has no gleam.toml, mix.exs, or rebar.config in {}",
                        route.target,
                        target_root.display()
                    );
                }
            }
            // Routes to the remaining hosts are planned and published, but
            // their native manifest is not yet cross-checked against the
            // route: each format needs its own parser (`.cabal`, `mix.exs`,
            // `*.rockspec`, `DESCRIPTION`, `Package.swift`, …) and a wrong one
            // would reject valid releases.
            //
            // Skipping is the honest gap, and the narrow one: manifest parsing
            // catches an invalid package identity before this runs, and
            // `zed release publish` still refuses to overwrite a published
            // version with different content.
            other => unchecked.push(format!(
                "{} [{}]: no native-manifest parser yet; the route is planned \
                 and published but its own manifest is not compared against it",
                route.target,
                other.as_str()
            )),
        }
    }
    Ok(unchecked)
}

/// The JVM build file present in `root`, if any. Ordered so the most common
/// build system is named first in diagnostics.
fn jvm_build_file(root: &Path) -> Option<&'static str> {
    [
        "build.gradle.kts",
        "build.gradle",
        "build.sbt",
        "project.clj",
        "deps.edn",
    ]
    .into_iter()
    .find(|name| root.join(name).exists())
}

/// The BEAM build file present in `root`, if any.
fn hex_build_file(root: &Path) -> Option<&'static str> {
    ["mix.exs", "rebar.config"]
        .into_iter()
        .find(|name| root.join(name).exists())
}

#[derive(Debug, Deserialize)]
struct GleamPackageManifest {
    name: String,
    version: String,
}

/// Cross-check a `gleam.toml` against its route.
///
/// Gleam is the one Hex ecosystem whose manifest is data rather than code, so
/// it is the one that can be checked. Names are snake_case on Hex, which is a
/// different spelling from the zed package name — the route carries the Hex
/// name and this compares against that, not against `[package] name`.
fn validate_gleam_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native Hex target `{target}` has no {}", path.display()))?;
    let parsed: GleamPackageManifest = toml::from_str(&text)
        .with_context(|| format!("invalid gleam manifest {}", path.display()))?;
    if parsed.name != expected_name {
        bail!(
            "native Hex target `{target}` declares package `{expected_name}`, but {} names `{}`",
            path.display(),
            parsed.name
        );
    }
    if parsed.version != expected_version {
        bail!(
            "native Hex target `{target}` must use release-set version `{expected_version}`, but {} uses `{}`",
            path.display(),
            parsed.version
        );
    }
    Ok(())
}

fn validate_npm_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native npm target `{target}` has no {}", path.display()))?;
    let package: NpmPackageManifest = serde_json::from_str(&text)
        .with_context(|| format!("invalid npm package manifest {}", path.display()))?;
    if package.name != expected_name {
        bail!(
            "native npm target `{target}` declares package `{expected_name}`, but {} names `{}`",
            path.display(),
            package.name
        );
    }
    if package.version != expected_version {
        bail!(
            "native npm target `{target}` must use release-set version `{expected_version}`, but {} uses `{}`",
            path.display(),
            package.version
        );
    }
    if package.private {
        bail!(
            "native npm target `{target}` cannot be released because {} sets `private: true`",
            path.display()
        );
    }
    Ok(())
}

fn strip_yaml_comment(value: &str) -> &str {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if double => escaped = true,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'#' if !single && !double => return value[..index].trim_end(),
            _ => {}
        }
    }
    value.trim_end()
}

fn decode_yaml_scalar(value: &str, path: &Path, key: &str) -> Result<String> {
    let value = strip_yaml_comment(value).trim();
    if value.is_empty() {
        bail!("{} has an empty `{key}` value", path.display());
    }
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .with_context(|| format!("invalid quoted `{key}` in {}", path.display()));
    }
    if value.starts_with('\'') {
        if !value.ends_with('\'') || value.len() < 2 {
            bail!("invalid quoted `{key}` in {}", path.display());
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    Ok(value.to_string())
}

fn pubspec_scalar(text: &str, path: &Path, key: &str) -> Result<Option<String>> {
    let prefix = format!("{key}:");
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix(&prefix) {
            return decode_yaml_scalar(value, path, key).map(Some);
        }
    }
    Ok(None)
}

fn validate_pubspec_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native pub.dev target `{target}` has no {}", path.display()))?;
    let name = pubspec_scalar(&text, path, "name")?
        .with_context(|| format!("{} has no top-level `name`", path.display()))?;
    let version = pubspec_scalar(&text, path, "version")?
        .with_context(|| format!("{} has no top-level `version`", path.display()))?;
    if name != expected_name {
        bail!(
            "native pub.dev target `{target}` declares package `{expected_name}`, but {} names `{name}`",
            path.display()
        );
    }
    if version != expected_version {
        bail!(
            "native pub.dev target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    if let Some(destination) = pubspec_scalar(&text, path, "publish_to")? {
        bail!(
            "native pub.dev target `{target}` cannot be released because {} sets `publish_to: {destination}`; pub.dev packages must omit `publish_to`",
            path.display()
        );
    }
    Ok(())
}

fn validate_pyproject_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native PyPI target `{target}` has no {}", path.display()))?;
    let manifest: PythonProjectManifest = toml::from_str(&text)
        .with_context(|| format!("invalid Python project manifest {}", path.display()))?;
    let project = manifest
        .project
        .with_context(|| format!("{} has no `[project]` table", path.display()))?;
    if normalize_pypi_name(&project.name) != normalize_pypi_name(expected_name) {
        bail!(
            "native PyPI target `{target}` declares package `{expected_name}`, but {} names `{}`",
            path.display(),
            project.name
        );
    }
    if project.dynamic.iter().any(|field| field == "version") {
        bail!(
            "native PyPI target `{target}` cannot join a coordinated release while {} declares `version` as dynamic",
            path.display()
        );
    }
    let version = project.version.with_context(|| {
        format!(
            "native PyPI target `{target}` requires a static `[project].version` in {}",
            path.display()
        )
    })?;
    if version != expected_version {
        bail!(
            "native PyPI target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    Ok(())
}

fn validate_cargo_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "native crates.io target `{target}` has no {}",
            path.display()
        )
    })?;
    let manifest: CargoPackageManifest = toml::from_str(&text)
        .with_context(|| format!("invalid Cargo package manifest {}", path.display()))?;
    if manifest.package.name != expected_name {
        bail!(
            "native crates.io target `{target}` declares package `{expected_name}`, but {} names `{}`",
            path.display(),
            manifest.package.name
        );
    }
    if manifest.package.version != expected_version {
        bail!(
            "native crates.io target `{target}` must use release-set version `{expected_version}`, but {} uses `{}`",
            path.display(),
            manifest.package.version
        );
    }
    match manifest.package.publish {
        Some(CargoPublishPolicy::Enabled(false)) => bail!(
            "native crates.io target `{target}` cannot be released because {} sets `publish = false`",
            path.display()
        ),
        Some(CargoPublishPolicy::Registries(ref registries))
            if !registries.iter().any(|registry| registry == "crates-io") =>
        {
            bail!(
                "native crates.io target `{target}` excludes `crates-io` in {}",
                path.display()
            );
        }
        _ => {}
    }
    Ok(())
}

fn xml_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

fn without_xml_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = text.find(&open) else {
        return text.to_string();
    };
    let Some(relative_end) = text[start + open.len()..].find(&close) else {
        return text.to_string();
    };
    let end = start + open.len() + relative_end + close.len();
    format!("{}{}", &text[..start], &text[end..])
}

fn validate_maven_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native Maven target `{target}` has no {}", path.display()))?;
    // Parent coordinates describe the build parent, not the artifact being
    // published. Strip that block before reading the project coordinates.
    let project = without_xml_block(&text, "parent");
    let group = xml_value(&project, "groupId")
        .with_context(|| format!("{} has no project `<groupId>`", path.display()))?;
    let artifact = xml_value(&project, "artifactId")
        .with_context(|| format!("{} has no project `<artifactId>`", path.display()))?;
    let version = xml_value(&project, "version")
        .with_context(|| format!("{} has no project `<version>`", path.display()))?;
    let actual = format!("{group}:{artifact}");
    if actual != expected_name {
        bail!(
            "native Maven target `{target}` declares package `{expected_name}`, but {} names `{actual}`",
            path.display()
        );
    }
    if version != expected_version {
        bail!(
            "native Maven target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    Ok(())
}

fn find_single_manifest(
    root: &Path,
    extension: &str,
    label: &str,
    target: &str,
) -> Result<PathBuf> {
    let mut paths = fs::read_dir(root)
        .with_context(|| format!("native {label} target `{target}` has no {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .collect::<Vec<_>>();
    paths.sort();
    match paths.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!(
            "native {label} target `{target}` has no *.{extension} manifest in {}",
            root.display()
        ),
        _ => bail!(
            "native {label} target `{target}` has multiple *.{extension} manifests in {}; route must be unambiguous",
            root.display()
        ),
    }
}

fn quoted_assignment(text: &str, field: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        if !left.trim_end().ends_with(field) {
            continue;
        }
        let value = right.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            return Some(value[1..value.len() - 1].to_string());
        }
    }
    None
}

fn validate_rubygems_manifest(
    root: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let path = find_single_manifest(root, "gemspec", "RubyGems", target)?;
    let text = fs::read_to_string(&path)?;
    let name = quoted_assignment(&text, ".name")
        .with_context(|| format!("{} has no literal gem name assignment", path.display()))?;
    let version = quoted_assignment(&text, ".version")
        .with_context(|| format!("{} has no literal gem version assignment", path.display()))?;
    if name != expected_name {
        bail!(
            "native RubyGems target `{target}` declares package `{expected_name}`, but {} names `{name}`",
            path.display()
        );
    }
    if version != expected_version {
        bail!(
            "native RubyGems target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    Ok(())
}

fn validate_nuget_manifest(
    root: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let path = find_single_manifest(root, "csproj", "NuGet", target)?;
    let text = fs::read_to_string(&path)?;
    let name = xml_value(&text, "PackageId")
        .or_else(|| xml_value(&text, "AssemblyName"))
        .or_else(|| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_string)
        })
        .context("NuGet project has no package identity")?;
    let version = xml_value(&text, "PackageVersion")
        .or_else(|| xml_value(&text, "Version"))
        .with_context(|| format!("{} has no `<Version>`", path.display()))?;
    if !name.eq_ignore_ascii_case(expected_name) {
        bail!(
            "native NuGet target `{target}` declares package `{expected_name}`, but {} names `{name}`",
            path.display()
        );
    }
    if version != expected_version {
        bail!(
            "native NuGet target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    Ok(())
}

fn validate_composer_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "native Packagist target `{target}` has no {}",
            path.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid Composer manifest {}", path.display()))?;
    let name = manifest
        .get("name")
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("{} has no string `name`", path.display()))?;
    if name != expected_name {
        bail!(
            "native Packagist target `{target}` declares package `{expected_name}`, but {} names `{name}`",
            path.display()
        );
    }
    if let Some(version) = manifest.get("version").and_then(serde_json::Value::as_str)
        && version != expected_version
    {
        bail!(
            "native Packagist target `{target}` must use release-set version `{expected_version}`, but {} uses `{version}`",
            path.display()
        );
    }
    Ok(())
}

fn validate_go_manifest(path: &Path, target: &str, expected_name: &str) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("native Go target `{target}` has no {}", path.display()))?;
    let module = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("module ").map(str::trim))
        .with_context(|| format!("{} has no `module` directive", path.display()))?;
    if module != expected_name {
        bail!(
            "native Go target `{target}` declares package `{expected_name}`, but {} names `{module}`",
            path.display()
        );
    }
    Ok(())
}

pub fn render_human(plan: &ReleasePlan) -> String {
    let mut output = String::new();
    output.push_str(&format!("release set {}\n", plan.release_set));
    output.push_str(&format!(
        "source: {} @ {} ({})\n",
        plan.source.repository, plan.source.vcs_tag, plan.source.package
    ));
    output.push_str("zed artifacts:\n");
    for artifact in &plan.zed {
        let target = artifact.target.as_deref().unwrap_or("repository");
        output.push_str(&format!(
            "  - {}/{} <- {} [target: {}]\n",
            artifact.package, artifact.version, artifact.dir, target
        ));
    }
    output.push_str("native artifacts:\n");
    if plan.native.is_empty() {
        output.push_str("  - none declared\n");
    } else {
        for artifact in &plan.native {
            // Print the version the host will actually store, not the release
            // set's stable version: `--channel rc` previously rendered a line
            // indistinguishable from a stable plan, which is the one place a
            // reader would notice they were about to ship a candidate.
            output.push_str(&format!(
                "  - {} {}@{} <- {} [target: {}, tag: {}",
                artifact.registry,
                artifact.package,
                artifact.channel.version,
                artifact.dir,
                artifact.target,
                artifact.vcs_tag
            ));
            if !artifact.channel.channel.is_default() {
                output.push_str(&format!(", channel: {}", artifact.channel.channel));
            }
            if let Some(tag) = &artifact.channel.dist_tag {
                output.push_str(&format!(", dist-tag: {tag}"));
            }
            if artifact.channel.mutable {
                output.push_str(", mutable");
            }
            if artifact.channel.moderated {
                output.push_str(", reviewed before it lands");
            }
            output.push_str("]\n");
        }
    }
    output.push_str("forge package mirrors:\n");
    if plan.forge.is_empty() {
        output.push_str("  - none declared\n");
    } else {
        for artifact in &plan.forge {
            output.push_str(&format!(
                "  - {} via {} {}@{} <- {} [target: {}, tag: {}]\n",
                artifact.registry,
                artifact.format,
                artifact.package,
                artifact.version,
                artifact.dir,
                artifact.target,
                artifact.vcs_tag
            ));
        }
    }
    output
}

pub fn plan(project: &Path, json: bool, channel: ReleaseChannel, iteration: u32) -> Result<()> {
    let manifest = read_manifest(project)?;
    let unchecked = validate_native_manifests(project, &manifest)?;
    let plan = build_plan(&manifest, channel, iteration)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", render_human(&plan));
        // "Nothing was verified" and "everything matched" must not look the
        // same to whoever is about to cut a release.
        for note in &unchecked {
            println!("unchecked {note}");
        }
    }
    Ok(())
}

/// Upload every native route in the release set to its channel's endpoint.
///
/// `dry_run` prints the exact request each route would send — verb, URL,
/// headers, body shape — with credentials redacted, and sends nothing. That is
/// the same construction path a real run takes, so a dry run that looks right
/// is evidence the real one will be.
///
/// Routes whose host publishes by VCS tag, or needs a request sequence zed
/// cannot yet drive, are reported and skipped rather than failing the whole
/// set: a polyglot release that reaches eight of nine registries should say so
/// and name the ninth.
pub fn publish(
    project: &Path,
    channel: ReleaseChannel,
    iteration: u32,
    dry_run: bool,
    only_target: Option<&str>,
) -> Result<()> {
    let manifest = read_manifest(project)?;
    let unchecked = validate_native_manifests(project, &manifest)?;
    for note in &unchecked {
        println!("unchecked {note}");
    }
    let plan = build_plan(&manifest, channel, iteration)?;

    let mut published = 0usize;
    let mut selected = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for artifact in &plan.native {
        if only_target.is_some_and(|target| target != artifact.target) {
            continue;
        }
        selected += 1;
        let host = artifact.channel.host;
        if artifact.channel.moderated {
            println!(
                "note  {} [{}] is reviewed before it lands; zed submits, a human accepts",
                artifact.package, host
            );
        }
        let credential = native_host_client::credential_for(host);
        let payload = native_artifact_path(project, artifact);

        // A multi-request publish has no single request to preview: step 2's
        // URL is only known once step 1 has answered. Building one here would
        // hit `publish_request`'s `MultiStepPublish` guard and report the
        // route as skipped, which is exactly wrong now that the sequence is
        // implemented.
        if matches!(
            artifact.channel.protocol,
            RegistryProtocol::PubDev | RegistryProtocol::MavenCentralPortal
        ) {
            println!(
                "route {} -> {host} {}@{} [{}, multi-request]",
                artifact.target,
                artifact.package,
                artifact.channel.version,
                artifact.channel.channel
            );
            if dry_run {
                println!(
                    "      first request is authenticated to {host}; the rest are \
                     derived from its response"
                );
                continue;
            }
            if !payload.exists() {
                skipped.push(format!(
                    "{} [{host}]: no built artifact at {}",
                    artifact.target,
                    payload.display()
                ));
                continue;
            }
            let mut send = |request: &RegistryRequest| {
                native_host_client::execute_detailed(request, RegistryLimits::default())
            };
            match native_host_client::publish_sequence(
                &artifact.channel,
                &artifact.package,
                &payload,
                credential.as_deref(),
                &mut send,
            ) {
                Ok(steps) => {
                    for step in &steps {
                        println!("      [{}] {}", step.status, step.description);
                    }
                    published += 1;
                }
                Err(error) => {
                    skipped.push(format!("{} [{host}]: {error:#}", artifact.target));
                }
            }
            continue;
        }

        let request = match native_host_client::publish_request(
            &artifact.channel,
            &artifact.package,
            &payload,
            credential.as_deref(),
        ) {
            Ok(request) => request,
            // `publish_request` knows the channel but not the tag template,
            // so it names the version. For a VCS-published host the tag is
            // the instruction, and `go/v0.1.0-rc.2` is not `0.1.0-rc.2` —
            // telling someone to push the wrong ref is worse than saying
            // nothing.
            Err(NativeHostClientError::VcsPublished { host, .. }) => {
                skipped.push(format!(
                    "{} [{host}]: publishes by pushing a VCS tag, not by uploading \
                     to a registry; tag `{}` and let the index pick it up",
                    artifact.target, artifact.vcs_tag
                ));
                continue;
            }
            Err(error) => {
                skipped.push(format!("{} [{}]: {error}", artifact.target, host));
                continue;
            }
        };
        report_request(&artifact.target, artifact, &request);
        if dry_run {
            continue;
        }
        if !payload.exists() {
            skipped.push(format!(
                "{} [{}]: no built artifact at {}",
                artifact.target,
                host,
                payload.display()
            ));
            continue;
        }
        let mut send = |request: &RegistryRequest| {
            native_host_client::execute_detailed(request, RegistryLimits::default())
        };
        let steps = native_host_client::publish_sequence(
            &artifact.channel,
            &artifact.package,
            &payload,
            credential.as_deref(),
            &mut send,
        )
        .with_context(|| format!("publish {} to {host}", artifact.package))?;
        for step in &steps {
            println!("      [{}] {}", step.status, step.description);
        }
        published += 1;
    }

    for note in &skipped {
        println!("skip  {note}");
    }
    if dry_run {
        // Count what was selected, not what the manifest declares: with
        // `--target` those differ, and reporting the larger number reads as
        // "everything is covered".
        println!("\ndry run: {selected} route(s) planned, nothing sent");
    } else {
        println!(
            "\npublished {published} route(s), skipped {}",
            skipped.len()
        );
    }
    Ok(())
}

/// List the versions each native route's registry already serves.
///
/// This is the pull half: it reads the ecosystem registry over its own HTTP
/// API, with no package-manager binary involved, which is what makes it usable
/// from a runner that has none of these toolchains installed.
pub fn versions(project: &Path, only_target: Option<&str>) -> Result<()> {
    let manifest = read_manifest(project)?;
    let plan = build_plan(&manifest, ReleaseChannel::Stable, 1)?;

    for artifact in &plan.native {
        if only_target.is_some_and(|target| target != artifact.target) {
            continue;
        }
        let host = artifact.channel.host;
        let request =
            match native_host_client::version_index_request(&artifact.channel, &artifact.package) {
                Ok(request) => request,
                Err(error) => {
                    println!("{} [{host}] {}: {error}", artifact.target, artifact.package);
                    continue;
                }
            };
        let body = match native_host_client::execute(&request) {
            Ok(body) => body,
            Err(error) => {
                println!("{} [{host}] {}: {error}", artifact.target, artifact.package);
                continue;
            }
        };
        match native_host_client::parse_versions(&artifact.channel, &body) {
            Ok(found) => println!(
                "{} [{host}] {}: {}",
                artifact.target,
                artifact.package,
                if found.is_empty() {
                    "no published versions".to_string()
                } else {
                    found.join(", ")
                }
            ),
            Err(error) => println!("{} [{host}] {}: {error}", artifact.target, artifact.package),
        }
    }
    Ok(())
}

/// Where a built native artifact is expected to sit inside its target root.
///
/// Deliberately a convention rather than a search: a release that picked up
/// whichever archive happened to be lying in the directory would be a release
/// nobody can reproduce.
fn native_artifact_path(project: &Path, artifact: &NativeReleaseArtifact) -> PathBuf {
    project
        .join(&artifact.dir)
        .join(".zed/native-release")
        .join(artifact.channel.host.as_str())
        .join(format!(
            "{}-{}.artifact",
            artifact.package.replace(['/', ':', '@'], "-"),
            artifact.channel.version
        ))
}

fn report_request(target: &str, artifact: &NativeReleaseArtifact, request: &RegistryRequest) {
    let channel = artifact.channel.channel;
    let tag = artifact
        .channel
        .dist_tag
        .as_deref()
        .map(|tag| format!(" tag={tag}"))
        .unwrap_or_default();
    println!(
        "route {target} -> {} {}@{} [{channel}{tag}]",
        artifact.channel.host, artifact.package, artifact.channel.version
    );
    for line in request.describe().lines() {
        println!("      {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multi_host_manifest() -> Manifest {
        Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.4.0"

[package.repository]
url = "https://github.com/acme/clients"

[targets.nodejs]
dir = "clients/typescript"

[targets.nodejs.native]
registry = "npm"
package = "@acme/client"

[targets.python]
dir = "clients/python"

[targets.python.native]
registry = "pypi"
package = "acme-client"

[targets.java]
dir = "clients/java"

[targets.java.native]
registry = "clojars"
package = "com.acme:client"

[targets.ruby]
dir = "clients/ruby"

[targets.ruby.native]
registry = "rubygems"
package = "acme-client"
"#,
        )
        .unwrap()
    }

    #[test]
    fn one_channel_flag_resolves_to_four_different_version_strings() {
        // The behaviour the whole native-host model exists for: `--channel rc`
        // is one input, and each ecosystem stores a candidate differently. A
        // plan that emitted `1.4.0-rc.1` everywhere would be rejected by PyPI
        // and mis-sorted by RubyGems.
        let plan = build_plan(&multi_host_manifest(), ReleaseChannel::Rc, 2).unwrap();
        let mut resolved: Vec<(&str, &str)> = plan
            .native
            .iter()
            .map(|artifact| (artifact.target.as_str(), artifact.channel.version.as_str()))
            .collect();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("java", "1.4.0-RC2"),
                ("nodejs", "1.4.0-rc.2"),
                ("python", "1.4.0rc2"),
                ("ruby", "1.4.0.rc.2"),
            ]
        );

        // The release set still shares one source version and one tag — the
        // channel changes the destination, not the commit being released.
        for artifact in &plan.native {
            assert_eq!(artifact.version, "1.4.0");
            assert!(artifact.channel.requires_opt_in);
        }
        assert_eq!(plan.source.version, "1.4.0");
    }

    #[test]
    fn only_npm_gets_a_dist_tag_and_stable_leaves_versions_alone() {
        let rc = build_plan(&multi_host_manifest(), ReleaseChannel::Rc, 1).unwrap();
        let tags: Vec<(&str, Option<&str>)> = rc
            .native
            .iter()
            .map(|artifact| {
                (
                    artifact.target.as_str(),
                    artifact.channel.dist_tag.as_deref(),
                )
            })
            .collect();
        // Without the dist-tag, `npm install @acme/client` would serve the
        // candidate; the other three have no such mechanism to get wrong.
        assert_eq!(tags.iter().filter(|(_, tag)| tag.is_some()).count(), 1);
        assert!(tags.contains(&("nodejs", Some("rc"))));

        let stable = build_plan(&multi_host_manifest(), ReleaseChannel::Stable, 1).unwrap();
        for artifact in &stable.native {
            assert_eq!(artifact.channel.version, "1.4.0", "{}", artifact.target);
            assert!(!artifact.channel.requires_opt_in);
        }
    }

    #[test]
    fn a_host_without_a_candidate_track_fails_the_plan_by_name() {
        // Publishing a candidate as stable would move every unpinned
        // consumer, so the plan has to stop and say which target cannot.
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.4.0"

[package.repository]
url = "https://github.com/acme/clients"

[targets.r]
dir = "clients/r"

[targets.r.native]
registry = "cran"
package = "acme.client"
"#,
        )
        .unwrap();
        let error = build_plan(&manifest, ReleaseChannel::Rc, 1).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("target `r`"), "{message}");
        assert!(message.contains("cran"), "{message}");
        // The same manifest releases fine on the stable track.
        assert!(build_plan(&manifest, ReleaseChannel::Stable, 1).is_ok());
    }

    fn hex_manifest(package: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"
[package]
org = "zed-pkg-test"
name = "gleam-lib"
version = "1.0.0"

[package.repository]
url = "https://github.com/zed-pkg-test/gleam-lib"

[publish.native]
registry = "hex"
package = "{package}"
"#
        ))
        .unwrap()
    }

    #[test]
    fn a_gleam_manifest_is_cross_checked_because_it_is_data() {
        // Gleam is the one Hex ecosystem whose manifest is TOML rather than
        // executable code, so it is the one that can actually be verified.
        // Shape copied from the real `zed-pkg-test/gleam-lib` fixture.
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("gleam.toml"),
            "name = \"zed_pkg_test_gleam_lib\"\nversion = \"1.0.0\"\nlicences = [\"MIT\"]\n",
        )
        .unwrap();

        let unchecked =
            validate_native_manifests(root.path(), &hex_manifest("zed_pkg_test_gleam_lib"))
                .unwrap();
        assert!(unchecked.is_empty(), "a gleam.toml route is fully checked");

        // A route naming a different package must not publish under it.
        let error = validate_native_manifests(root.path(), &hex_manifest("some_other_package"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("names `zed_pkg_test_gleam_lib`"), "{error}");
    }

    #[test]
    fn a_gleam_version_that_drifts_from_the_release_set_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("gleam.toml"),
            "name = \"zed_pkg_test_gleam_lib\"\nversion = \"0.9.0\"\n",
        )
        .unwrap();
        let error = validate_native_manifests(root.path(), &hex_manifest("zed_pkg_test_gleam_lib"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("release-set version `1.0.0`"), "{error}");
        assert!(error.contains("uses `0.9.0`"), "{error}");
    }

    #[test]
    fn a_beam_build_script_is_reported_unchecked_rather_than_rejected() {
        // `mix.exs` and `rebar.config` are executable code. Refusing the route
        // would reject every Elixir and Erlang package; passing it silently
        // would claim a check that never ran.
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("mix.exs"), "defmodule X do\nend\n").unwrap();
        let unchecked =
            validate_native_manifests(root.path(), &hex_manifest("anything_at_all")).unwrap();
        assert_eq!(unchecked.len(), 1);
        assert!(unchecked[0].contains("mix.exs"), "{}", unchecked[0]);
        assert!(
            unchecked[0].contains("not cross-checked"),
            "{}",
            unchecked[0]
        );

        // No build file at all is still an error: that is a broken route, not
        // an unparseable one.
        let empty = tempfile::tempdir().unwrap();
        assert!(validate_native_manifests(empty.path(), &hex_manifest("x")).is_err());
    }

    #[test]
    fn a_gradle_jvm_target_publishes_instead_of_being_rejected_for_having_no_pom() {
        // Requiring `pom.xml` rejected the majority of modern JVM projects
        // outright — Gradle, sbt, Leiningen, and deps.edn all hold their
        // coordinates in build code.
        let manifest = Manifest::parse(
            r#"
[package]
org = "zedtest"
name = "clients"
version = "3.1.0"

[package.repository]
url = "https://github.com/zed-pkg-test/clients"

[targets.java]
dir = "clients/java"

[targets.java.native]
registry = "maven-central"
package = "com.acme:client"
"#,
        )
        .unwrap();

        let root = tempfile::tempdir().unwrap();
        let java = root.path().join("clients/java");
        fs::create_dir_all(&java).unwrap();

        // No build file at all: still a hard failure.
        assert!(validate_native_manifests(root.path(), &manifest).is_err());

        // Gradle: routable, and the gap is reported rather than hidden.
        fs::write(java.join("build.gradle.kts"), "plugins { }\n").unwrap();
        let unchecked = validate_native_manifests(root.path(), &manifest).unwrap();
        assert_eq!(unchecked.len(), 1);
        assert!(
            unchecked[0].contains("build.gradle.kts"),
            "{}",
            unchecked[0]
        );

        // A pom.xml alongside it wins, and is checked properly.
        fs::write(
            java.join("pom.xml"),
            "<project><groupId>com.acme</groupId><artifactId>client</artifactId>\
             <version>3.1.0</version></project>",
        )
        .unwrap();
        assert!(
            validate_native_manifests(root.path(), &manifest)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_single_language_package_routes_from_the_root_publish_section() {
        // A repo with no `[targets.*]` declares its route at
        // `[publish.native]`, so the channel and tag template have to be read
        // from there. Looking only at `targets` silently dropped the declared
        // channel and left such a package on the stable track.
        let manifest = Manifest::parse(
            r#"
[package]
org = "zed-pkg-test"
name = "node-lib"
version = "1.0.0"

[package.repository]
url = "https://github.com/zed-pkg-test/node-lib"

[publish.native]
registry = "npm"
package = "@zed-pkg-test/node-lib"
channel = "beta"
"#,
        )
        .unwrap();

        let declared = build_plan(&manifest, ReleaseChannel::Stable, 1).unwrap();
        assert_eq!(declared.native.len(), 1);
        let route = &declared.native[0];
        assert_eq!(route.channel.channel, ReleaseChannel::Beta);
        assert_eq!(route.channel.version, "1.0.0-beta.1");
        assert_eq!(route.channel.dist_tag.as_deref(), Some("beta"));
        // The tag has to carry the channel version too, or the pre-release
        // and the eventual 1.0.0 would claim the same ref.
        assert_eq!(route.vcs_tag, "v1.0.0-beta.1");
        assert_eq!(
            route.version, "1.0.0",
            "the release set version is unchanged"
        );

        let overridden = build_plan(&manifest, ReleaseChannel::Rc, 3).unwrap();
        assert_eq!(overridden.native[0].channel.version, "1.0.0-rc.3");
        assert_eq!(overridden.native[0].vcs_tag, "v1.0.0-rc.3");
    }

    #[test]
    fn a_manifest_declared_channel_applies_when_no_flag_overrides_it() {
        // A client generated from an unstable API surface only ever ships as
        // a candidate; that belongs in the manifest, not in every CI invocation.
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.4.0"

[package.repository]
url = "https://github.com/acme/clients"

[targets.nodejs]
dir = "clients/typescript"

[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
channel = "beta"
"#,
        )
        .unwrap();
        let default = build_plan(&manifest, ReleaseChannel::Stable, 3).unwrap();
        assert_eq!(default.native[0].channel.version, "1.4.0-beta.3");

        // An explicit channel wins, so a repo can still cut a real release.
        let overridden = build_plan(&manifest, ReleaseChannel::Rc, 1).unwrap();
        assert_eq!(overridden.native[0].channel.version, "1.4.0-rc.1");
    }

    fn polyglot_manifest() -> Manifest {
        Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/clients"

[targets.rust]
dir = "clients/rust"

[targets.rust.native]
registry = "crates-io"
package = "acme-client"

[targets.repository]
dir = "."
name = "clients-repository"

[targets.nodejs]
dir = "clients/typescript"
adapter = "node"

[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
forge = ["github-packages", "gitlab-packages", "bitbucket-packages"]

[targets.dart]
dir = "clients/dart"

[targets.dart.native]
registry = "pub.dev"
package = "acme_client"

[targets.python]
dir = "clients/python"

[targets.python.native]
registry = "pypi"
package = "Acme.Client"
forge = ["gitlab-packages"]
"#,
        )
        .unwrap()
    }

    fn write_native_manifests(root: &Path, npm: &str, cargo: &str, pubspec: &str, pyproject: &str) {
        fs::create_dir_all(root.join("clients/typescript")).unwrap();
        fs::create_dir_all(root.join("clients/rust")).unwrap();
        fs::create_dir_all(root.join("clients/dart")).unwrap();
        fs::create_dir_all(root.join("clients/python")).unwrap();
        fs::write(root.join("clients/typescript/package.json"), npm).unwrap();
        fs::write(root.join("clients/rust/Cargo.toml"), cargo).unwrap();
        fs::write(root.join("clients/dart/pubspec.yaml"), pubspec).unwrap();
        fs::write(root.join("clients/python/pyproject.toml"), pyproject).unwrap();
    }

    #[test]
    fn polyglot_plan_is_deterministic_and_includes_native_routes() {
        let manifest = polyglot_manifest();

        let plan = build_plan(&manifest, ReleaseChannel::Stable, 1).unwrap();
        assert_eq!(plan.release_set, "acme/clients@1.2.3#v1.2.3");
        assert_eq!(
            plan.zed
                .iter()
                .map(|artifact| artifact.target.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["dart", "nodejs", "python", "repository", "rust"]
        );
        assert_eq!(
            plan.native
                .iter()
                .map(|artifact| (artifact.registry.as_str(), artifact.package.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("pub.dev", "acme_client"),
                ("npm", "@acme/client"),
                ("pypi", "Acme.Client"),
                ("crates-io", "acme-client"),
            ]
        );
        assert_eq!(
            plan.forge
                .iter()
                .map(|artifact| (
                    artifact.registry.as_str(),
                    artifact.format.as_str(),
                    artifact.package.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("github-packages", "npm", "@acme/client"),
                ("gitlab-packages", "npm", "@acme/client"),
                ("bitbucket-packages", "npm", "@acme/client"),
                ("gitlab-packages", "pypi", "Acme.Client"),
            ]
        );

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"release_set\":\"acme/clients@1.2.3#v1.2.3\""));
        let human = render_human(&plan);
        assert!(human.contains("native artifacts:"));
        assert!(human.contains("npm @acme/client@1.2.3"));
        assert!(human.contains("forge package mirrors:"));
        assert!(human.contains("github-packages via npm @acme/client@1.2.3"));
    }

    #[test]
    fn native_manifests_match_declared_routes_and_release_version() {
        let root = tempfile::tempdir().unwrap();
        write_native_manifests(
            root.path(),
            r#"{"name":"@acme/client","version":"1.2.3","private":false}"#,
            r#"
[package]
name = "acme-client"
version = "1.2.3"
publish = ["crates-io"]
"#,
            "name: acme_client\nversion: 1.2.3\n",
            "[project]\nname = \"acme-client\"\nversion = \"1.2.3\"\n",
        );

        validate_native_manifests(root.path(), &polyglot_manifest()).unwrap();
    }

    #[test]
    fn native_manifest_mismatches_fail_before_planning() {
        let cases = [
            (
                r#"{"name":"@wrong/client","version":"1.2.3"}"#,
                r#"[package]
name = "acme-client"
version = "1.2.3"
"#,
                "names `@wrong/client`",
            ),
            (
                r#"{"name":"@acme/client","version":"9.9.9"}"#,
                r#"[package]
name = "acme-client"
version = "1.2.3"
"#,
                "uses `9.9.9`",
            ),
            (
                r#"{"name":"@acme/client","version":"1.2.3","private":true}"#,
                r#"[package]
name = "acme-client"
version = "1.2.3"
"#,
                "private: true",
            ),
            (
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                r#"[package]
name = "wrong-client"
version = "1.2.3"
"#,
                "names `wrong-client`",
            ),
            (
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                r#"[package]
name = "acme-client"
version = "9.9.9"
"#,
                "uses `9.9.9`",
            ),
            (
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                r#"[package]
name = "acme-client"
version = "1.2.3"
publish = false
"#,
                "publish = false",
            ),
        ];

        for (npm, cargo, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                npm,
                cargo,
                "name: acme_client\nversion: 1.2.3\n",
                "[project]\nname = \"Acme.Client\"\nversion = \"1.2.3\"\n",
            );
            let error = validate_native_manifests(root.path(), &polyglot_manifest())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "{error:?} did not contain {expected:?}"
            );
        }
    }

    #[test]
    fn pubspec_mismatches_fail_before_planning() {
        let cases = [
            (
                "name: wrong_client\nversion: 1.2.3\n",
                "names `wrong_client`",
            ),
            ("name: acme_client\nversion: 9.9.9\n", "uses `9.9.9`"),
            (
                "name: acme_client\nversion: 1.2.3\npublish_to: none\n",
                "publish_to: none",
            ),
            (
                "name: 'acme_client' # package\nversion: \"1.2.3\" # release\n",
                "__valid__",
            ),
        ];

        for (pubspec, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                "[package]\nname = \"acme-client\"\nversion = \"1.2.3\"\n",
                pubspec,
                "[project]\nname = \"Acme.Client\"\nversion = \"1.2.3\"\n",
            );
            let result = validate_native_manifests(root.path(), &polyglot_manifest());
            if expected == "__valid__" {
                result.unwrap();
            } else {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains(expected),
                    "{error:?} did not contain {expected:?}"
                );
            }
        }
    }

    #[test]
    fn pyproject_mismatches_fail_before_planning() {
        let cases = [
            (
                "[project]\nname = \"wrong-client\"\nversion = \"1.2.3\"\n",
                "names `wrong-client`",
            ),
            (
                "[project]\nname = \"Acme.Client\"\nversion = \"9.9.9\"\n",
                "uses `9.9.9`",
            ),
            (
                "[project]\nname = \"Acme.Client\"\ndynamic = [\"version\"]\n",
                "declares `version` as dynamic",
            ),
            (
                "[build-system]\nrequires = []\nbuild-backend = \"example.backend\"\n",
                "has no `[project]` table",
            ),
            (
                "[project]\nname = \"acme_client\"\nversion = \"1.2.3\"\n",
                "__valid__",
            ),
        ];

        for (pyproject, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                "[package]\nname = \"acme-client\"\nversion = \"1.2.3\"\n",
                "name: acme_client\nversion: 1.2.3\n",
                pyproject,
            );
            let result = validate_native_manifests(root.path(), &polyglot_manifest());
            if expected == "__valid__" {
                result.unwrap();
            } else {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains(expected),
                    "{error:?} did not contain {expected:?}"
                );
            }
        }
    }

    #[test]
    fn single_language_plan_keeps_the_root_package() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "http-kit"
version = "0.4.0"

[package.repository]
url = "https://github.com/acme/http-kit"

[publish.native]
registry = "npm"
package = "@acme/http-kit"
forge = ["github-packages", "gitlab-packages", "bitbucket-packages"]
"#,
        )
        .unwrap();

        let plan = build_plan(&manifest, ReleaseChannel::Stable, 1).unwrap();
        assert_eq!(plan.zed.len(), 1);
        assert_eq!(plan.zed[0].target, None);
        assert_eq!(plan.zed[0].package, "acme/http-kit");
        assert_eq!(plan.native.len(), 1);
        assert_eq!(plan.native[0].target, "repository");
        assert_eq!(plan.native[0].dir, ".");
        assert_eq!(plan.native[0].registry, "npm");
        assert_eq!(plan.native[0].package, "@acme/http-kit");
        assert_eq!(plan.forge.len(), 3);
        assert!(
            plan.forge
                .iter()
                .all(|artifact| artifact.target == "repository" && artifact.dir == ".")
        );

        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"@acme/http-kit","version":"0.4.0"}"#,
        )
        .unwrap();
        validate_native_manifests(root.path(), &manifest).unwrap();
    }

    #[test]
    fn maven_rubygems_nuget_packagist_and_go_manifests_are_validated() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["java", "ruby", "csharp", "php", "go"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        fs::write(
            root.path().join("java/pom.xml"),
            r#"<project>
  <groupId>com.acme</groupId>
  <artifactId>client</artifactId>
  <version>1.2.3</version>
</project>"#,
        )
        .unwrap();
        fs::write(
            root.path().join("ruby/acme-client.gemspec"),
            r#"Gem::Specification.new do |spec|
  spec.name = "acme-client"
  spec.version = "1.2.3"
end
"#,
        )
        .unwrap();
        fs::write(
            root.path().join("csharp/Acme.Client.csproj"),
            r#"<Project><PropertyGroup>
  <PackageId>Acme.Client</PackageId>
  <Version>1.2.3</Version>
</PropertyGroup></Project>"#,
        )
        .unwrap();
        fs::write(
            root.path().join("php/composer.json"),
            r#"{"name":"acme/client","version":"1.2.3"}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("go/go.mod"),
            "module github.com/acme/client\n\ngo 1.24\n",
        )
        .unwrap();

        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/clients"

[targets.java]
dir = "java"
[targets.java.native]
registry = "maven-central"
package = "com.acme:client"
forge = ["github-packages", "gitlab-packages", "bitbucket-packages"]

[targets.ruby]
dir = "ruby"
[targets.ruby.native]
registry = "rubygems"
package = "acme-client"
forge = ["github-packages", "gitlab-packages"]

[targets.csharp]
dir = "csharp"
[targets.csharp.native]
registry = "nuget"
package = "Acme.Client"
forge = ["github-packages", "gitlab-packages"]

[targets.php]
dir = "php"
[targets.php.native]
registry = "packagist"
package = "acme/client"
forge = ["gitlab-packages"]

[targets.golang]
dir = "go"
[targets.golang.native]
registry = "go-modules"
package = "github.com/acme/client"
tag_format = "go/v{version}"
forge = ["gitlab-packages"]
"#,
        )
        .unwrap();

        validate_native_manifests(root.path(), &manifest).unwrap();
        let plan = build_plan(&manifest, ReleaseChannel::Stable, 1).unwrap();
        assert_eq!(plan.native.len(), 5);
        assert_eq!(plan.forge.len(), 9);
        assert_eq!(
            plan.native
                .iter()
                .find(|artifact| artifact.target == "golang")
                .unwrap()
                .vcs_tag,
            "go/v1.2.3"
        );
    }
}
