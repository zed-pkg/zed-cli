#!/usr/bin/env python3
"""Add pub.dev manifest validation and fixed preflight support."""

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
    '''            NativeRegistry::CratesIo => {
                validate_cargo_manifest(
                    &target_root.join("Cargo.toml"),
                    &route.target,
                    &route.package,
                    &manifest.package.version,
                )?;
            }
        }''',
    '''            NativeRegistry::CratesIo => {
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
        }''',
    "pub.dev validation dispatch",
)

replace_once(
    "src/release.rs",
    '''fn validate_cargo_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {''',
    '''fn strip_yaml_comment(value: &str) -> &str {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\\\' if double => escaped = true,
            b'\\'' if !double => single = !single,
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
    if value.starts_with('\\'') {
        if !value.ends_with('\\'') || value.len() < 2 {
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

fn validate_cargo_manifest(
    path: &Path,
    target: &str,
    expected_name: &str,
    expected_version: &str,
) -> Result<()> {''',
    "pubspec validation helpers",
)

replace_once(
    "src/release.rs",
    '''[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
"#,''',
    '''[targets.nodejs.native]
registry = "npm"
package = "@acme/client"

[targets.dart]
dir = "clients/dart"

[targets.dart.native]
registry = "pub.dev"
package = "acme_client"
"#,''',
    "release test Dart route",
)

replace_once(
    "src/release.rs",
    '''    fn write_native_manifests(root: &Path, npm: &str, cargo: &str) {
        fs::create_dir_all(root.join("clients/typescript")).unwrap();
        fs::create_dir_all(root.join("clients/rust")).unwrap();
        fs::write(root.join("clients/typescript/package.json"), npm).unwrap();
        fs::write(root.join("clients/rust/Cargo.toml"), cargo).unwrap();
    }''',
    '''    fn write_native_manifests(root: &Path, npm: &str, cargo: &str, pubspec: &str) {
        fs::create_dir_all(root.join("clients/typescript")).unwrap();
        fs::create_dir_all(root.join("clients/rust")).unwrap();
        fs::create_dir_all(root.join("clients/dart")).unwrap();
        fs::write(root.join("clients/typescript/package.json"), npm).unwrap();
        fs::write(root.join("clients/rust/Cargo.toml"), cargo).unwrap();
        fs::write(root.join("clients/dart/pubspec.yaml"), pubspec).unwrap();
    }''',
    "native manifest fixture writer",
)

replace_once(
    "src/release.rs",
    '''            vec!["nodejs", "repository", "rust"]
        );''',
    '''            vec!["dart", "nodejs", "repository", "rust"]
        );''',
    "Zed target ordering assertion",
)

replace_once(
    "src/release.rs",
    '''            vec![("npm", "@acme/client"), ("crates-io", "acme-client")]
        );''',
    '''            vec![
                ("pub.dev", "acme_client"),
                ("npm", "@acme/client"),
                ("crates-io", "acme-client"),
            ]
        );''',
    "native route ordering assertion",
)

replace_once(
    "src/release.rs",
    '''publish = ["crates-io"]
"#,
        );''',
    '''publish = ["crates-io"]
"#,
            "name: acme_client\\nversion: 1.2.3\\n",
        );''',
    "valid pubspec fixture",
)

replace_once(
    "src/release.rs",
    '''        for (npm, cargo, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(root.path(), npm, cargo);''',
    '''        for (npm, cargo, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                npm,
                cargo,
                "name: acme_client\\nversion: 1.2.3\\n",
            );''',
    "existing mismatch fixture calls",
)

