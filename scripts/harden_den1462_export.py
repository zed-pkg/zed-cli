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
    let plan_folded = plan_relative.to_ascii_lowercase();
    let output_folded = output_relative.to_ascii_lowercase();
    ensure!(
        output_folded != plan_folded,
        "mise output `{output_relative}` cannot overwrite its source environment plan"
    );
    ensure!(
        output_folded != EXPORT_STATE_PATH.to_ascii_lowercase(),
        "mise output cannot target reserved export state `{EXPORT_STATE_PATH}`"
    );
    let staging_folded = STAGING_DIR.to_ascii_lowercase();
    let staging_prefix = format!("{staging_folded}/");
    for (kind, relative, folded) in [
        ("environment plan", plan_relative, plan_folded.as_str()),
        ("mise output", output_relative, output_folded.as_str()),
    ] {
        ensure!(
            folded != staging_folded && !folded.starts_with(&staging_prefix),
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

old_state_load = """    let mut state = load_state(&state_path)?;
    let current = read_regular_file(output_path, "mise output")?;
"""
new_state_load = """    let mut state = load_state(&state_path)?;
    if let Some(existing) = state.outputs.keys().find(|existing| {
        existing.eq_ignore_ascii_case(output_relative) && existing.as_str() != output_relative
    }) {
        bail!(
            "portable mise export path collision: `{output_relative}` conflicts with existing state key `{existing}`"
        );
    }
    let current = read_regular_file(output_path, "mise output")?;
"""
text = replace_once(text, old_state_load, new_state_load, "portable state-key collision")

render_anchor = """fn render_mise_config(plan: &EnvironmentPlanV2) -> Result<String> {
    ensure!(
"""
render_normalized = """fn render_mise_config(plan: &EnvironmentPlanV2) -> Result<String> {
    let plan = plan.normalized();
    ensure!(
"""
text = replace_once(text, render_anchor, render_normalized, "semantic renderer normalization")

text = replace_once(
    text,
    "export_value_map(values, field, true, false)?",
    "export_value_map(values, field, true, true)?",
    "recursive sensitive-key validation",
)

old_deterministic_tail = """        assert_eq!(commands[0].as_str(), Some("zed install --frozen"));
        assert_eq!(commands[1].as_str(), Some("cargo check"));
    }

    #[test]
    fn write_check_and_unchanged_state_are_conflict_safe() {
"""
new_deterministic_tail = """        assert_eq!(commands[0].as_str(), Some("zed install --frozen"));
        assert_eq!(commands[1].as_str(), Some("cargo check"));
    }

    #[test]
    fn set_like_presentation_order_does_not_change_semantic_output() {
        let mut first = simple_plan();
        first.platforms = vec![
            "macos-arm64".to_string(),
            "linux-x64".to_string(),
            "macos-arm64".to_string(),
        ];
        first.tasks.get_mut("setup").unwrap().aliases = vec![
            "z-bootstrap".to_string(),
            "a-bootstrap".to_string(),
            "z-bootstrap".to_string(),
        ];
        first.tasks.get_mut("setup").unwrap().depends = vec![
            "prepare".to_string(),
            "prepare".to_string(),
        ];

        let mut second = first.clone();
        second.platforms.reverse();
        second.tasks.get_mut("setup").unwrap().aliases.reverse();
        second.tasks.get_mut("setup").unwrap().depends.reverse();

        assert_eq!(render_mise_config(&first).unwrap(), render_mise_config(&second).unwrap());
        assert_eq!(digest_plan(&first).unwrap(), digest_plan(&second).unwrap());
    }

    #[test]
    fn write_check_and_unchanged_state_are_conflict_safe() {
"""
text = replace_once(
    text,
    old_deterministic_tail,
    new_deterministic_tail,
    "semantic presentation-order regression",
)

old_unchanged_tail = """        assert_eq!(unchanged.action, MiseExportAction::Unchanged);
    }

    #[test]
    fn hand_edits_and_unowned_outputs_are_never_overwritten() {
"""
new_unchanged_tail = """        assert_eq!(unchanged.action, MiseExportAction::Unchanged);

        let collision = export_mise(
            temp.path(),
            &plan_path,
            Path::new(".MISE.TOML"),
            MiseExportMode::Write,
        )
        .unwrap_err();
        assert!(collision.to_string().contains("portable mise export path collision"));
    }

    #[test]
    fn hand_edits_and_unowned_outputs_are_never_overwritten() {
"""
text = replace_once(text, old_unchanged_tail, new_unchanged_tail, "case-folded state collision")

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
            "ZED-ENV.TOML",
            ".zed/mise-export-state.json",
            ".ZED/MISE-EXPORT-STATE.JSON",
            ".zpkg-staging/mise.toml",
            ".ZPKG-STAGING/mise.toml",
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
text = text.replace(
    "Output and state changes share `ProjectTransaction`, including crash recovery\nand rollback.",
    "Output and state changes share `ProjectTransaction`, including crash recovery\nand rollback. Plan, output, state, and staging identities are compared with\nportable ASCII case-folding so a Linux-generated ownership file cannot become\nambiguous on Windows or macOS.",
    1,
)
text = text.replace(
    "Print,\ncheck, and write modes all use the same renderer.",
    "Print,\ncheck, and write modes all render the normalized semantic plan, so set-like\npresentation order cannot diverge under one plan digest.",
    1,
)
docs.write_text(text, encoding="utf-8")
