from pathlib import Path

path = Path('src/mise.rs')
text = path.read_text()

old = '''#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct MiseConfig {
    #[serde(default)]
    tools: BTreeMap<String, ToolValue>,
    #[serde(default)]
    env: BTreeMap<String, MiseValue>,
    #[serde(default)]
    tasks: BTreeMap<String, MiseValue>,
    #[serde(default)]
    settings: BTreeMap<String, MiseValue>,
}
'''
new = '''#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct MiseConfig {
    #[serde(default)]
    tools: BTreeMap<String, ToolValue>,
    #[serde(default)]
    env: BTreeMap<String, MiseValue>,
    #[serde(default)]
    vars: BTreeMap<String, MiseValue>,
    #[serde(default)]
    tasks: BTreeMap<String, MiseValue>,
    #[serde(default)]
    settings: BTreeMap<String, MiseValue>,
    /// Manager-native sections that Zed does not execute yet. They remain
    /// lossless and inert instead of being silently discarded.
    #[serde(flatten, default)]
    unknown: BTreeMap<String, toml::Value>,
}
'''
assert old in text, 'MiseConfig block not found'
text = text.replace(old, new, 1)

old = '''            Self::Versions(values) => {
                values.sort();
                values.dedup();
            }
            Self::Options(options) => {
                if let Some(version) = &mut options.version {
                    let mut values = version.as_vec();
                    values.sort();
                    values.dedup();
                    options.version = Some(OneOrManyString::Many(values));
                }
'''
new = '''            Self::Versions(values) => dedup_preserving_order(values),
            Self::Options(options) => {
                if let Some(version) = &mut options.version {
                    let mut values = version.as_vec();
                    dedup_preserving_order(&mut values);
                    options.version = Some(OneOrManyString::Many(values));
                }
'''
assert old in text, 'version normalization block not found'
text = text.replace(old, new, 1)

old = '''        plan.extensions.insert(
            "mise.env".to_string(),
            serde_json::to_value(&config.env).context("failed to encode mise env")?,
        );
        plan.extensions.insert(
            "mise.tasks".to_string(),
            serde_json::to_value(&config.tasks).context("failed to encode mise tasks")?,
        );
'''
new = '''        plan.extensions.insert(
            "mise.env".to_string(),
            serde_json::to_value(&config.env).context("failed to encode mise env")?,
        );
        plan.extensions.insert(
            "mise.vars".to_string(),
            serde_json::to_value(&config.vars).context("failed to encode mise vars")?,
        );
        plan.extensions.insert(
            "mise.tasks".to_string(),
            serde_json::to_value(&config.tasks).context("failed to encode mise tasks")?,
        );
'''
assert old in text, 'env/tasks extension block not found'
text = text.replace(old, new, 1)

old = '''        plan.extensions.insert(
            "mise.settings".to_string(),
            serde_json::to_value(&config.settings).context("failed to encode mise settings")?,
        );
        plan.extensions.insert(
            "mise.source_sha256".to_string(),
'''
new = '''        plan.extensions.insert(
            "mise.settings".to_string(),
            serde_json::to_value(&config.settings).context("failed to encode mise settings")?,
        );
        plan.extensions.insert(
            "mise.unknown_top_level".to_string(),
            serde_json::to_value(&config.unknown)
                .context("failed to encode unsupported mise sections")?,
        );
        plan.extensions.insert(
            "mise.source_sha256".to_string(),
'''
assert old in text, 'settings/source extension block not found'
text = text.replace(old, new, 1)

