use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::{Manifest, NativeRegistry};

use crate::config::read_manifest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleasePlan {
    pub release_set: String,
    pub source: ReleaseSource,
    pub zed: Vec<ZedReleaseArtifact>,
    pub native: Vec<NativeReleaseArtifact>,
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
    pub version: String,
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

pub fn build_plan(manifest: &Manifest) -> ReleasePlan {
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
        .map(|route| NativeReleaseArtifact {
            target: route.target,
            registry: route.registry.as_str().to_string(),
            package: route.package,
            version: version.clone(),
            dir: route.dir,
        })
        .collect();

    ReleasePlan {
        release_set: format!("{source_package}@{version}#{vcs_tag}"),
        source: ReleaseSource {
            package: source_package,
            version,
            vcs_tag,
            repository: manifest.package.repository.url.clone(),
        },
        zed,
        native,
    }
}

pub fn validate_native_manifests(project: &Path, manifest: &Manifest) -> Result<()> {
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
        }
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
            output.push_str(&format!(
                "  - {} {}@{} <- {} [target: {}]\n",
                artifact.registry,
                artifact.package,
                artifact.version,
                artifact.dir,
                artifact.target
            ));
        }
    }
    output
}

pub fn plan(project: &Path, json: bool) -> Result<()> {
    let manifest = read_manifest(project)?;
    validate_native_manifests(project, &manifest)?;
    let plan = build_plan(&manifest);
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", render_human(&plan));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let plan = build_plan(&manifest);
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

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"release_set\":\"acme/clients@1.2.3#v1.2.3\""));
        let human = render_human(&plan);
        assert!(human.contains("native artifacts:"));
        assert!(human.contains("npm @acme/client@1.2.3"));
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
"#,
        )
        .unwrap();

        let plan = build_plan(&manifest);
        assert_eq!(plan.zed.len(), 1);
        assert_eq!(plan.zed[0].target, None);
        assert_eq!(plan.zed[0].package, "acme/http-kit");
        assert!(plan.native.is_empty());
        assert!(render_human(&plan).contains("none declared"));
    }
}
