//! Dart package-manager wiring derived from native `pubspec.yaml` identities.
//!
//! Zed package coordinates and Dart pub package names are different namespaces:
//! `zed-pkg-test/dart-lib` may legitimately declare `name:
//! zed_pkg_test_dart_lib`. The installer historically used the final
//! installation-directory component (`dart-lib`) as the YAML key, which pub
//! cannot resolve. This module rewrites the generated path-dependency fragment
//! from each installed package's declared native identity and fails closed when
//! that identity is ambiguous or unsafe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

const WIRING_FILE: &str = ".zed/pub-deps.yaml";

/// Replace the provisional Dart dependency keys emitted by the installer.
/// Other adapters have no `pub-deps.yaml`, so this is a no-op for every
/// non-Dart install.
///
/// On invalid native metadata the provisional file is removed before the error
/// is returned. That prevents downstream `dart pub get` invocations from
/// consuming a syntactically valid but semantically incorrect package map.
pub(crate) fn rewrite_if_present(project: &Path) -> Result<()> {
    let wiring = project.join(WIRING_FILE);
    if !wiring.is_file() {
        return Ok(());
    }

    match rewrite(project, &wiring) {
        Ok(()) => Ok(()),
        Err(error) => {
            let removal = fs::remove_file(&wiring);
            match removal {
                Ok(()) => Err(error.context(format!(
                    "invalid Dart dependency wiring; removed {}",
                    wiring.display()
                ))),
                Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {
                    Err(error)
                }
                Err(remove_error) => Err(error.context(format!(
                    "invalid Dart dependency wiring and could not remove {}: {remove_error}",
                    wiring.display()
                ))),
            }
        }
    }
}

fn rewrite(project: &Path, wiring: &Path) -> Result<()> {
    let provisional = fs::read_to_string(wiring)
        .with_context(|| format!("reading provisional Dart wiring {}", wiring.display()))?;
    let installed_roots = parse_wired_paths(&provisional)?;
    let mut dependencies: BTreeMap<String, PathBuf> = BTreeMap::new();

    for relative in installed_roots {
        let package_root = project.join(&relative);
        let pubspec = package_root.join("pubspec.yaml");
        let package_name = declared_pub_name(&pubspec)?;
        if let Some(previous) = dependencies.insert(package_name.clone(), relative.clone()) {
            bail!(
                "installed Dart packages `{}` and `{}` both declare pub package name `{package_name}`",
                previous.display(),
                relative.display()
            );
        }
    }

    let mut rendered = String::from("dependencies:\n");
    for (name, relative) in dependencies {
        let portable = relative.to_string_lossy().replace('\\', "/");
        let quoted_path = serde_json::to_string(&portable)?;
        writeln!(&mut rendered, "  {name}:\n    path: {quoted_path}")?;
    }
    write_atomic(wiring, rendered.as_bytes())
}

/// Read either the historical merge fragment:
///
/// ```yaml
/// dart-lib:
///   path: zed_modules/acme/dart-lib
/// ```
///
/// or a complete dependency document. The wrapper form is accepted so this
/// finalizer remains idempotent for callers that invoke it more than once.
fn parse_wired_paths(document: &str) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    let mut dependency_keys = BTreeSet::new();
    let mut pending_package: Option<(String, usize)> = None;
    let mut document_mode = false;
    let mut saw_package = false;

    for (index, raw_line) in document.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let leading = raw_line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if raw_line.as_bytes()[..leading].contains(&b'\t') {
            bail!(
                "tabs are not allowed in Dart wiring indentation on line {} of {WIRING_FILE}",
                index + 1
            );
        }
        let indent = leading;

        if trimmed == "dependencies:" {
            if pending_package.is_some() || document_mode || saw_package {
                bail!(
                    "the top-level `dependencies:` mapping must appear exactly once before package entries in {WIRING_FILE}"
                );
            }
            if indent != 0 {
                bail!(
                    "the top-level `dependencies:` mapping must not be indented on line {} of {WIRING_FILE}",
                    index + 1
                );
            }
            document_mode = true;
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("path:") {
            let Some((package, package_indent)) = pending_package.take() else {
                bail!(
                    "Dart wiring path on line {} of {WIRING_FILE} has no package mapping",
                    index + 1
                );
            };
            if indent != package_indent + 2 {
                bail!(
                    "Dart wiring path for `{package}` on line {} of {WIRING_FILE} must be indented two spaces below its package key",
                    index + 1
                );
            }
            let scalar = parse_yaml_scalar(value.trim()).with_context(|| {
                format!("invalid path scalar on line {} of {WIRING_FILE}", index + 1)
            })?;
            let relative = PathBuf::from(scalar);
            validate_relative_install_path(&relative)?;
            paths.insert(relative);
            continue;
        }

        if let Some(package) = trimmed.strip_suffix(':') {
            if pending_package.is_some() {
                bail!(
                    "a Dart dependency mapping is missing `path:` before line {} of {WIRING_FILE}",
                    index + 1
                );
            }
            let package = package.trim();
            if package.is_empty() {
                bail!(
                    "empty Dart dependency key on line {} of {WIRING_FILE}",
                    index + 1
                );
            }
            let expected_indent = if document_mode { 2 } else { 0 };
            if indent != expected_indent {
                bail!(
                    "Dart dependency key `{package}` on line {} of {WIRING_FILE} must be indented {expected_indent} spaces",
                    index + 1
                );
            }
            if !dependency_keys.insert(package.to_string()) {
                bail!("Dart dependency key `{package}` appears more than once in {WIRING_FILE}");
            }
            saw_package = true;
            pending_package = Some((package.to_string(), indent));
            continue;
        }

        bail!(
            "unsupported Dart wiring content on line {} of {WIRING_FILE}: `{trimmed}`",
            index + 1
        );
    }

    if let Some((package, _)) = pending_package {
        bail!("Dart dependency mapping `{package}` in {WIRING_FILE} is missing `path:`");
    }
    if paths.is_empty() {
        bail!("{WIRING_FILE} contains no Dart dependency paths");
    }
    Ok(paths.into_iter().collect())
}

