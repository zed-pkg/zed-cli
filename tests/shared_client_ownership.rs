use std::{fs, path::PathBuf};

const ZED_CLIENT_SHA: &str = "6a046b0a5e262c0c6a23851b810d7fe7eda251c4";
const ZED_INTERFACES_SHA: &str = "0c2ffa7be791a44c8aa2a69ab4b1ea87aab4729c";
const RETIRED_INTERFACES_SHA: &str = "0c51d732cb01a377b2bc00e8d945b355e41961c1";

fn repository_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn cargo_sources_are_immutable_and_share_one_interface_authority() {
    let manifest = repository_file("Cargo.toml");
    assert!(manifest.contains(&format!(
        "zed-client = {{ git = \"https://github.com/zed-pkg/zed-clients.git\", rev = \"{ZED_CLIENT_SHA}\" }}"
    )));
    assert!(manifest.contains(&format!(
        "zed-interfaces = {{ git = \"https://github.com/zed-pkg/zed-interfaces.git\", rev = \"{ZED_INTERFACES_SHA}\" }}"
    )));
    assert!(!manifest.contains(RETIRED_INTERFACES_SHA));

    let lock = repository_file("Cargo.lock");
    assert!(lock.contains("name = \"zed-client\""));
    assert!(lock.contains(ZED_CLIENT_SHA));
    assert!(lock.contains(ZED_INTERFACES_SHA));
}

#[test]
fn registry_read_and_metadata_write_operations_use_zed_client() {
    let registry = repository_file("src/registry.rs");

    for required in [
        "shared: zed_client::Client",
        ".get_package(org, name)",
        ".get_version(org, name, version)",
        ".search(query)",
        ".claim_org(slug)",
        ".set_yanked(org, name, version, yanked)",
    ] {
        assert!(
            registry.contains(required),
            "missing shared-client production call: {required}"
        );
    }

    for retired in [
        ".get(self.url(&registry::package_path(org, name)))",
        ".get(self.url(&registry::version_path(org, name, version)))",
        ".get(self.url(&registry::search_path()))",
        ".post(self.url(&registry::orgs_path()))",
        ".post(self.url(&registry::yank_path(org, name, version)))",
    ] {
        assert!(
            !registry.contains(retired),
            "registry operation regressed to CLI-owned HTTP: {retired}"
        );
    }
}

#[test]
fn dependency_matching_uses_zed_lib_instead_of_a_cli_copy() {
    let solver = repository_file("src/install_graph/solver.rs");
    assert!(solver.contains("use zed_lib::requirement_matches;"));
    assert!(!solver.contains("fn requirement_matches("));
}

#[test]
fn copied_schemas_and_validation_fixtures_track_the_manifest_pin() {
    for path in [
        "src/validation.rs",
        "tests/validate_cli.rs",
        "schemas/zed-interfaces/README.md",
    ] {
        let contents = repository_file(path);
        assert!(
            contents.contains(ZED_INTERFACES_SHA),
            "{path} does not name the active interface revision"
        );
        assert!(
            !contents.contains(RETIRED_INTERFACES_SHA),
            "{path} still names the retired interface revision"
        );
    }
}
