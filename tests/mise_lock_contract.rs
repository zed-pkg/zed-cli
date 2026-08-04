use zed_cli::mise_lock::{MiseLockDocument, MiseLockValidationMode, MisePlatformInfo};

const SHA256_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA256_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn external_consumer_can_round_trip_current_option_dependent_lock_state() {
    let source = format!(
        r#"
[pkgx-packages.linux-x64."zlib.net@1.3.1"]
url = "https://repo.example.test/zlib.tar.gz"
checksum = "sha256:{SHA256_A}"
pkgx_provides = ["zlib-flate"]

[[tools.node]]
version = "22.4.0"
backend = "core:node"
options = {{ flavor = "full" }}
[tools.node.platforms.linux-x64]
checksum = "sha256:{SHA256_A}"
url = "https://downloads.example.test/node.tar.gz"
pkgx_deps = ["zlib.net@1.3.1"]
additional_artifacts = [
  {{ url = "https://downloads.example.test/npm.tar.gz", checksum = "sha256:{SHA256_B}" }}
]

[[tools.node]]
version = "22.4.0"
backend = "core:node"
options = {{ flavor = "minimal" }}
[tools.node.platforms.linux-x64]
checksum = "sha256:{SHA256_B}"
url = "https://downloads.example.test/node-minimal.tar.gz"
"#
    );

    let parsed =
        MiseLockDocument::parse(&source, "mise.lock", MiseLockValidationMode::FrozenPortable)
            .unwrap();
    assert_eq!(parsed.tools["node"].len(), 2);

    let normalized_toml = parsed.to_toml_string().unwrap();
    let reparsed = MiseLockDocument::parse(
        &normalized_toml,
        "normalized.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    assert_eq!(parsed.normalized(), reparsed.normalized());
    assert_eq!(
        parsed.semantic_digest_sha256().unwrap(),
        reparsed.semantic_digest_sha256().unwrap()
    );
}

#[test]
fn public_normalization_expands_compact_platform_checksums() {
    let source = format!(
        "[[tools.python]]\nversion = \"3.12.4\"\n[tools.python.platforms]\nlinux-x64 = \"sha256:{SHA256_A}\"\n"
    );
    let parsed =
        MiseLockDocument::parse(&source, "mise.lock", MiseLockValidationMode::FrozenPortable)
            .unwrap();
    let normalized = parsed.normalized();
    assert!(
        normalized.tools["python"][0]
            .platforms
            .get("linux-x64")
            .is_some_and(|info| matches!(info, MisePlatformInfo::Detail(_)))
    );
}

#[test]
fn public_frozen_boundary_rejects_unverified_shared_packages() {
    let source = r#"
[conda-packages.linux-x64."ncurses"]
url = "https://repo.example.test/ncurses.tar.bz2"

[[tools.python]]
version = "3.12.4"
[tools.python.platforms.linux-x64]
checksum = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
conda_deps = ["ncurses"]
"#;
    let error =
        MiseLockDocument::parse(source, "mise.lock", MiseLockValidationMode::FrozenPortable)
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("conda-packages.linux-x64.ncurses.checksum")
    );
}

#[test]
fn pinned_current_mise_wire_fixture_round_trips_without_platform_key_loss() {
    let source = include_str!("fixtures/mise-lock/current-actionlint.lock");
    let parsed = MiseLockDocument::parse(
        source,
        "current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    let actionlint = &parsed.tools["actionlint"][0];
    assert_eq!(actionlint.platforms.len(), 2);
    assert!(actionlint.platforms.contains_key("linux-x64"));
    assert!(actionlint.platforms.contains_key("macos-arm64"));

    let rendered = parsed.to_toml_string().unwrap();
    assert!(rendered.contains("[tools.actionlint.\"platforms.linux-x64\"]"));
    assert!(rendered.contains("[tools.actionlint.\"platforms.macos-arm64\"]"));
    assert!(!rendered.contains("[tools.actionlint.platforms.linux-x64]"));

    let reparsed = MiseLockDocument::parse(
        &rendered,
        "rendered-current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    assert_eq!(parsed.normalized(), reparsed.normalized());
    assert_eq!(
        parsed.semantic_digest_sha256().unwrap(),
        reparsed.semantic_digest_sha256().unwrap()
    );
}

#[test]
fn provenance_mutation_in_current_wire_changes_semantic_identity() {
    let source = include_str!("fixtures/mise-lock/current-actionlint.lock");
    let github = MiseLockDocument::parse(
        source,
        "current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    let cosign_source = source.replacen(
        "provenance = \"github-attestations\"",
        "provenance = \"cosign\"",
        1,
    );
    let cosign = MiseLockDocument::parse(
        &cosign_source,
        "current-actionlint-cosign.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    assert_ne!(
        github.semantic_digest_sha256().unwrap(),
        cosign.semantic_digest_sha256().unwrap()
    );
}

#[test]
fn mixed_nested_and_current_platform_encodings_fail_closed() {
    let source = format!(
        r#"
[[tools.node]]
version = "22.4.0"
[tools.node.platforms.linux-x64]
checksum = "sha256:{SHA256_A}"
[tools.node."platforms.macos-arm64"]
checksum = "sha256:{SHA256_B}"
"#
    );
    let error = MiseLockDocument::parse(
        &source,
        "mixed.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap_err();
    assert!(error.to_string().contains("mixes nested `platforms`"));
}
