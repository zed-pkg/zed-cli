use zed_interfaces::registry::{PackageMetadata, VersionMetadata};
use zed_interfaces::vcs::Vcs;

use super::*;
use crate::pack::pack;

fn test_config(registry: &Path, home: &Path) -> Config {
    Config {
        registry: format!("file://{}", registry.display()),
        home: home.to_path_buf(),
        token: None,
        auth_url: "http://127.0.0.1/unused".to_owned(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
        mirrors: Vec::new(),
        fallback: crate::mirrored_registry::FallbackPolicy::Disabled,
    }
}

fn manifest_text(org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) -> String {
    let mut text = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"

[package.repository]
vcs = "git"
url = "https://example.invalid/{org}/{name}"
"#,
    );
    if !dependencies.is_empty() {
        text.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            text.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    text
}

fn fixture_suffix(version: &str) -> String {
    version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn publish_version(
    registry_root: &Path,
    scratch: &Path,
    org: &str,
    name: &str,
    version: &str,
    dependencies: &[(&str, &str)],
) -> String {
    let suffix = fixture_suffix(version);
    let source = scratch.join(format!("source-{name}-{suffix}"));
    fs::create_dir_all(&source).unwrap();
    let manifest_text = manifest_text(org, name, version, dependencies);
    fs::write(source.join(MANIFEST_FILE), &manifest_text).unwrap();
    fs::write(
        source.join("payload.txt"),
        format!("{org}/{name}@{version}\n"),
    )
    .unwrap();
    let manifest = Manifest::parse(&manifest_text).unwrap();
    let packed = pack(
        &source,
        &manifest,
        Some(&scratch.join(format!("packed-{name}-{suffix}"))),
    )
    .unwrap();

    let artifacts = registry_root.join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    fs::copy(
        &packed.path,
        artifacts.join(format!("{}.tar.gz", packed.sha256)),
    )
    .unwrap();

    let package_dir = registry_root.join("packages").join(org).join(name);
    fs::create_dir_all(package_dir.join("versions")).unwrap();
    let version_metadata = VersionMetadata {
        org: org.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
        vcs_tag: format!("v{version}"),
        vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        download_url: format!("file://{}/{}.tar.gz", artifacts.display(), packed.sha256),
        published_at: "1970-01-01T00:00:00Z".to_owned(),
        yanked: false,
        mirrors: Vec::new(),
        signatures: Vec::new(),
    };
    fs::write(
        package_dir.join("versions").join(format!("{version}.json")),
        serde_json::to_string_pretty(&version_metadata).unwrap(),
    )
    .unwrap();

    let package_path = package_dir.join("package.json");
    let mut package_metadata = fs::read_to_string(&package_path)
        .ok()
        .and_then(|text| serde_json::from_str::<PackageMetadata>(&text).ok())
        .unwrap_or_else(|| PackageMetadata {
            org: org.to_owned(),
            name: name.to_owned(),
            description: Some(format!("fixture {name}")),
            vcs: Vcs::Git,
            repo_url: format!("https://example.invalid/{org}/{name}"),
            version_scheme: manifest.package.version_scheme,
            latest: None,
            tags: Vec::new(),
            versions: Vec::new(),
            mirrors: Vec::new(),
            signing_keys: Vec::new(),
        });
    if !package_metadata.versions.iter().any(|item| item == version) {
        package_metadata.versions.push(version.to_owned());
    }
    version::sort_desc(&mut package_metadata.versions);
    package_metadata.latest = package_metadata.versions.first().cloned();
    fs::write(
        package_path,
        serde_json::to_string_pretty(&package_metadata).unwrap(),
    )
    .unwrap();
    packed.sha256
}

fn write_consumer(project: &Path, dependencies: &[(&str, &str)]) {
    fs::create_dir_all(project).unwrap();
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("consumer", "app", "0.1.0", dependencies),
    )
    .unwrap();
}

#[test]
fn compatible_cycle_reuses_the_warm_store_without_redownloading() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    publish_version(
        &registry,
        &scratch,
        "test",
        "a",
        "1.0.0",
        &[("test/b", "^1")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "b",
        "1.0.0",
        &[("test/a", "^1")],
    );
    write_consumer(&project, &[("test/a", "^1")]);
    let config = test_config(&registry, &home);

    let cold = prefetch(&project, &config, false).unwrap();
    let warm = prefetch(&project, &config, false).unwrap();

    assert_eq!(cold.resolved, 2);
    assert_eq!(cold.downloaded, 2);
    assert_eq!(warm.resolved, 2);
    assert_eq!(warm.downloaded, 0);
}

#[test]
fn self_cycle_with_a_tail_stays_finite() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    publish_version(&registry, &scratch, "test", "leaf", "1.0.0", &[]);
    publish_version(
        &registry,
        &scratch,
        "test",
        "a",
        "1.0.0",
        &[("test/a", "=1.0.0"), ("test/leaf", "^1")],
    );
    write_consumer(&project, &[("test/a", "=1.0.0")]);

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, 2);
    assert_eq!(report.downloaded, 2);
}

