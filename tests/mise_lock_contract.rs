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

    let parsed = MiseLockDocument::parse(
        &source,
        "mise.lock",
        MiseLockValidationMode::FrozenPortable,
    )
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
    let parsed = MiseLockDocument::parse(
        &source,
        "mise.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    let normalized = parsed.normalized();
    assert!(matches!(
        normalized.tools["python"][0].platforms["linux-x64"],
        MisePlatformInfo::Detail(_)
    ));
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
    let error = MiseLockDocument::parse(
        source,
        "mise.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap_err();
    assert!(error.to_string().contains("conda-packages.linux-x64.ncurses.checksum"));
}
