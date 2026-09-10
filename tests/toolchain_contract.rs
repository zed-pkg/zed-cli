use std::fs;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

#[test]
fn release_and_portability_use_the_repository_toolchain_pin() {
    let toolchain: toml::Value = toml::from_str(&read("rust-toolchain.toml"))
        .expect("rust-toolchain.toml must be valid TOML");
    let channel = toolchain["toolchain"]["channel"]
        .as_str()
        .expect("rust-toolchain.toml toolchain.channel must be a string");

    assert!(
        !matches!(channel, "stable" | "beta" | "nightly"),
        "release builds require an exact Rust version, not moving channel `{channel}`"
    );
    let components = channel.split('.').collect::<Vec<_>>();
    assert_eq!(
        components.len(),
        3,
        "Rust toolchain pin must use an exact major.minor.patch version"
    );
    assert!(
        components
            .iter()
            .all(|component| component.parse::<u64>().is_ok()),
        "Rust toolchain pin must contain numeric major.minor.patch components"
    );

    let portability = read(".github/workflows/cli-portability.yml");
    assert!(
        portability.contains(&format!("rust-toolchain: '{channel}'")),
        "CLI portability workflow must use rust-toolchain.toml channel {channel}"
    );

    let release = read(".github/workflows/release.yml");
    assert!(
        release.contains(&format!("toolchain: '{channel}'")),
        "release workflow must use rust-toolchain.toml channel {channel}"
    );
}

#[test]
fn matrix_workflows_derive_components_portably_from_the_toolchain_manifest() {
    for path in [
        ".github/workflows/asdf-interop.yml",
        ".github/workflows/task-runtime.yml",
    ] {
        let workflow = read(path);
        assert!(
            workflow.contains("with open(\"rust-toolchain.toml\", \"rb\") as source:"),
            "{path} must derive Rust configuration from rust-toolchain.toml"
        );
        assert!(
            workflow.contains("component=\"${component%$'\\r'}\""),
            "{path} must strip Windows CRLF from Python-emitted component names before rustup"
        );
        assert!(
            !workflow.contains("rustup toolchain install stable"),
            "{path} must not bypass the repository toolchain pin with moving stable"
        );
    }
}
