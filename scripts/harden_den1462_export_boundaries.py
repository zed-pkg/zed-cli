#!/usr/bin/env python3
"""Temporary residual hardener applied after the base DEN-1462 materializer."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


source = Path("src/mise_export.rs")
text = source.read_text(encoding="utf-8")

# The base PR rejects exact aliases and transaction staging. Strengthen that
# contract for portable case-insensitive filesystems and reserve the state path
# for both input and output.
old_relationships = '''fn validate_export_path_relationships(
    plan_path: &Path,
    plan_relative: &str,
    output_path: &Path,
    output_relative: &str,
) -> Result<()> {
    ensure!(
        output_relative != plan_relative,
        "mise output `{output_relative}` cannot overwrite its source environment plan"
    );
    ensure!(
        output_relative != EXPORT_STATE_PATH,
        "mise output cannot target reserved export state `{EXPORT_STATE_PATH}`"
    );
    let staging_prefix = format!("{STAGING_DIR}/");
    for (kind, relative) in [
        ("environment plan", plan_relative),
        ("mise output", output_relative),
    ] {
        ensure!(
            relative != STAGING_DIR && !relative.starts_with(&staging_prefix),
            "{kind} cannot target reserved transaction staging `{STAGING_DIR}`: `{relative}`"
        );
    }
'''
new_relationships = '''fn validate_export_path_relationships(
    plan_path: &Path,
    plan_relative: &str,
    output_path: &Path,
    output_relative: &str,
) -> Result<()> {
    ensure!(
        !portable_path_eq(output_relative, plan_relative),
        "mise output `{output_relative}` cannot overwrite its source environment plan"
    );
    for (kind, relative) in [
        ("environment plan", plan_relative),
        ("mise output", output_relative),
    ] {
        ensure!(
            !portable_path_eq(relative, EXPORT_STATE_PATH),
            "{kind} cannot target reserved export state `{EXPORT_STATE_PATH}`"
        );
        ensure!(
            !reserved_path_or_child(relative, STAGING_DIR),
            "{kind} cannot target reserved transaction staging `{STAGING_DIR}`: `{relative}`"
        );
    }
'''
if "!portable_path_eq(output_relative, plan_relative)" not in text:
    text = replace_once(text, old_relationships, new_relationships, "portable path relationships")

if "fn portable_path_eq(" not in text:
    anchor = "fn read_plan(path: &Path) -> Result<EnvironmentPlanV2> {\n"
    helpers = '''fn portable_path_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn reserved_path_or_child(path: &str, reserved: &str) -> bool {
    portable_path_eq(path, reserved)
        || path
            .get(..reserved.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(reserved))
            && path.as_bytes().get(reserved.len()) == Some(&b'/')
}

'''
    text = replace_once(text, anchor, helpers + anchor, "portable path helpers")

# Protect every existing prefix of generated files, while allowing a missing
# safe suffix to be created within the project transaction.
if "fn ensure_no_symlink_existing_prefix(" not in text:
    anchor = "fn ensure_no_symlink_components(\n"
    helper = '''fn ensure_no_symlink_existing_prefix(
    root: &Path,
    relative: &Path,
    kind: &str,
) -> Result<()> {
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    let mut suffix_missing = false;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(value) = component else {
            bail!("{kind} must be normalized and project-relative");
        };
        current.push(value);
        if suffix_missing {
            continue;
        }
        let leaf = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "{kind} crosses a symlink at {}",
                    current.display()
                );
                if !leaf {
                    ensure!(
                        metadata.is_dir(),
                        "{kind} parent is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix_missing = true;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {kind} {}", current.display()));
            }
        }
    }
    Ok(())
}

'''
    text = replace_once(text, anchor, helper + anchor, "existing-prefix symlink helper")

old_resolve = '''    let relative = validate_relative_argument(requested, kind)?;
    ensure_no_symlink_components(root, &relative, kind, must_exist)?;
    let path = root.join(&relative);
'''
new_resolve = '''    let relative = validate_relative_argument(requested, kind)?;
    if must_exist {
        ensure_no_symlink_components(root, &relative, kind, true)?;
    } else {
        ensure_no_symlink_existing_prefix(root, &relative, kind)?;
    }
    let path = root.join(&relative);
'''
if old_resolve in text:
    text = replace_once(text, old_resolve, new_resolve, "generated path prefix validation")

state_anchor = '''    let state_path = root.join(EXPORT_STATE_PATH);
    let mut state = load_state(&state_path)?;
'''
state_replacement = '''    let state_path = root.join(EXPORT_STATE_PATH);
    ensure_no_symlink_existing_prefix(
        root,
        Path::new(EXPORT_STATE_PATH),
        "mise export state",
    )?;
    let mut state = load_state(&state_path)?;
'''
if "Path::new(EXPORT_STATE_PATH),\n        \"mise export state\"" not in text:
    text = replace_once(text, state_anchor, state_replacement, "state prefix validation")

# Add residual regressions not present in the base hardening.
if "fn nested_tool_option_secrets_fail_closed()" not in text:
    anchor = '''    #[test]
    fn task_invocations_and_shell_argument_vectors_fail_closed() {
'''
    tests = '''    #[test]
    fn nested_tool_option_secrets_fail_closed() {
        let mut plan = simple_plan();
        plan.tools.get_mut("node").unwrap().versions_mut()[0]
            .options
            .insert(
                "config".to_string(),
                EnvironmentValue::Table(BTreeMap::from([(
                    "password".to_string(),
                    EnvironmentValue::String("plaintext".to_string()),
                )])),
            );
        let error = render_mise_config(&plan).unwrap_err();
        assert!(error
            .to_string()
            .contains("tools.node.versions[0].options.config.password"));
    }

    #[test]
    fn write_can_create_missing_safe_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let plan_path = write_plan(temp.path(), &simple_plan());
        let report = export_mise(
            temp.path(),
            &plan_path,
            Path::new("generated/.mise.toml"),
            MiseExportMode::Write,
        )
        .unwrap();
        assert_eq!(report.action, MiseExportAction::Written);
        assert!(temp.path().join("generated/.mise.toml").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn export_state_rejects_symlinked_zed_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let plan_path = write_plan(temp.path(), &simple_plan());
        symlink(outside.path(), temp.path().join(".zed")).unwrap();

        let error = export_mise(
            temp.path(),
            &plan_path,
            Path::new(".mise.toml"),
            MiseExportMode::Write,
        )
        .unwrap_err();
        assert!(error.to_string().contains("mise export state crosses a symlink"));
        assert!(!temp.path().join(".mise.toml").exists());
        assert!(!outside.path().join("mise-export-state.json").exists());
    }

'''
    text = replace_once(text, anchor, tests + anchor, "residual security unit tests")

source.write_text(text, encoding="utf-8")

integration = Path("tests/mise_export_cli.rs")
text = integration.read_text(encoding="utf-8")
if "fn case_only_reserved_paths_fail_before_mutation()" not in text:
    addition = r'''

#[test]
fn case_only_reserved_paths_fail_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);
    let original_plan = fs::read(project.join("zed-env.json")).unwrap();

    let alias = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            "ZED-ENV.JSON",
            "--write",
        ],
    );
    assert!(!alias.status.success());
    assert!(String::from_utf8_lossy(&alias.stderr).contains("cannot overwrite"));
    assert_eq!(fs::read(project.join("zed-env.json")).unwrap(), original_plan);

    let reserved = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            ".ZED/MISE-EXPORT-STATE.JSON",
            "--write",
        ],
    );
    assert!(!reserved.status.success());
    assert!(String::from_utf8_lossy(&reserved.stderr).contains("reserved export state"));
    assert!(!project.join(".zed/mise-export-state.json").exists());
    assert!(!project.join(".ZED/MISE-EXPORT-STATE.JSON").exists());
}
'''
    text += addition
integration.write_text(text, encoding="utf-8")

docs = Path("docs/mise-export.md")
text = docs.read_text(encoding="utf-8")
if "case-only aliases" not in text:
    anchor = "## Fail-closed boundary\n"
    section = '''## Portable ownership boundaries

Plan, output, export-state, and transaction-staging paths are compared case-insensitively so a candidate that is safe on Linux cannot collide when checked out on a typical Windows filesystem. Generated output and export-state paths reject symlinks in every existing parent component; safe missing parent directories may still be created inside the project transaction.

Sensitive-key validation remains recursive through nested vars and tool-option tables. Exact diagnostics identify the rejected nested field instead of committing plaintext.

'''
    text = replace_once(text, anchor, section + anchor, "portable ownership documentation")
docs.write_text(text, encoding="utf-8")
