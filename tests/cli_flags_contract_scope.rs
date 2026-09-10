use std::fs;

use flags2env::BundledFlags2Env;
use toml::Value;

fn contract() -> Value {
    let text = fs::read_to_string(".cli-flags.toml").expect("read .cli-flags.toml");
    toml::from_str(&text).expect("parse .cli-flags.toml")
}

fn env_at<'a>(root: &'a Value, path: &[&str]) -> &'a str {
    let mut value = root;
    for key in path {
        value = value
            .get(*key)
            .unwrap_or_else(|| panic!("missing {}", path.join(".")));
    }
    value
        .get("env")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing env at {}", path.join(".")))
}

fn assert_no_long_key(value: &Value, path: &str) {
    match value {
        Value::Table(table) => {
            assert!(
                !table.contains_key("long"),
                "{path} uses unsupported flags2env key `long`"
            );
            for (key, child) in table {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                assert_no_long_key(child, &child_path);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_long_key(child, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn canonical_contract_audits_and_uses_supported_flags2env_schema() {
    BundledFlags2Env::new()
        .audit_config(Some(".cli-flags.toml"))
        .expect("flags2env audit must accept the canonical contract");

    let contract = contract();
    assert_eq!(
        contract["parse"]["allow_unknown"].as_bool(),
        Some(false),
        "canonical contract must remain fail-closed"
    );
    assert_no_long_key(&contract, "");
}

#[test]
fn mirror_and_key_flags_match_clap_command_scopes() {
    let contract = contract();
    let root_flags = contract["flags"].as_table().expect("root flags table");

    for legacy_root in [
        "mirror_json",
        "mirror_package",
        "mirror_bootstrap_url",
        "mirror_output",
        "key_id",
    ] {
        assert!(
            !root_flags.contains_key(legacy_root),
            "{legacy_root} must not be accepted as a root-level flag"
        );
    }

    for (path, expected_env) in [
        (
            ["commands", "mirror", "commands", "list", "flags", "json"].as_slice(),
            "ZED_PKG_MIRROR_JSON",
        ),
        (
            [
                "commands", "mirror", "commands", "check", "flags", "package",
            ]
            .as_slice(),
            "ZED_PKG_MIRROR_PACKAGE",
        ),
        (
            ["commands", "mirror", "commands", "check", "flags", "json"].as_slice(),
            "ZED_PKG_MIRROR_JSON",
        ),
        (
            [
                "commands",
                "mirror",
                "commands",
                "bootstrap",
                "flags",
                "url",
            ]
            .as_slice(),
            "ZED_PKG_MIRROR_BOOTSTRAP_URL",
        ),
        (
            [
                "commands", "mirror", "commands", "sync", "flags", "output",
            ]
            .as_slice(),
            "ZED_PKG_MIRROR_OUTPUT",
        ),
        (
            [
                "commands", "key", "commands", "generate", "flags", "key-id",
            ]
            .as_slice(),
            "ZED_PKG_KEY_ID",
        ),
        (
            ["commands", "key", "commands", "show", "flags", "key-id"].as_slice(),
            "ZED_PKG_KEY_ID",
        ),
        (
            [
                "commands", "key", "commands", "enroll", "flags", "key-id",
            ]
            .as_slice(),
            "ZED_PKG_KEY_ID",
        ),
    ] {
        assert_eq!(env_at(&contract, path), expected_env, "{}", path.join("."));
    }

    assert!(
        contract["commands"]["key"]["commands"]["list"]
            .get("flags")
            .is_none(),
        "key list must not accept --key-id"
    );
}
