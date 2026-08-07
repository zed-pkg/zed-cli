use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn checksum(digit: char) -> String {
    digit.to_string().repeat(64)
}

fn revision(digit: char) -> String {
    digit.to_string().repeat(40)
}

fn write_fixture(root: &Path) {
    fs::write(
        root.join(".tool-versions"),
        "nodejs 22.11.0\npython 3.12.4\n",
    )
    .unwrap();
    fs::create_dir_all(root.join(".zed")).unwrap();
    fs::write(
        root.join(".zed/asdf.lock.toml"),
        format!(
            r#"schema = 1

[plugins.nodejs]
version = "22.11.0"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
revision = "{}"
sha256 = "{}"
platforms = ["x86_64-linux"]

[plugins.python]
version = "3.12.4"
url = "https://github.com/danhper/asdf-python.git"
revision = "{}"
sha256 = "{}"
"#,
            revision('1'),
            checksum('a'),
            revision('2'),
            checksum('b'),
        ),
    )
    .unwrap();
}

#[test]
fn canonical_env_dispatch_imports_and_verifies_asdf_read_only() {
    let temp = tempfile::tempdir().unwrap();
    write_fixture(temp.path());
    let config_before = fs::read(temp.path().join(".tool-versions")).unwrap();
    let lock_before = fs::read(temp.path().join(".zed/asdf.lock.toml")).unwrap();

    for action in ["import", "verify"] {
        let output = Command::new(binary())
            .current_dir(temp.path())
            .env("PATH", "")
            .args(["env", action, "asdf", "--frozen", "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{action} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if action == "verify" {
            assert_eq!(value["manager"], "asdf");
            assert_eq!(value["verified"], true);
        } else {
            assert_eq!(value["tools"].as_object().unwrap().len(), 2);
        }
    }

    assert_eq!(
        fs::read(temp.path().join(".tool-versions")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read(temp.path().join(".zed/asdf.lock.toml")).unwrap(),
        lock_before
    );
}

#[test]
fn canonical_env_dispatch_fails_closed_without_frozen_provenance() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".tool-versions"), "nodejs 22.11.0\n").unwrap();
    let output = Command::new(binary())
        .current_dir(temp.path())
        .env("PATH", "")
        .args(["env", "verify", "asdf", "--frozen"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("frozen asdf import requires `.zed/asdf.lock.toml`")
    );
}