fn validate_relative_install_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!(
            "Dart dependency path `{}` must be a non-empty project-relative path",
            path.display()
        );
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => bail!(
                "Dart dependency path `{}` contains a current-directory component",
                path.display()
            ),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => bail!(
                "Dart dependency path `{}` escapes the managed project",
                path.display()
            ),
        }
    }
    Ok(())
}

fn declared_pub_name(pubspec: &Path) -> Result<String> {
    let text = fs::read_to_string(pubspec)
        .with_context(|| format!("Dart dependency is missing readable {}", pubspec.display()))?;
    let mut declared: Option<String> = None;

    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.strip_prefix('\u{feff}').unwrap_or(raw_line);
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }
        let Some(value) = trimmed.strip_prefix("name:") else {
            continue;
        };
        let name = parse_yaml_scalar(value.trim()).with_context(|| {
            format!(
                "invalid top-level `name` on line {} of {}",
                index + 1,
                pubspec.display()
            )
        })?;
        if !is_valid_pub_name(&name) {
            bail!(
                "Dart package name `{name}` in {} is invalid; expected lowercase letters, digits, and underscores beginning with a letter",
                pubspec.display()
            );
        }
        if let Some(previous) = &declared {
            bail!(
                "{} declares top-level pub package name more than once (`{previous}` and `{name}`)",
                pubspec.display()
            );
        }
        declared = Some(name);
    }

    declared.with_context(|| {
        format!(
            "{} has no valid top-level `name:`; Zed cannot generate Dart package wiring",
            pubspec.display()
        )
    })
}