#[test]
fn sixty_four_node_cycle_resolves_each_coordinate_once() {
    const NODES: usize = 64;

    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    for index in 0..NODES {
        let name = format!("node-{index:02}");
        let next = format!("test/node-{:02}", (index + 1) % NODES);
        publish_version(
            &registry,
            &scratch,
            "test",
            &name,
            "1.0.0",
            &[(next.as_str(), "=1.0.0")],
        );
    }
    write_consumer(&project, &[("test/node-00", "=1.0.0")]);

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, NODES);
    assert_eq!(report.downloaded, NODES);
}

#[test]
fn multiple_roots_into_one_cycle_do_not_duplicate_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    publish_version(
        &registry,
        &scratch,
        "test",
        "a",
        "1.0.0",
        &[("test/b", "^1")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "b",
        "1.0.0",
        &[("test/a", "^1")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "c",
        "1.0.0",
        &[("test/b", "^1")],
    );
    write_consumer(&project, &[("test/a", "^1"), ("test/c", "^1")]);

    let report = prefetch(&project, &test_config(&registry, &home), false).unwrap();
    assert_eq!(report.resolved, 3);
    assert_eq!(report.downloaded, 3);
}

#[test]
fn divergent_version_cycle_fails_fast_with_version_qualified_paths() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let scratch = temp.path().join("scratch");
    let home = temp.path().join("home");
    let project = temp.path().join("project");

    // A1 -> B1 -> A2 -> B0 -> A2. The exact graph contract can represent
    // this, but lockfile/materialization v1 still selects one version per
    // `org/name`. This regression ensures that boundary is deterministic and
    // bounded rather than recursive or silently collapsed.
    publish_version(
        &registry,
        &scratch,
        "test",
        "b",
        "0.1.0",
        &[("test/a", "=2.0.0")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "a",
        "2.0.0",
        &[("test/b", "=0.1.0")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "b",
        "1.0.0",
        &[("test/a", "=2.0.0")],
    );
    publish_version(
        &registry,
        &scratch,
        "test",
        "a",
        "1.0.0",
        &[("test/b", "=1.0.0")],
    );
    write_consumer(&project, &[("test/a", "=1.0.0")]);

    let error = prefetch(&project, &test_config(&registry, &home), false).unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("version conflict for test/a"), "{message}");
    assert!(
        message.contains("`=1.0.0` via consumer/app@0.1.0 -> test/a"),
        "{message}"
    );
    assert!(
        message.contains(
            "`=2.0.0` via consumer/app@0.1.0 -> test/a@1.0.0 -> test/b@1.0.0 -> test/a"
        ),
        "{message}"
    );
    assert!(
        message.lines().count() < 80,
        "cycle conflict diagnostic grew unexpectedly: {message}"
    );
    assert!(!project.join(LOCKFILE_FILE).exists());
    assert!(!project.join("zed_modules").exists());
}
