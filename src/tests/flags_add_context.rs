#[test]
fn add_project_and_target_are_declared_in_actual_flags2env_contract() {
    // Verify the embedded flags-2-env parser, not just Clap's derived
    // public CLI. Split/inline spellings must agree on the same env keys.
    for words in [
        vec![
            "zed",
            "add",
            "acme/http@^1",
            "--project",
            "apps/api",
            "--target",
            "attolang",
        ],
        vec![
            "zed",
            "add",
            "acme/http@^1",
            "--project=apps/api",
            "--target=attolang",
        ],
        vec![
            "zed",
            "add",
            "--target=attolang",
            "acme/http@^1",
            "--project=apps/api",
        ],
    ] {
        let argv = words.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
        let parsed = parse_embedded(&argv).expect("embedded flags2env add selectors");
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert!(
            parsed.unknown_options.is_empty(),
            "{:?}",
            parsed.unknown_options
        );
        assert_eq!(
            parsed.flags.get("ZED_PKG_ADD_PROJECT").map(String::as_str),
            Some("apps/api"),
            "{words:?}"
        );
        assert_eq!(
            parsed.flags.get("ZED_PKG_TARGET").map(String::as_str),
            Some("attolang"),
            "{words:?}"
        );
        assert_eq!(
            explicit_env_keys(&argv, &parsed.subcommands).unwrap(),
            BTreeSet::from([
                "ZED_PKG_ADD_PROJECT".to_string(),
                "ZED_PKG_TARGET".to_string(),
            ])
        );
    }
}

#[test]
fn add_options_are_scoped_and_unknown_selectors_fail_closed() {
    let document: toml::Value = toml::from_str(CONTRACT).unwrap();
    let add = find_command_scope(document.as_table().unwrap(), "add").unwrap();
    let flags = add.get("flags").and_then(toml::Value::as_table).unwrap();
    assert_eq!(
        flags["project"]["env"].as_str(),
        Some("ZED_PKG_ADD_PROJECT")
    );
    assert_eq!(flags["target"]["env"].as_str(), Some("ZED_PKG_TARGET"));
    let argv = ["zed", "add", "acme/http@^1", "--projcet=apps/api"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let result = parse_embedded(&argv).unwrap();
    assert!(
        result
            .unknown_options
            .iter()
            .any(|name| name.starts_with("--projcet")),
        "flags2env accepted an unknown project selector: {:?}",
        result.unknown_options
    );
}
