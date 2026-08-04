#!/usr/bin/env python3
"""Validate every tool selector before scalar/table export selection."""

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
    """    ensure!(
        version.extensions.is_empty(),
        "unsupported `{field}.extensions`: no certified mise mapping exists"
    );

    if version.options.is_empty() && requirement.platforms.is_empty() {
""",
    """    ensure!(
        version.extensions.is_empty(),
        "unsupported `{field}.extensions`: no certified mise mapping exists"
    );
    validate_selector_requirement(&requirement.requirement, field)?;

    if version.options.is_empty() && requirement.platforms.is_empty() {
""",
    "selector validation before scalar fast path",
)

anchor = """fn insert_selector(table: &mut toml::Table, requirement: &str, field: &str) -> Result<()> {
"""
helper = """fn validate_selector_requirement(requirement: &str, field: &str) -> Result<()> {
    for selector in ["path", "prefix", "ref", "env"] {
        let prefix = format!("{selector}:");
        if let Some(value) = requirement.strip_prefix(&prefix) {
            let selector_field = format!("{field}.{selector}");
            ensure_clean(value, &selector_field)?;
            if selector == "path" {
                validate_relative_argument(Path::new(value), &selector_field)?;
            }
            return Ok(());
        }
    }
    Ok(())
}

fn insert_selector(table: &mut toml::Table, requirement: &str, field: &str) -> Result<()> {
"""
text = replace_once(text, anchor, helper, "selector validation helper")

old_test_anchor = """    #[test]
    fn task_invocations_and_shell_argument_vectors_fail_closed() {
"""
new_test = r'''    #[test]
    fn path_selectors_are_portable_even_on_the_scalar_fast_path() {
        for requirement in [
            "path:../tool",
            "path:/opt/tool",
            r"path:C:\\tool",
            r"path:\\\\server\\share\\tool",
            "path:~/tool",
            "path:$HOME/tool",
            "path:${HOME}/tool",
            "path:%USERPROFILE%/tool",
        ] {
            let mut plan = simple_plan();
            plan.tools.get_mut("node").unwrap().versions_mut()[0]
                .requirement
                .requirement = requirement.to_string();
            let error = render_mise_config(&plan).unwrap_err().to_string();
            assert!(
                error.contains("project-relative") || error.contains("cannot escape"),
                "unexpected error for {requirement}: {error}"
            );
        }

        let mut plan = simple_plan();
        plan.tools.get_mut("node").unwrap().versions_mut()[0]
            .requirement
            .requirement = "path:vendor/node".to_string();
        let rendered = render_mise_config(&plan).unwrap();
        let value: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(value["tools"]["node"].as_str(), Some("path:vendor/node"));
    }

    #[test]
    fn task_invocations_and_shell_argument_vectors_fail_closed() {
'''
text = replace_once(text, old_test_anchor, new_test, "path selector regressions")

path.write_text(text, encoding="utf-8")

docs = Path("docs/mise-export.md")
text = docs.read_text(encoding="utf-8")
text = text.replace(
    "- `version`, `path`, `prefix`, and `ref` selectors;\n",
    "- `version`, portable project-relative `path`, `prefix`, and `ref` selectors;\n",
    1,
)
text = text.replace(
    "- project-relative/drive/UNC/parent/symlink path hardening;\n",
    "- project-relative/drive/UNC/home/parent/symlink path hardening for outputs and `path:` selectors;\n",
    1,
)
docs.write_text(text, encoding="utf-8")