replace_once(
    "src/release.rs",
    '''    #[test]
    fn single_language_plan_keeps_the_root_package() {''',
    '''    #[test]
    fn pubspec_mismatches_fail_before_planning() {
        let cases = [
            ("name: wrong_client\\nversion: 1.2.3\\n", "names `wrong_client`"),
            ("name: acme_client\\nversion: 9.9.9\\n", "uses `9.9.9`"),
            (
                "name: acme_client\\nversion: 1.2.3\\npublish_to: none\\n",
                "publish_to: none",
            ),
            (
                "name: 'acme_client' # package\\nversion: \\\"1.2.3\\\" # release\\n",
                "__valid__",
            ),
        ];

        for (pubspec, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            write_native_manifests(
                root.path(),
                r#"{"name":"@acme/client","version":"1.2.3"}"#,
                "[package]\\nname = \\\"acme-client\\\"\\nversion = \\\"1.2.3\\\"\\n",
                pubspec,
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
    "pubspec mismatch tests",
)

replace_once(
    "src/preflight.rs",
    '''    "CARGO_REGISTRY_TOKEN",
    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
];''',
    '''    "CARGO_REGISTRY_TOKEN",
    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
    "PUB_HOSTED_URL",
    "PUB_TOKEN",
    "DART_PUB_TOKEN",
];''',
    "Dart credential environment stripping",
)

replace_once(
    "src/preflight.rs",
    '''                NativeRegistry::CratesIo => ("cargo", vec!["package", "--list", "--allow-dirty"]),
            };''',
    '''                NativeRegistry::CratesIo => ("cargo", vec!["package", "--list", "--allow-dirty"]),
                NativeRegistry::PubDev => ("dart", vec!["pub", "publish", "--dry-run"]),
            };''',
    "pub.dev adapter command",
)

replace_once(
    "src/preflight.rs",
    '''[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
"#,''',
    '''[targets.nodejs.native]
registry = "npm"
package = "@acme/client"

[targets.dart]
dir = "clients/dart"
[targets.dart.native]
registry = "pub.dev"
package = "acme_client"
"#,''',
    "preflight test Dart route",
)

replace_once(
    "src/preflight.rs",
    '''        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].target, "nodejs");''',
    '''        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].target, "dart");
        assert_eq!(specs[0].program, "dart");
        assert_eq!(specs[0].args, ["pub", "publish", "--dry-run"]);
        assert_eq!(specs[0].cwd, Path::new("/repo/clients/dart"));
        assert_eq!(specs[1].target, "nodejs");''',
    "preflight spec count and Dart assertion",
)

replace_once(
    "src/preflight.rs",
    '''        assert_eq!(specs[0].program, "npm");
        assert_eq!(
            specs[0].args,
            ["pack", "--dry-run", "--json", "--ignore-scripts"]
        );
        assert_eq!(specs[0].cwd, Path::new("/repo/clients/typescript"));
        assert_eq!(specs[1].target, "rust");
        assert_eq!(specs[1].program, "cargo");
        assert_eq!(specs[1].args, ["package", "--list", "--allow-dirty"]);
        assert_eq!(specs[1].cwd, Path::new("/repo/clients/rust"));''',
    '''        assert_eq!(specs[1].program, "npm");
        assert_eq!(
            specs[1].args,
            ["pack", "--dry-run", "--json", "--ignore-scripts"]
        );
        assert_eq!(specs[1].cwd, Path::new("/repo/clients/typescript"));
        assert_eq!(specs[2].target, "rust");
        assert_eq!(specs[2].program, "cargo");
        assert_eq!(specs[2].args, ["package", "--list", "--allow-dirty"]);
        assert_eq!(specs[2].cwd, Path::new("/repo/clients/rust"));''',
    "preflight npm/Cargo shifted assertions",
)

replace_once(
    "src/preflight.rs",
    '''            vec!["nodejs", "rust"]
        );''',
    '''            vec!["dart", "nodejs", "rust"]
        );''',
    "preflight execution order",
)

replace_once(
    "README.md",
    '''| `zed release preflight` | Validate native manifests, then run fixed credential-free npm/crates.io package preflight adapters |''',
    '''| `zed release preflight` | Validate native manifests, then run fixed credential-free npm/crates.io/pub.dev package preflight adapters |''',
    "README adapter list",
)

print("added pub.dev validation and preflight")
