use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-asdf"))
}

fn checksum(digit: char) -> String {
    digit.to_string().repeat(64)
}

fn revision(digit: char) -> String {
    digit.to_string().repeat(40)
}

fn write_fixture(root: &std::path::Path) {
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
fn verify_is_black_box_read_only_and_does_not_need_asdf_on_path() {
    let temp = tempfile::tempdir().unwrap();
    write_fixture(temp.path());
    let before_config = fs::read(temp.path().join(".tool-versions")).unwrap();
    let before_lock = fs::read(temp.path().join(".zed/asdf.lock.toml")).unwrap();

    let output = Command::new(binary())
        .current_dir(temp.path())
        .env("PATH", "")
        .args(["verify", "--frozen", "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["manager"], "asdf");
    assert_eq!(value["verified"], true);
    assert_eq!(value["tools"], 2);
    assert_eq!(
        fs::read(temp.path().join(".tool-versions")).unwrap(),
        before_config
    );
    assert_eq!(
        fs::read(temp.path().join(".zed/asdf.lock.toml")).unwrap(),
        before_lock
    );
}

#[test]
fn frozen_verify_fails_without_provenance_sidecar() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".tool-versions"), "nodejs 22.11.0\n").unwrap();

    let output = Command::new(binary())
        .current_dir(temp.path())
        .env("PATH", "")
        .args(["verify", "--frozen"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("frozen asdf import requires `.zed/asdf.lock.toml`")
    );
}

#[test]
fn adapter_does_not_inherit_parent_tool_versions() {
    let parent = tempfile::tempdir().unwrap();
    let parent_config = parent.path().join(".tool-versions");
    fs::write(&parent_config, "nodejs 22.11.0\n").unwrap();
    let child = parent.path().join("child");
    fs::create_dir_all(&child).unwrap();

    let output = Command::new(binary())
        .current_dir(&child)
        .env("PATH", "")
        .args(["import", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("project-local asdf config does not exist"));
    assert!(!stderr.contains(parent_config.to_string_lossy().as_ref()));
}

#[test]
fn moving_ref_is_rejected_in_frozen_mode() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".tool-versions"), "nodejs ref:main\n").unwrap();
    fs::create_dir_all(temp.path().join(".zed")).unwrap();
    fs::write(
        temp.path().join(".zed/asdf.lock.toml"),
        format!(
            r#"schema = 1

[plugins.nodejs]
version = "ref:main"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
revision = "{}"
sha256 = "{}"
"#,
            revision('1'),
            checksum('a'),
        ),
    )
    .unwrap();

    let output = Command::new(binary())
        .current_dir(temp.path())
        .env("PATH", "")
        .args(["verify", "--frozen"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("moving selector"));
}
