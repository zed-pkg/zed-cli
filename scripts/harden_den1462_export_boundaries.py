#!/usr/bin/env python3
"""Temporary materializer for DEN-1462 export ownership/security boundaries."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


source = Path("src/mise_export.rs")
text = source.read_text(encoding="utf-8")

if "ensure_export_path_separation(&plan_relative, &output_relative)?;" not in text:
    anchor = '''    let (output_path, output_relative) =
        resolve_project_path(&root, output_arg, "mise output", false)?;

    let plan = read_plan(&plan_path)?;
'''
    replacement = '''    let (output_path, output_relative) =
        resolve_project_path(&root, output_arg, "mise output", false)?;
    ensure_export_path_separation(&plan_relative, &output_relative)?;

    let plan = read_plan(&plan_path)?;
'''
    text = replace_once(text, anchor, replacement, "path separation call")

if "ensure_no_symlink_existing_prefix(root, Path::new(EXPORT_STATE_PATH)" not in text:
    anchor = '''    let state_path = root.join(EXPORT_STATE_PATH);
    let mut state = load_state(&state_path)?;
'''
    replacement = '''    let state_path = root.join(EXPORT_STATE_PATH);
    ensure_no_symlink_existing_prefix(
        root,
        Path::new(EXPORT_STATE_PATH),
        "mise export state",
    )?;
    let mut state = load_state(&state_path)?;
'''
    text = replace_once(text, anchor, replacement, "state symlink boundary")

# Recursively retain the sensitive-key policy through arrays/tables and tool options.
text = text.replace(
    'environment_value_to_toml(value, &format!("{field}.options.{name}"), true)?',
    'environment_value_to_toml(value, &format!("{field}.options.{name}"), true, true)?',
)
text = text.replace(
    'environment_value_to_toml(value, &format!("{field}.{name}"), allow_complex)?',
    'environment_value_to_toml(\n                value,\n                &format!("{field}.{name}"),\n                allow_complex,\n                reject_sensitive,\n            )?',
)
if "reject_sensitive: bool," not in text[text.index("fn environment_value_to_toml(") : text.index("fn insert_optional_string")]:
    text = replace_once(
        text,
        '''fn environment_value_to_toml(
    value: &EnvironmentValue,
    field: &str,
    allow_complex: bool,
) -> Result<toml::Value> {
''',
        '''fn environment_value_to_toml(
    value: &EnvironmentValue,
    field: &str,
    allow_complex: bool,
    reject_sensitive: bool,
) -> Result<toml::Value> {
''',
        "environment value signature",
    )
text = text.replace(
    'environment_value_to_toml(value, &format!("{field}[{index}]"), true)',
    'environment_value_to_toml(\n                            value,\n                            &format!("{field}[{index}]"),\n                            true,\n                            reject_sensitive,\n                        )',
)
text = text.replace(
    'toml::Value::Table(export_value_map(values, field, true, false)?)',
    'toml::Value::Table(export_value_map(\n                values,\n                field,\n                true,\n                reject_sensitive,\n            )?)',
)

if "fn ensure_export_path_separation(" not in text:
    anchor = "fn validate_sha256(value: &str, field: &str) -> Result<()> {\n"
    helpers = '''fn ensure_export_path_separation(plan: &str, output: &str) -> Result<()> {
    ensure!(
        !portable_path_eq(plan, output),
        "mise output `{output}` must be different from environment plan `{plan}`"
    );
    ensure!(
        !portable_path_eq(output, EXPORT_STATE_PATH),
        "mise output path `{output}` is reserved for Zed export ownership state"
    );
    ensure!(
        !portable_path_eq(plan, EXPORT_STATE_PATH),
        "environment plan path `{plan}` is reserved for Zed export ownership state"
    );
    Ok(())
}

fn portable_path_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

'''
    text = replace_once(text, anchor, helpers + anchor, "path separation helpers")

# For generated output/state, reject symlinks in every existing prefix but allow
# a missing suffix to be created atomically by the transaction.
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
    text = replace_once(text, anchor, helper + anchor, "existing-prefix helper")

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

# Unit regressions.
if "fn nested_sensitive_fields_fail_closed()" not in text:
    anchor = '''    #[test]
    fn task_invocations_and_shell_argument_vectors_fail_closed() {
'''
    tests = '''    #[test]
    fn nested_sensitive_fields_fail_closed() {
        let mut plan = simple_plan();
        plan.vars.insert(
            "release".to_string(),
            EnvironmentValue::Table(BTreeMap::from([(
                "api_token".to_string(),
                EnvironmentValue::String("plaintext".to_string()),
            )])),
        );
        let error = render_mise_config(&plan).unwrap_err();
        assert!(error.to_string().contains("vars.release.api_token"));

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
    text = replace_once(text, anchor, tests + anchor, "security unit tests")

source.write_text(text, encoding="utf-8")

integration = Path("tests/mise_export_cli.rs")
text = integration.read_text(encoding="utf-8")
if "fn reserved_and_alias_paths_fail_before_mutation()" not in text:
    addition = r'''

#[test]
fn reserved_and_alias_paths_fail_before_mutation() {
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
            "zed-env.json",
            "--write",
        ],
    );
    assert!(!alias.status.success());
    assert!(String::from_utf8_lossy(&alias.stderr).contains("must be different"));
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
    assert!(String::from_utf8_lossy(&reserved.stderr).contains("reserved"));
    assert!(!project.join(".zed/mise-export-state.json").exists());
    assert!(!project.join(".ZED/MISE-EXPORT-STATE.JSON").exists());
}
'''
    text += addition
integration.write_text(text, encoding="utf-8")

docs = Path("docs/mise-export.md")
text = docs.read_text(encoding="utf-8")
if "## Reserved paths and recursive secret checks" not in text:
    anchor = "## Fail-closed boundary\n"
    section = '''## Reserved paths and recursive secret checks

The output must be distinct from the input plan and cannot use the reserved `.zed/mise-export-state.json` ownership path, including case-only aliases that would collide on common Windows filesystems. Export state and generated output paths reject symlinks in every existing parent component; missing safe parent directories may be created inside the project transaction.

Secret-like key detection is recursive through nested vars, task values, arrays/tables, and tool options. A nested key such as `release.api_token` or `options.config.password` is rejected with its exact field path rather than being committed as plaintext.

'''
    text = replace_once(text, anchor, section + anchor, "security documentation")
docs.write_text(text, encoding="utf-8")
