#!/usr/bin/env python3
"""Add PyPI manifest validation and fixed local build preflight support."""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one insertion point, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/release.rs",
    '''enum CargoPublishPolicy {
    Enabled(bool),
    Registries(Vec<String>),
}

pub fn build_plan''',
    '''enum CargoPublishPolicy {
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

pub fn build_plan''',
    "PyPI metadata structs",
)

replace_once(
    "src/release.rs",
    '''            NativeRegistry::PubDev => {
                validate_pubspec_manifest(
                    &target_root.join("pubspec.yaml"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
        }''',
    '''            NativeRegistry::PubDev => {
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
        }''',
    "PyPI validation dispatch",
)

replace_once(
    "src/release.rs",
    '''fn validate_cargo_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {''',
    '''fn validate_pyproject_manifest(
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
) -> Result<()> {''',
    "PyPI pyproject validator",
)

replace_once(
    "src/release.rs",
    '''[targets.dart.native]
registry = "pub.dev"
package = "acme_client"
"#,''',
    '''[targets.dart.native]
registry = "pub.dev"
package = "acme_client"

[targets.python]
dir = "clients/python"

[targets.python.native]
registry = "pypi"
package = "Acme.Client"
"#,''',
    "release test Python route",
)

replace_once(
    "src/release.rs",
    '''    fn write_native_manifests(root: &Path, npm: &str, cargo: &str, pubspec: &str) {
        fs::create_dir_all(root.join("clients/typescript")).unwrap();
        fs::create_dir_all(root.join("clients/rust")).unwrap();
        fs::create_dir_all(root.join("clients/dart")).unwrap();
        fs::write(root.join("clients/typescript/package.json"), npm).unwrap();
        fs::write(root.join("clients/rust/Cargo.toml"), cargo).unwrap();
        fs::write(root.join("clients/dart/pubspec.yaml"), pubspec).unwrap();
    }''',
    '''    fn write_native_manifests(
        root: &Path,
        npm: &str,
        cargo: &str,
        pubspec: &str,
        pyproject: &str,
    ) {
        fs::create_dir_all(root.join("clients/typescript")).unwrap();
        fs::create_dir_all(root.join("clients/rust")).unwrap();
        fs::create_dir_all(root.join("clients/dart")).unwrap();
        fs::create_dir_all(root.join("clients/python")).unwrap();
        fs::write(root.join("clients/typescript/package.json"), npm).unwrap();
        fs::write(root.join("clients/rust/Cargo.toml"), cargo).unwrap();
        fs::write(root.join("clients/dart/pubspec.yaml"), pubspec).unwrap();
        fs::write(root.join("clients/python/pyproject.toml"), pyproject).unwrap();
    }''',
    "native manifest fixture writer with PyPI",
)

replace_once(
    "src/release.rs",
    '''            vec!["dart", "nodejs", "repository", "rust"]
        );''',
    '''            vec!["dart", "nodejs", "python", "repository", "rust"]
        );''',
    "release Zed target order with Python",
)

replace_once(
    "src/release.rs",
    '''                ("pub.dev", "acme_client"),
                ("npm", "@acme/client"),
                ("crates-io", "acme-client"),''',
    '''                ("pub.dev", "acme_client"),
                ("npm", "@acme/client"),
                ("pypi", "Acme.Client"),
                ("crates-io", "acme-client"),''',
    "release native route order with PyPI",
)

replace_once(
    "src/release.rs",
    '''            "name: acme_client\\nversion: 1.2.3\\n",
        );''',
    '''            "name: acme_client\\nversion: 1.2.3\\n",
            "[project]\\nname = \\\"acme-client\\\"\\nversion = \\\"1.2.3\\\"\\n",
        );''',
    "valid PyPI fixture",
)

replace_once(
    "src/release.rs",
    '''                "name: acme_client\\nversion: 1.2.3\\n",
            );''',
    '''                "name: acme_client\\nversion: 1.2.3\\n",
                "[project]\\nname = \\\"Acme.Client\\\"\\nversion = \\\"1.2.3\\\"\\n",
            );''',
    "existing mismatch cases add valid PyPI fixture",
)

replace_once(
    "src/release.rs",
    '''                pubspec,
            );''',
    '''                pubspec,
                "[project]\\nname = \\\"Acme.Client\\\"\\nversion = \\\"1.2.3\\\"\\n",
            );''',
    "pubspec mismatch cases add valid PyPI fixture",
)

replace_once(
    "src/release.rs",
    '''    #[test]
    fn single_language_plan_keeps_the_root_package() {''',
    '''    #[test]
    fn pyproject_mismatches_fail_before_planning() {
        let cases = [
            (
                "[project]\\nname = \\\"wrong-client\\\"\\nversion = \\\"1.2.3\\\"\\n",
                "names `wrong-client`",
            ),
            (
                "[project]\\nname = \\\"Acme.Client\\\"\\nversion = \\\"9.9.9\\\"\\n",
                "uses `9.9.9`",
            ),
            (
                "[project]\\nname = \\\"Acme.Client\\\"\\ndynamic = [\\\"version\\\"]\\n",
                "declares `version` as dynamic",
            ),
            (
                "[build-system]\\nrequires = []\\nbuild-backend = \\\"example.backend\\\"\\n",
                "has no `[project]` table",
            ),
            (
                "[project]\\nname = \\\"acme_client\\\"\\nversion = \\\"1.2.3\\\"\\n",
                "__valid__",
            ),
        ];

        for (pyproject, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                "[package]\\nname = \\\"acme-client\\\"\\nversion = \\\"1.2.3\\\"\\n",
                "name: acme_client\\nversion: 1.2.3\\n",
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
    fn single_language_plan_keeps_the_root_package() {''',
    "PyPI validation tests",
)

