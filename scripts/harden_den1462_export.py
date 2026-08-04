#!/usr/bin/env python3
"""Temporary idempotent semantic hardening for deterministic mise export."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("src/mise_export.rs")
text = path.read_text(encoding="utf-8")

text = replace_once(
    text,
    "use crate::transaction::ProjectTransaction;\n",
    "use crate::transaction::{ProjectTransaction, STAGING_DIR};\n",
    "transaction staging import",
)

old_paths = """    let (output_path, output_relative) =
        resolve_project_path(&root, output_arg, "mise output", false)?;

    let plan = read_plan(&plan_path)?;
"""
new_paths = """    let (output_path, output_relative) =
        resolve_project_path(&root, output_arg, "mise output", false)?;
    validate_export_path_relationships(
        &plan_path,
        &plan_relative,
        &output_path,
        &output_relative,
    )?;

    let plan = read_plan(&plan_path)?;
"""
text = replace_once(text, old_paths, new_paths, "export path relationship validation")

read_plan_anchor = """fn read_plan(path: &Path) -> Result<EnvironmentPlanV2> {
"""
relationship_helper = """fn validate_export_path_relationships(
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

    if output_path.exists() {
        let canonical_output = output_path
            .canonicalize()
            .with_context(|| format!("failed to resolve mise output {}", output_path.display()))?;
        let canonical_plan = plan_path
            .canonicalize()
            .with_context(|| format!("failed to resolve environment plan {}", plan_path.display()))?;
        ensure!(
            canonical_output != canonical_plan,
            "mise output resolves to its source environment plan: {}",
            output_path.display()
        );
    }
    Ok(())
}

fn read_plan(path: &Path) -> Result<EnvironmentPlanV2> {
"""
text = replace_once(text, read_plan_anchor, relationship_helper, "path relationship helper")

text = replace_once(
    text,
    "export_value_map(values, field, true, false)?",
    "export_value_map(values, field, true, true)?",
    "recursive sensitive-key validation",
)

old_sensitive_tail = """        assert!(render_mise_config(&plan)
            .unwrap_err()
            .to_string()
            .contains("complex environment value"));
    }
"""
new_sensitive_tail = """        assert!(render_mise_config(&plan)
            .unwrap_err()
            .to_string()
            .contains("complex environment value"));

        let mut plan = simple_plan();
        plan.vars.insert(
            "release".to_string(),
            EnvironmentValue::Table(BTreeMap::from([(
                "api_token".to_string(),
                EnvironmentValue::String("plaintext".to_string()),
            )])),
        );
        assert!(render_mise_config(&plan)
            .unwrap_err()
            .to_string()
            .contains("vars.release.api_token"));
    }
"""
text = replace_once(text, old_sensitive_tail, new_sensitive_tail, "nested secret regression")

old_paths_loop = """        for output in ["../mise.toml", "C:\\\\mise.toml", "\\\\\\\\server\\\\share\\\\mise.toml"] {
"""
new_paths_loop = """        for output in [
            "../mise.toml",
            "C:\\\\mise.toml",
            "\\\\\\\\server\\\\share\\\\mise.toml",
            "zed-env.toml",
            ".zed/mise-export-state.json",
            ".zpkg-staging/mise.toml",
        ] {
"""
text = replace_once(text, old_paths_loop, new_paths_loop, "reserved-path regressions")

path.write_text(text, encoding="utf-8")

docs = Path("docs/mise-export.md")
text = docs.read_text(encoding="utf-8")
text = text.replace(
    "- project-relative/drive/UNC/parent/symlink path hardening; and\n",
    "- project-relative/drive/UNC/parent/symlink path hardening;\n- refusal to target the source plan, export sidecar, or transaction staging; and\n",
    1,
)
text = text.replace(
    "look credential-bearing, including password, secret, token, private/access key,\nAPI key, credential, and authorization names.",
    "look credential-bearing at any nesting depth, including password, secret, token,\nprivate/access key, API key, credential, and authorization names.",
    1,
)
docs.write_text(text, encoding="utf-8")
