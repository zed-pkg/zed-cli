use std::fs;

const QUIET_BY_DEFAULT: [&str; 5] = [
    "dead_code",
    "unused_imports",
    "unused_variables",
    "unused_mut",
    "unused_assignments",
];

#[test]
fn ordinary_builds_only_quiet_the_retained_unused_categories() {
    let manifest = fs::read_to_string("Cargo.toml").expect("read Cargo.toml");
    let manifest: toml::Value = toml::from_str(&manifest).expect("parse Cargo.toml");
    let rust_lints = manifest
        .get("lints")
        .and_then(|lints| lints.get("rust"))
        .and_then(toml::Value::as_table)
        .expect("[lints.rust] table");

    for lint in QUIET_BY_DEFAULT {
        assert_eq!(
            rust_lints.get(lint).and_then(toml::Value::as_str),
            Some("allow"),
            "{lint} must be quiet by default"
        );
    }

    assert_eq!(
        rust_lints
            .get("unused_must_use")
            .and_then(toml::Value::as_str),
        Some("deny"),
        "unused_must_use is correctness-sensitive and must remain denied"
    );
    assert_ne!(
        rust_lints.get("unsafe_code").and_then(toml::Value::as_str),
        Some("allow"),
        "unsafe_code must remain visible"
    );
    assert_ne!(
        rust_lints
            .get("unreachable_pub")
            .and_then(toml::Value::as_str),
        Some("allow"),
        "unreachable public API must remain visible"
    );
    assert!(
        !rust_lints.contains_key("unused"),
        "never suppress Rust's blanket unused lint group"
    );
}

#[test]
fn strict_unused_alias_restores_every_quiet_category_as_an_error() {
    let config = fs::read_to_string(".cargo/config.toml").expect("read .cargo/config.toml");
    let config: toml::Value = toml::from_str(&config).expect("parse .cargo/config.toml");
    let alias = config
        .get("alias")
        .and_then(|aliases| aliases.get("strict-unused"))
        .and_then(toml::Value::as_str)
        .expect("strict-unused alias");

    assert!(alias.contains("clippy --all-targets --locked --"));
    for lint in QUIET_BY_DEFAULT {
        let cli_lint = lint.replace('_', "-");
        assert!(
            alias.contains(&format!("-D {cli_lint}")),
            "strict-unused must deny {lint}"
        );
    }
    assert!(
        !alias.contains("-A unused"),
        "strict audit must not weaken any unused lint group"
    );
}
