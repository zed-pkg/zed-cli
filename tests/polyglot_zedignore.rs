use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use zed_cli::pack::pack_all;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::MANIFEST_FILE;

fn archive_entries(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = fs::File::open(path).unwrap();
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let mut entries = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        entries.insert(path, bytes);
    }
    entries
}

fn manifest_text(target_dir: &str, publish: &str) -> String {
    format!(
        r#"[package]
org = "acme"
name = "polyglot-ignore"
version = "1.0.0"

[package.repository]
url = "https://example.invalid/acme/polyglot-ignore"

{publish}

[targets.nodejs]
dir = "{target_dir}"
adapter = "node"
"#
    )
}

#[test]
fn target_local_zedignore_controls_staging_and_final_archive() {
    let project = tempfile::tempdir().unwrap();
    let target = project.path().join("clients/ts");
    fs::create_dir_all(target.join("target")).unwrap();
    fs::write(target.join("keep.txt"), "keep\n").unwrap();
    fs::write(target.join("secret.local"), "secret\n").unwrap();
    fs::write(target.join("target/release.txt"), "checked in\n").unwrap();
    fs::write(
        target.join(".zedignore"),
        "secret.local\n!target\n!.zedignore\n",
    )
    .unwrap();
    fs::write(project.path().join(".zedignore"), "keep.txt\n").unwrap();

    let text = manifest_text("clients/ts", "[publish]\nexclude = [\"target/**\"]");
    fs::write(project.path().join(MANIFEST_FILE), &text).unwrap();
    let manifest = Manifest::parse(&text).unwrap();

    let packages = pack_all(project.path(), &manifest, None).unwrap();
    let entries = archive_entries(&packages[0].packed.path);
    assert!(entries.contains_key("pkg/keep.txt"));
    assert!(entries.contains_key("pkg/target/release.txt"));
    assert!(!entries.contains_key("pkg/secret.local"));
    assert!(!entries.contains_key("pkg/.zedignore"));

    let emitted = String::from_utf8(entries["pkg/.zpkg.toml"].clone()).unwrap();
    assert!(!emitted.contains("secret.local"));
    assert!(!emitted.contains("!target"));
    assert!(!emitted.contains(".zedignore"));
}

#[test]
fn root_target_uses_repository_root_zedignore() {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("keep.txt"), "keep\n").unwrap();
    fs::write(project.path().join("private.txt"), "private\n").unwrap();
    fs::write(project.path().join(".zedignore"), "private.txt\n").unwrap();

    let text = manifest_text(".", "");
    fs::write(project.path().join(MANIFEST_FILE), &text).unwrap();
    let manifest = Manifest::parse(&text).unwrap();

    let packages = pack_all(project.path(), &manifest, None).unwrap();
    let entries = archive_entries(&packages[0].packed.path);
    assert!(entries.contains_key("pkg/keep.txt"));
    assert!(!entries.contains_key("pkg/private.txt"));
    assert!(!entries.contains_key("pkg/.zedignore"));
}

#[test]
fn invalid_target_local_glob_fails_with_source_context() {
    let project = tempfile::tempdir().unwrap();
    let target = project.path().join("clients/ts");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("keep.txt"), "keep\n").unwrap();
    fs::write(target.join(".zedignore"), "[\n").unwrap();

    let text = manifest_text("clients/ts", "");
    fs::write(project.path().join(MANIFEST_FILE), &text).unwrap();
    let manifest = Manifest::parse(&text).unwrap();

    let error = pack_all(project.path(), &manifest, None)
        .err()
        .expect("invalid target-local glob must fail packing");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("invalid glob pattern `[`"), "{rendered}");
}