old = '''fn ensure_project_local_path(path: &str, field: &str) -> Result<()> {
    let path = path.trim();
    ensure!(!path.is_empty(), "{field} cannot be empty");
    let parsed = Path::new(path);
    ensure!(!parsed.is_absolute(), "{field} must be project-relative");
    ensure!(
        parsed
            .components()
            .all(|component| !matches!(component, Component::ParentDir)),
        "{field} cannot escape the project root"
    );
    Ok(())
}
'''
new = '''fn ensure_project_local_path(path: &str, field: &str) -> Result<()> {
    let path = path.trim();
    ensure!(!path.is_empty(), "{field} cannot be empty");
    let parsed = Path::new(path);
    let has_windows_drive = path.as_bytes().get(1).is_some_and(|value| *value == b':')
        && path
            .as_bytes()
            .first()
            .is_some_and(|value| value.is_ascii_alphabetic());
    let has_parent = path.split(['/', '\\']).any(|part| part == "..");
    ensure!(
        !parsed.is_absolute()
            && !has_windows_drive
            && !path.starts_with('~')
            && !path.starts_with("$HOME")
            && !path.starts_with("${HOME}")
            && !path.starts_with("%USERPROFILE%")
            && !path.starts_with("\\\\")
            && !path.starts_with("//"),
        "{field} must be project-relative on every supported platform"
    );
    ensure!(!has_parent, "{field} cannot escape the project root");
    ensure!(
        parsed
            .components()
            .all(|component| !matches!(component, Component::ParentDir)),
        "{field} cannot escape the project root"
    );
    Ok(())
}

fn dedup_preserving_order(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}
'''
assert old in text, 'path validation block not found'
text = text.replace(old, new, 1)
path.write_text(text)

test_path = Path('tests/mise_adapter.rs')
tests = test_path.read_text()
insert = r'''

#[test]
fn multi_version_order_is_semantic_and_preserved() {
    let root = temp_project("version-order");
    write(
        &root.join(".mise.toml"),
        r#"
[tools]
node = ["22", "20", "22"]
"#,
    );

    let imported = import_mise(MiseAdapter::discover(&root).unwrap()).unwrap();
    assert!(imported.source_document.contains("[\"22\", \"20\"]"));
    assert!(!imported.source_document.contains("[\"20\", \"22\"]"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_top_level_sections_remain_lossless_and_inert() {
    let root = temp_project("unknown-sections");
    write(
        &root.join(".mise.toml"),
        r#"
[tools]
node = "22"

[hooks]
postinstall = "echo never-run-by-import"

[plugins]
custom = "https://example.invalid/plugin.git"
"#,
    );

    let imported = import_mise(MiseAdapter::discover(&root).unwrap()).unwrap();
    let unknown = imported
        .plan
        .extensions
        .get("mise.unknown_top_level")
        .expect("unknown sections are represented");
    assert_eq!(unknown["hooks"]["postinstall"], "echo never-run-by-import");
    assert_eq!(
        unknown["plugins"]["custom"],
        "https://example.invalid/plugin.git"
    );
    assert!(imported.source_document.contains("postinstall"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vars_are_preserved_for_future_native_template_evaluation() {
    let root = temp_project("vars");
    write(
        &root.join(".mise.toml"),
        r#"
[vars]
channel = "stable"
retries = 3
"#,
    );

    let imported = import_mise(MiseAdapter::discover(&root).unwrap()).unwrap();
    let vars = imported.plan.extensions.get("mise.vars").unwrap();
    assert_eq!(vars["channel"], "stable");
    assert_eq!(vars["retries"], 3);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn portable_path_validation_rejects_windows_and_unc_absolute_paths() {
    let root = temp_project("portable-paths");
    for path in [r"C:\Users\alex\mise.toml", r"\\server\share\mise.toml"] {
        let adapter = MiseAdapter::for_source(&root, path).unwrap();
        let error = import_mise(adapter).unwrap_err().to_string();
        assert!(error.contains("project-relative"));
    }
    fs::remove_dir_all(root).unwrap();
}
'''
assert 'fn multi_version_order_is_semantic_and_preserved' not in tests
test_path.write_text(tests + insert)

doc_path = Path('docs/mise.md')
doc = doc_path.read_text()
needle = '- Project-local config only; global/user configuration is ignored.\n'
replacement = '''- Project-local config only; global/user configuration is ignored.\n- Unknown top-level mise sections are preserved losslessly under `mise.unknown_top_level` and remain inert; import never executes hooks or plugins.\n- Multi-version order is preserved because PATH/default selection can be order-sensitive.\n'''
assert needle in doc
doc_path.write_text(doc.replace(needle, replacement, 1))