fn is_valid_pub_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn parse_yaml_scalar(value: &str) -> Result<String> {
    if value.is_empty() {
        bail!("empty YAML scalar");
    }
    if value.starts_with('"') {
        return serde_json::from_str(value).context("invalid double-quoted YAML scalar");
    }
    if let Some(inner) = value.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            bail!("unterminated single-quoted YAML scalar");
        };
        return Ok(inner.replace("''", "'"));
    }

    let unquoted = value
        .split_once(" #")
        .map(|(head, _)| head)
        .unwrap_or(value)
        .trim();
    if unquoted.is_empty() {
        bail!("empty YAML scalar");
    }
    Ok(unquoted.to_string())
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("Dart wiring path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary Dart wiring in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("writing temporary Dart wiring for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary Dart wiring for {}", path.display()))?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(project: &Path, relative: &str, pubspec: &str) {
        let root = project.join(relative);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("pubspec.yaml"), pubspec).unwrap();
    }

    fn provisional(project: &Path, body: &str) -> PathBuf {
        let path = project.join(WIRING_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn rewrites_the_actual_legacy_fragment_to_declared_pub_identity() {
        let project = tempfile::tempdir().unwrap();
        package(
            project.path(),
            "zed_modules/zed-pkg-test/dart-lib",
            "name: zed_pkg_test_dart_lib\nversion: 1.0.0\n",
        );
        let path = provisional(
            project.path(),
            concat!(
                "# Generated by `zed install`.\n",
                "# Merge these entries under `dependencies:`.\n",
                "dart-lib:\n",
                "  path: zed_modules/zed-pkg-test/dart-lib\n"
            ),
        );

        rewrite_if_present(project.path()).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "dependencies:\n  zed_pkg_test_dart_lib:\n    path: \"zed_modules/zed-pkg-test/dart-lib\"\n"
        );
    }

    #[test]
    fn complete_document_is_idempotently_accepted() {
        let project = tempfile::tempdir().unwrap();
        package(
            project.path(),
            "zed_modules/acme/a-first",
            "name: a_first\n",
        );
        let path = provisional(
            project.path(),
            "dependencies:\n  a_first:\n    path: \"zed_modules/acme/a-first\"\n",
        );

        rewrite_if_present(project.path()).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "dependencies:\n  a_first:\n    path: \"zed_modules/acme/a-first\"\n"
        );
    }

    #[test]
    fn sorts_declared_names_and_accepts_quoted_pubspec_scalars() {
        let project = tempfile::tempdir().unwrap();
        package(
            project.path(),
            "zed_modules/acme/z-last",
            "name: 'z_last'\n",
        );
        package(
            project.path(),
            "zed_modules/acme/a-first",
            "name: \"a_first\"\n",
        );
        let path = provisional(
            project.path(),
            "z-last:\n  path: zed_modules/acme/z-last\na-first:\n  path: zed_modules/acme/a-first\n",
        );

        rewrite_if_present(project.path()).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            concat!(
                "dependencies:\n",
                "  a_first:\n",
                "    path: \"zed_modules/acme/a-first\"\n",
                "  z_last:\n",
                "    path: \"zed_modules/acme/z-last\"\n"
            )
        );
    }

    #[test]
    fn duplicate_native_names_fail_closed_and_remove_provisional_wiring() {
        let project = tempfile::tempdir().unwrap();
        package(project.path(), "zed_modules/acme/one", "name: same_name\n");
        package(project.path(), "zed_modules/acme/two", "name: same_name\n");
        let path = provisional(
            project.path(),
            "one:\n  path: zed_modules/acme/one\ntwo:\n  path: zed_modules/acme/two\n",
        );

        let error = rewrite_if_present(project.path()).unwrap_err();

        assert!(format!("{error:#}").contains("both declare pub package name `same_name`"));
        assert!(!path.exists());
    }

    #[test]
    fn invalid_or_missing_pub_name_fails_closed() {
        for pubspec in ["version: 1.0.0\n", "name: dart-lib\n"] {
            let project = tempfile::tempdir().unwrap();
            package(project.path(), "zed_modules/acme/dart-lib", pubspec);
            let path = provisional(
                project.path(),
                "dart-lib:\n  path: zed_modules/acme/dart-lib\n",
            );

            assert!(rewrite_if_present(project.path()).is_err());
            assert!(!path.exists());
        }
    }

    #[test]
    fn bom_comments_and_nested_names_do_not_confuse_declared_identity() {
        let project = tempfile::tempdir().unwrap();
        let pubspec = project.path().join("pubspec.yaml");
        fs::write(
            &pubspec,
            "\u{feff}---\n# package identity\nname: real_package # retained\nmetadata:\n  name: nested_decoy\n",
        )
        .unwrap();

        assert_eq!(declared_pub_name(&pubspec).unwrap(), "real_package");
    }

    #[test]
    fn duplicate_top_level_pub_names_are_rejected() {
        let project = tempfile::tempdir().unwrap();
        let pubspec = project.path().join("pubspec.yaml");
        fs::write(&pubspec, "name: first_name\nname: second_name\n").unwrap();

        let error = declared_pub_name(&pubspec).unwrap_err().to_string();
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn unsafe_install_paths_are_rejected() {
        for path_value in ["../outside", "./inside", "/absolute"] {
            let error =
                parse_wired_paths(&format!("package:\n  path: {path_value}\n")).unwrap_err();
            assert!(format!("{error:#}").contains("Dart dependency path"));
        }
    }

    #[test]
    fn malformed_fragment_structure_is_rejected() {
        for document in [
            "path: zed_modules/acme/pkg\n",
            "package:\nother:\n  path: zed_modules/acme/other\n",
            "package:\n    path: zed_modules/acme/pkg\n",
            "dependencies:\n    package:\n      path: zed_modules/acme/pkg\n",
            "package:\n  unsupported: value\n",
            "  package:\n    path: zed_modules/acme/pkg\n",
            "dependencies:\npackage:\n  path: zed_modules/acme/pkg\n",
            "dependencies:\ndependencies:\n",
        ] {
            assert!(parse_wired_paths(document).is_err(), "{document}");
        }
    }

    #[test]
    fn duplicate_provisional_keys_are_rejected() {
        let error = parse_wired_paths(
            "package:\n  path: zed_modules/acme/one\npackage:\n  path: zed_modules/acme/two\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("appears more than once"), "{error}");
    }

    #[test]
    fn absent_dart_wiring_is_a_no_op() {
        let project = tempfile::tempdir().unwrap();
        rewrite_if_present(project.path()).unwrap();
    }
}