replace_once(
    "src/preflight.rs",
    '''use std::path::{Path, PathBuf};''',
    '''use std::fs;
use std::path::{Path, PathBuf};''',
    "preflight filesystem import",
)

replace_once(
    "src/preflight.rs",
    '''    "DART_PUB_TOKEN",
];''',
    '''    "DART_PUB_TOKEN",
    "TWINE_USERNAME",
    "TWINE_PASSWORD",
    "TWINE_REPOSITORY_URL",
    "UV_PUBLISH_TOKEN",
    "UV_PUBLISH_USERNAME",
    "UV_PUBLISH_PASSWORD",
    "PYPI_TOKEN",
    "PIP_INDEX_URL",
    "PIP_EXTRA_INDEX_URL",
];''',
    "PyPI credential/index environment stripping",
)

replace_once(
    "src/preflight.rs",
    '''        let mut command = Command::new(&spec.program);''',
    '''        if spec.registry == NativeRegistry::PyPi {
            let output = spec.cwd.join(".zed/native-preflight/pypi");
            if output.exists() {
                fs::remove_dir_all(&output).with_context(|| {
                    format!("remove stale PyPI preflight output {}", output.display())
                })?;
            }
            fs::create_dir_all(&output).with_context(|| {
                format!("create PyPI preflight output {}", output.display())
            })?;
        }
        let mut command = Command::new(&spec.program);''',
    "safe PyPI output directory preparation",
)

replace_once(
    "src/preflight.rs",
    '''pub fn build_specs(project: &Path, manifest: &Manifest) -> Vec<NativePreflightSpec> {''',
    '''fn python_program() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

pub fn build_specs(project: &Path, manifest: &Manifest) -> Vec<NativePreflightSpec> {''',
    "platform Python program helper",
)

replace_once(
    "src/preflight.rs",
    '''                NativeRegistry::PubDev => ("dart", vec!["pub", "publish", "--dry-run"]),
            };''',
    '''                NativeRegistry::PubDev => ("dart", vec!["pub", "publish", "--dry-run"]),
                NativeRegistry::PyPi => (
                    python_program(),
                    vec![
                        "-m",
                        "build",
                        "--no-isolation",
                        "--outdir",
                        ".zed/native-preflight/pypi",
                    ],
                ),
            };''',
    "fixed PyPI build adapter",
)

replace_once(
    "src/preflight.rs",
    '''[targets.dart.native]
registry = "pub.dev"
package = "acme_client"
"#,''',
    '''[targets.dart.native]
registry = "pub.dev"
package = "acme_client"

[targets.python]
dir = "clients/python"
[targets.python.native]
registry = "pypi"
package = "Acme.Client"
"#,''',
    "preflight test Python route",
)

replace_once(
    "src/preflight.rs",
    '''        assert_eq!(specs.len(), 3);''',
    '''        assert_eq!(specs.len(), 4);''',
    "preflight spec count with PyPI",
)

replace_once(
    "src/preflight.rs",
    '''        assert_eq!(specs[2].target, "rust");
        assert_eq!(specs[2].program, "cargo");
        assert_eq!(specs[2].args, ["package", "--list", "--allow-dirty"]);
        assert_eq!(specs[2].cwd, Path::new("/repo/clients/rust"));''',
    '''        assert_eq!(specs[2].target, "python");
        assert_eq!(specs[2].program, python_program());
        assert_eq!(
            specs[2].args,
            [
                "-m",
                "build",
                "--no-isolation",
                "--outdir",
                ".zed/native-preflight/pypi",
            ]
        );
        assert_eq!(specs[2].cwd, Path::new("/repo/clients/python"));
        assert_eq!(specs[3].target, "rust");
        assert_eq!(specs[3].program, "cargo");
        assert_eq!(specs[3].args, ["package", "--list", "--allow-dirty"]);
        assert_eq!(specs[3].cwd, Path::new("/repo/clients/rust"));''',
    "PyPI adapter spec assertion",
)

replace_once(
    "src/preflight.rs",
    '''            vec!["dart", "nodejs", "rust"]
        );''',
    '''            vec!["dart", "nodejs", "python", "rust"]
        );''',
    "preflight execution order with PyPI",
)

replace_once(
    "README.md",
    '''| `zed release preflight` | Validate native manifests, then run fixed credential-free npm/crates.io/pub.dev package preflight adapters |''',
    '''| `zed release preflight` | Validate native manifests, then run fixed credential-free npm/crates.io/pub.dev/PyPI package preflight adapters |''',
    "README PyPI adapter list",
)

print("added PyPI validation and preflight")
