//! End-to-end tests for the whole publish -> install loop, hermetic (a
//! file:// registry and a temp ZED_PKG_HOME; zero network). Includes
//! language-convention pruning checks (node/python/go/java trees) and the
//! container-safety guarantee for copy-mode installs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use semver::VersionReq;
use zed_cli::cli::{Adapter, InstallMode};
use zed_cli::config::Config;
use zed_cli::ops;
use zed_cli::pack;
use zed_cli::registry::{FileRegistry, Registry, resolve_version};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

fn manifest_toml(
    org: &str,
    name: &str,
    version: &str,
    deps: &BTreeMap<String, String>,
    smoke_test: Option<&str>,
) -> String {
    let mut out = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"
description = "fixture package"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/{org}/{name}"
"#
    );
    if !deps.is_empty() {
        out.push_str("\n[dependencies]\n");
        for (key, req) in deps {
            out.push_str(&format!("\"{key}\" = \"{req}\"\n"));
        }
    }
    if let Some(smoke) = smoke_test {
        out.push_str(&format!("\n[publish]\nsmoke_test = '{smoke}'\n"));
    }
    out
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

fn fixture_package(
    root: &Path,
    org: &str,
    name: &str,
    version: &str,
    deps: &BTreeMap<String, String>,
    smoke_test: Option<&str>,
    files: &[(&str, &str)],
) -> PathBuf {
    let dir = root.join(format!("{org}-{name}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(MANIFEST_FILE),
        manifest_toml(org, name, version, deps, smoke_test),
    )
    .unwrap();
    write_files(&dir, files);
    dir
}

fn archive_entries(archive: &Path) -> Vec<String> {
    let file = fs::File::open(archive).unwrap();
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    tar.entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
        .collect()
}

fn publish_to(registry: &FileRegistry, project: &Path) -> String {
    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let packed = pack::pack(project, &manifest, None).unwrap();
    let meta = ops::build_publish_meta(&manifest, &packed, Some("deadbeef".into()));
    registry.publish(&meta, &packed.path, None).unwrap();
    packed.sha256
}

fn test_config(tmp: &Path, registry_dir: &Path) -> Config {
    Config {
        registry: format!("file://{}", registry_dir.display()),
        home: tmp.join("zed-home"),
        token: None,
    }
}

const BASE_FILES: &[(&str, &str)] = &[
    ("src/lib.txt", "base lib\n"),
    ("LICENSE", "MIT\n"),
    ("README.md", "should be stripped\n"),
    ("tests/should_strip.rs", "#[test] fn x() {}\n"),
    (".github/workflows/ci.yml", "on: push\n"),
];

#[test]
fn pack_is_pruned_and_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    let project = fixture_package(
        tmp.path(),
        "acme",
        "demo",
        "0.1.0",
        &BTreeMap::new(),
        None,
        BASE_FILES,
    );

    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let first = pack::pack(&project, &manifest, None).unwrap();
    let second = pack::pack(&project, &manifest, None).unwrap();
    assert_eq!(first.sha256, second.sha256, "pack must be deterministic");

    let entries = archive_entries(&first.path);
    assert!(entries.contains(&format!("pkg/{MANIFEST_FILE}")));
    assert!(entries.contains(&"pkg/LICENSE".to_string()));
    assert!(entries.contains(&"pkg/src/lib.txt".to_string()));
    assert!(
        !entries.iter().any(|e| e.contains("tests/")
            || e.contains(".github")
            || e.to_lowercase().contains("readme")),
        "pruned entries leaked: {entries:?}"
    );
    assert!(first.excluded_count >= 3);
}

#[test]
fn language_conventions_are_stripped() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: &[(&str, &[(&str, &str)], &[&str], &[&str])] = &[
        (
            "node-pkg",
            &[
                ("package.json", "{\"name\":\"x\"}\n"),
                ("index.js", "module.exports = 1\n"),
                ("__tests__/a.test.js", "test\n"),
                ("lib/util.spec.js", "spec\n"),
            ],
            &["pkg/package.json", "pkg/index.js"],
            &["a.test.js", "util.spec.js"],
        ),
        (
            "python-pkg",
            &[
                ("pyproject.toml", "[project]\nname = \"x\"\n"),
                ("x/mod.py", "VALUE = 1\n"),
                ("tests/test_mod.py", "def test(): pass\n"),
                ("x/mod_test.py", "def test(): pass\n"),
            ],
            &["pkg/pyproject.toml", "pkg/x/mod.py"],
            &["test_mod.py", "mod_test.py"],
        ),
        (
            "go-pkg",
            &[
                ("go.mod", "module example.com/x\n"),
                ("main.go", "package main\n"),
                ("main_test.go", "package main\n"),
            ],
            &["pkg/go.mod", "pkg/main.go"],
            &["main_test.go"],
        ),
        (
            "java-pkg",
            &[
                ("pom.xml", "<project/>\n"),
                ("src/main/java/App.java", "class App {}\n"),
                ("src/test/java/AppTest.java", "class AppTest {}\n"),
            ],
            &["pkg/pom.xml", "pkg/src/main/java/App.java"],
            &["AppTest.java"],
        ),
    ];

    for (name, files, must_have, must_not_have) in cases {
        let project = fixture_package(
            tmp.path(),
            "acme",
            name,
            "0.1.0",
            &BTreeMap::new(),
            None,
            files,
        );
        let manifest =
            Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
        let packed = pack::pack(&project, &manifest, None).unwrap();
        let entries = archive_entries(&packed.path);
        for required in *must_have {
            assert!(
                entries.contains(&required.to_string()),
                "{name}: missing {required} in {entries:?}"
            );
        }
        for banned in *must_not_have {
            assert!(
                !entries.iter().any(|e| e.contains(banned)),
                "{name}: {banned} leaked into {entries:?}"
            );
        }
    }
}

#[test]
fn publish_install_roundtrip_with_transitive_deps() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    let base = fixture_package(
        tmp.path(),
        "acme",
        "base",
        "0.1.0",
        &BTreeMap::new(),
        None,
        &[("src/base.txt", "base\n"), ("LICENSE", "MIT\n")],
    );
    publish_to(&registry, &base);

    let mut demo_deps = BTreeMap::new();
    demo_deps.insert("acme/base".to_string(), "^0.1".to_string());
    let demo = fixture_package(
        tmp.path(),
        "acme",
        "demo",
        "0.2.0",
        &demo_deps,
        None,
        BASE_FILES,
    );
    let demo_sha = publish_to(&registry, &demo);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "app",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/demo".to_string(), "^0.2".to_string());
            deps
        },
        None,
        &[],
    );

    let cfg = test_config(tmp.path(), &registry_dir);
    let outcome = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    assert_eq!(outcome.installed.len(), 2, "direct + transitive");

    let demo_link = consumer.join(MODULES_DIR).join("acme").join("demo");
    let base_link = consumer.join(MODULES_DIR).join("acme").join("base");
    assert!(
        fs::symlink_metadata(&demo_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(base_link.join("src/base.txt").exists());
    let link_target = fs::canonicalize(&demo_link).unwrap();
    let store_root = fs::canonicalize(cfg.home.join("store")).unwrap();
    assert!(
        link_target.starts_with(&store_root),
        "symlink must point into the global store"
    );
    assert!(demo_link.join("src/lib.txt").exists());
    assert!(!demo_link.join("README.md").exists());

    let lock = Lockfile::parse(&fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.packages.len(), 2);
    assert_eq!(lock.find("acme", "demo").unwrap().sha256, demo_sha);
    assert_eq!(lock.find("acme", "demo").unwrap().vcs_tag, "v0.2.0");

    // Frozen re-install from the lockfile succeeds after wiping modules.
    fs::remove_dir_all(consumer.join(MODULES_DIR)).unwrap();
    ops::install(
        &consumer,
        &cfg,
        true,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    assert!(demo_link.join("src/lib.txt").exists());
}

#[test]
fn copy_mode_is_container_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    let lib = fixture_package(
        tmp.path(),
        "acme",
        "libpkg",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("dist/bundle.js", "js\n"), ("LICENSE", "MIT\n")],
    );
    publish_to(&registry, &lib);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "containerapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/libpkg".to_string(), "^1".to_string());
            deps
        },
        None,
        &[],
    );

    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Copy,
        Adapter::None,
        false,
    )
    .unwrap();

    let modules = consumer.join(MODULES_DIR);
    let mut checked = 0;
    for entry in walkdir_all(&modules) {
        let meta = fs::symlink_metadata(&entry).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "copy mode must not produce symlinks (found {entry:?}); \
             OCI image layers need self-contained files"
        );
        checked += 1;
    }
    assert!(checked > 0);
    assert!(modules.join("acme/libpkg/dist/bundle.js").exists());
}

#[test]
fn circular_deps_terminate_and_install_both() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    // a -> b -> a: the resolver must terminate and install one copy of each.
    let mut a_deps = BTreeMap::new();
    a_deps.insert("acme/cyc-b".to_string(), "^0.1".to_string());
    let a = fixture_package(
        tmp.path(),
        "acme",
        "cyc-a",
        "0.1.0",
        &a_deps,
        None,
        &[("a.txt", "a\n")],
    );
    let mut b_deps = BTreeMap::new();
    b_deps.insert("acme/cyc-a".to_string(), "^0.1".to_string());
    let b = fixture_package(
        tmp.path(),
        "acme",
        "cyc-b",
        "0.1.0",
        &b_deps,
        None,
        &[("b.txt", "b\n")],
    );
    publish_to(&registry, &a);
    publish_to(&registry, &b);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "cyclic",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/cyc-a".to_string(), "^0.1".to_string());
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    let outcome = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    assert_eq!(
        outcome.installed.len(),
        2,
        "cycle resolved to exactly two packages"
    );
    assert!(consumer.join(MODULES_DIR).join("acme/cyc-a/a.txt").exists());
    assert!(consumer.join(MODULES_DIR).join("acme/cyc-b/b.txt").exists());
}

#[test]
fn node_adapter_links_into_node_modules() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    let lib = fixture_package(
        tmp.path(),
        "acme",
        "nodelib",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[
            ("package.json", "{\"name\":\"@acme/nodelib\"}\n"),
            ("index.js", "module.exports = 1\n"),
        ],
    );
    publish_to(&registry, &lib);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "nodeapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/nodelib".to_string(), "^1".to_string());
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Node,
        false,
    )
    .unwrap();

    let node_link = consumer.join("node_modules").join("@acme").join("nodelib");
    assert!(node_link.join("package.json").exists());
    assert!(node_link.join("index.js").exists());
}

#[test]
fn adapter_auto_is_context_aware_node_and_java() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    let lib = fixture_package(
        tmp.path(),
        "acme",
        "jarlib",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[
            ("lib/jarlib.jar", "not-really-a-jar\n"),
            ("index.js", "x\n"),
        ],
    );
    publish_to(&registry, &lib);
    let deps = {
        let mut deps = BTreeMap::new();
        deps.insert("acme/jarlib".to_string(), "^1".to_string());
        deps
    };
    let cfg = test_config(tmp.path(), &registry_dir);

    // package.json present -> auto selects the node adapter.
    let node_consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "autonode",
        "0.0.1",
        &deps,
        None,
        &[("package.json", "{}\n")],
    );
    ops::install(
        &node_consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
    )
    .unwrap();
    assert!(
        node_consumer
            .join("node_modules/@acme/jarlib/index.js")
            .exists()
    );

    // pom.xml present -> auto selects the java adapter and writes a
    // classpath file pointing at the installed jars.
    let java_consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "autojava",
        "0.0.1",
        &deps,
        None,
        &[("pom.xml", "<project/>\n")],
    );
    ops::install(
        &java_consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
    )
    .unwrap();
    let classpath = fs::read_to_string(java_consumer.join(".zed/classpath")).unwrap();
    assert!(classpath.contains("jarlib.jar"), "classpath: {classpath}");
    assert!(!java_consumer.join("node_modules").exists());
}

fn walkdir_all(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            out.push(path.clone());
            if entry.file_type().unwrap().is_dir() && !entry.file_type().unwrap().is_symlink() {
                stack.push(path);
            }
        }
    }
    out
}

#[test]
fn version_conflicts_fail_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    for version in ["0.1.0", "0.2.0"] {
        let pkg = fixture_package(
            tmp.path(),
            "acme",
            &format!("base{}", version.replace('.', "")),
            version,
            &BTreeMap::new(),
            None,
            &[("f.txt", "x\n")],
        );
        // Re-manifest as the same package name at two versions.
        fs::write(
            pkg.join(MANIFEST_FILE),
            manifest_toml("acme", "base", version, &BTreeMap::new(), None),
        )
        .unwrap();
        publish_to(&registry, &pkg);
    }
    let mut mid_deps = BTreeMap::new();
    mid_deps.insert("acme/base".to_string(), "^0.1".to_string());
    let mid = fixture_package(
        tmp.path(),
        "acme",
        "mid",
        "0.1.0",
        &mid_deps,
        None,
        &[("m.txt", "m\n")],
    );
    publish_to(&registry, &mid);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "conflicted",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/base".to_string(), "=0.2.0".to_string());
            deps.insert("acme/mid".to_string(), "^0.1".to_string());
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    let err = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("version conflict"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn resolver_picks_max_satisfying_stable() {
    let versions: Vec<String> = ["1.0.0", "1.2.3", "1.3.0-alpha.1", "2.0.0"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let req = VersionReq::parse("^1.2").unwrap();
    assert_eq!(
        resolve_version(&req, &versions).unwrap().to_string(),
        "1.2.3"
    );
    assert!(resolve_version(&VersionReq::parse("^3").unwrap(), &versions).is_none());
}

#[test]
fn test_local_runs_smoke_test_like_a_consumer() {
    let tmp = tempfile::tempdir().unwrap();
    let good = fixture_package(
        tmp.path(),
        "acme",
        "smoked",
        "0.1.0",
        &BTreeMap::new(),
        Some(r#"test -f "$ZED_PKG_TEST_TARGET/src/lib.txt""#),
        BASE_FILES,
    );
    let cfg = test_config(tmp.path(), &tmp.path().join("unused-registry"));
    ops::test_local(&good, &cfg).unwrap();

    let bad = fixture_package(
        tmp.path(),
        "acme",
        "smokefail",
        "0.1.0",
        &BTreeMap::new(),
        Some("exit 7"),
        BASE_FILES,
    );
    let err = ops::test_local(&bad, &cfg).unwrap_err();
    assert!(format!("{err:#}").contains("smoke_test failed"));
}

#[test]
fn concurrent_installs_share_the_store_safely() {
    // Issue #6: CLI commands are highly concurrent. Many processes installing
    // the same artifact into one store must not corrupt it. We drive threads
    // (each install takes the process flock) at one shared ZED_PKG_HOME.
    use std::sync::Arc;
    use std::thread;

    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let lib = fixture_package(
        tmp.path(),
        "acme",
        "shared",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/x.txt", "shared\n"), ("LICENSE", "MIT\n")],
    );
    publish_to(&registry, &lib);

    let home = Arc::new(tmp.path().join("shared-home"));
    let registry_dir = Arc::new(registry_dir);
    let root = Arc::new(tmp.path().to_path_buf());

    let mut handles = Vec::new();
    for i in 0..8 {
        let home = Arc::clone(&home);
        let registry_dir = Arc::clone(&registry_dir);
        let root = Arc::clone(&root);
        handles.push(thread::spawn(move || {
            let consumer = fixture_package(
                &root,
                "consumerorg",
                &format!("concur{i}"),
                "0.0.1",
                &{
                    let mut deps = BTreeMap::new();
                    deps.insert("acme/shared".to_string(), "^1".to_string());
                    deps
                },
                None,
                &[],
            );
            let cfg = Config {
                registry: format!("file://{}", registry_dir.display()),
                home: (*home).clone(),
                token: None,
            };
            ops::install(
                &consumer,
                &cfg,
                false,
                InstallMode::Symlink,
                Adapter::None,
                false,
            )
            .unwrap();
            assert!(
                consumer
                    .join(MODULES_DIR)
                    .join("acme/shared/src/x.txt")
                    .exists()
            );
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    // Exactly one extracted copy in the shared store despite 8 installers.
    let store = zed_cli::store::Store::new(&home);
    assert_eq!(store.status().0, 1);
}

#[test]
fn zip_artifacts_pack_deterministically_and_install() {
    // The registry hosts tarballs AND zip files. A zip artifact must pack
    // deterministically, prune the same way, and install through the store's
    // magic-byte extraction just like a tar.gz.
    use zed_cli::pack::pack_format;
    use zed_interfaces::artifact::ArtifactFormat;

    let tmp = tempfile::tempdir().unwrap();
    let project = fixture_package(
        tmp.path(),
        "acme",
        "zipped",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[
            ("dist/bundle.js", "zip me\n"),
            ("LICENSE", "MIT\n"),
            ("tests/t.rs", "strip\n"),
        ],
    );
    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let a = pack_format(&project, &manifest, None, ArtifactFormat::Zip).unwrap();
    let b = pack_format(&project, &manifest, None, ArtifactFormat::Zip).unwrap();
    assert_eq!(a.sha256, b.sha256, "zip pack must be deterministic");
    assert!(a.path.extension().unwrap() == "zip");
    assert_eq!(a.format, ArtifactFormat::Zip);

    // Publish the zip to a file registry and install it into a consumer.
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let meta = ops::build_publish_meta(&manifest, &a, Some("deadbeef".into()));
    assert_eq!(meta.format, ArtifactFormat::Zip);
    registry.publish(&meta, &a.path, None).unwrap();

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "zipconsumer",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/zipped".to_string(), "^1".to_string());
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let dest = consumer.join(MODULES_DIR).join("acme/zipped");
    assert!(
        dest.join("dist/bundle.js").exists(),
        "zip artifact extracted"
    );
    assert!(
        !dest.join("tests").exists(),
        "tests pruned from the zip too"
    );
}

#[test]
fn calendar_versions_resolve() {
    // A calver-scheme package publishes 2026.* tags; a semver range over them
    // resolves to the newest, proving the shared version resolver is wired in.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    for ver in ["2026.06.01", "2026.07.24", "2026.08.01"] {
        let dir = tmp.path().join(format!("cal-{ver}"));
        fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            "[package]\norg = \"acme\"\nname = \"caltool\"\nversion = \"{ver}\"\nversion_scheme = \"calver\"\n\n[package.repository]\nvcs = \"git\"\nurl = \"https://github.com/acme/caltool\"\n"
        );
        fs::write(dir.join(MANIFEST_FILE), &toml).unwrap();
        fs::write(dir.join("f.txt"), "x\n").unwrap();
        publish_to(&registry, &dir);
    }

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "calapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert(
                "acme/caltool".to_string(),
                ">=2026.0.0 <2027.0.0".to_string(),
            );
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let lock = Lockfile::parse(&fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(
        lock.find("acme", "caltool").unwrap().version,
        "2026.08.01",
        "newest calendar version resolved"
    );
}

/// Write a package fixture whose manifest declares a non-default
/// `version_scheme` (issue #3), so calendar/opaque versions publish and
/// install like any other package.
fn scheme_fixture(root: &Path, name: &str, version: &str, scheme: &str) -> PathBuf {
    let dir = root.join(format!("{name}-{}", version.replace(['.', '-'], "_")));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(MANIFEST_FILE),
        format!(
            r#"[package]
org = "acme"
name = "{name}"
version = "{version}"
version_scheme = "{scheme}"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/{name}"
"#
        ),
    )
    .unwrap();
    write_files(&dir, &[("f.txt", "x\n"), ("LICENSE", "MIT\n")]);
    dir
}

#[test]
fn calver_versions_resolve_by_semver_range() {
    // A calendar-versioned package resolves under normal semver ranges: the
    // resolver normalizes 2026.07.24 -> 2026.7.24 to a total order.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    for v in ["2026.07.24", "2026.08.01"] {
        publish_to(
            &registry,
            &scheme_fixture(tmp.path(), "caltool", v, "calver"),
        );
    }

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "calapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert(
                "acme/caltool".to_string(),
                ">=2026.0.0, <2027.0.0".to_string(),
            );
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    let outcome = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    assert_eq!(
        outcome.installed,
        vec![("acme/caltool".to_string(), "2026.08.01".to_string())]
    );
    let lock = Lockfile::parse(&fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find("acme", "caltool").unwrap().version, "2026.08.01");
}

#[test]
fn opaque_versions_require_exact_match() {
    // Opaque-scheme packages have no range algebra; the requirement must match
    // a published tag exactly, and a semver-range request finds nothing.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    for v in ["legacy-api", "release-candidate-1"] {
        publish_to(
            &registry,
            &scheme_fixture(tmp.path(), "opaquetool", v, "opaque"),
        );
    }
    let cfg = test_config(tmp.path(), &registry_dir);

    let exact = fixture_package(
        tmp.path(),
        "consumerorg",
        "opaqueapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/opaquetool".to_string(), "legacy-api".to_string());
            deps
        },
        None,
        &[],
    );
    let outcome = ops::install(
        &exact,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    assert_eq!(
        outcome.installed,
        vec![("acme/opaquetool".to_string(), "legacy-api".to_string())]
    );

    let ranged = fixture_package(
        tmp.path(),
        "consumerorg",
        "opaquerange",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/opaquetool".to_string(), "^1".to_string());
            deps
        },
        None,
        &[],
    );
    let err = ops::install(
        &ranged,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("no version"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn store_prune_removes_unreferenced_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let lib = fixture_package(
        tmp.path(),
        "acme",
        "prunable",
        "0.1.0",
        &BTreeMap::new(),
        None,
        &[("f.txt", "x\n")],
    );
    publish_to(&registry, &lib);

    let consumer_root = tmp.path().join("throwaway");
    fs::create_dir_all(&consumer_root).unwrap();
    let consumer = fixture_package(
        &consumer_root,
        "consumerorg",
        "gone",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/prunable".to_string(), "^0.1".to_string());
            deps
        },
        None,
        &[],
    );
    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();

    let store = zed_cli::store::Store::new(&cfg.home);
    assert_eq!(store.status().0, 1);
    fs::remove_dir_all(&consumer_root).unwrap();
    let (removed, _) = store.prune().unwrap();
    assert_eq!(removed, 1);
    assert_eq!(store.status().0, 0);
}

// ---------------------------------------------------------------------------
// new-feature coverage: bins, workspaces, build hooks, yank, gc, hardening

/// Packages expose executables via [bin]; installs hoist them into
/// zed_modules/.bin and `zed run` executes them with that dir on PATH.
#[test]
fn bins_are_hoisted_and_runnable() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let tool = fixture_package(
        tmp.path(),
        "acme",
        "toolkit",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("scripts/hello.sh", "#!/bin/sh\necho hello-from-toolkit\n")],
    );
    fs::write(
        tool.join(MANIFEST_FILE),
        format!(
            "{}\n[bin]\nhello = \"scripts/hello.sh\"\n",
            fs::read_to_string(tool.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    publish_to(&registry, &tool);

    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/toolkit".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("zed-local", "consumer", "0.0.0", &deps, None),
    )
    .unwrap();
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();

    let hoisted = consumer.join(MODULES_DIR).join(".bin").join("hello");
    assert!(hoisted.exists(), "hoisted bin link missing");
    let code = ops::run_bin(&consumer, "hello", &[]).unwrap();
    assert_eq!(code, 0, "zed run should propagate a zero exit");

    let missing = ops::run_bin(&consumer, "nope", &[]).unwrap_err();
    assert!(missing.to_string().contains("available: hello"));
}

/// [workspace] members resolve straight to their source directories, so
/// edits are live and the registry is never consulted for them.
#[test]
fn workspace_members_link_from_source() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let cfg = test_config(tmp.path(), &registry_dir);

    let root = tmp.path().join("mono");
    fs::create_dir_all(root.join("packages")).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        format!(
            "{}\n[workspace]\nmembers = [\"packages/*\"]\n",
            manifest_toml("acme", "mono-root", "0.0.0", &BTreeMap::new(), None)
        ),
    )
    .unwrap();

    let liba = root.join("packages").join("liba");
    write_files(&liba, &[("src/lib.txt", "v1 of liba\n")]);
    fs::write(
        liba.join(MANIFEST_FILE),
        manifest_toml("acme", "liba", "0.1.0", &BTreeMap::new(), None),
    )
    .unwrap();

    let app = root.join("packages").join("app");
    fs::create_dir_all(&app).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/liba".to_string(), "^0.1".to_string());
    fs::write(
        app.join(MANIFEST_FILE),
        manifest_toml("acme", "app", "0.1.0", &deps, None),
    )
    .unwrap();

    // No registry publish for liba: resolution must come from the workspace.
    ops::install(
        &app,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();

    let link = app.join(MODULES_DIR).join("acme").join("liba");
    let linked_lib = link.join("src/lib.txt");
    assert_eq!(fs::read_to_string(&linked_lib).unwrap(), "v1 of liba\n");

    // Live editing: a change in the member source is visible immediately.
    fs::write(liba.join("src/lib.txt"), "v2 of liba\n").unwrap();
    assert_eq!(fs::read_to_string(&linked_lib).unwrap(), "v2 of liba\n");

    // Workspace links are not pinned in the lockfile (no artifact exists).
    let lock = Lockfile::parse(&fs::read_to_string(app.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert!(lock.find("acme", "liba").is_none());
}

/// [build] steps run in a staging copy, results land in the per-platform
/// build cache, the immutable source store stays pristine, and builds only
/// run when explicitly allowed.
#[test]
fn build_hooks_stage_build_and_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let native = fixture_package(
        tmp.path(),
        "acme",
        "native",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.c", "int add(int a, int b);\n")],
    );
    fs::write(
        native.join(MANIFEST_FILE),
        format!(
            "{}\n[build]\ncommand = \"printf compiled > out.txt\"\noutputs = [\"out.txt\", \"src\"]\n",
            fs::read_to_string(native.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    let sha = publish_to(&registry, &native);

    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/native".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("zed-local", "consumer", "0.0.0", &deps, None),
    )
    .unwrap();

    // Without --allow-build the pristine source is linked (no out.txt).
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let dest = consumer.join(MODULES_DIR).join("acme").join("native");
    assert!(!dest.join("out.txt").exists());

    // With --allow-build the built tree is linked instead.
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        true,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dest.join("out.txt")).unwrap(),
        "compiled"
    );
    assert!(
        dest.join("src/lib.c").exists(),
        "declared output dirs survive"
    );

    // The store's source entry must remain build-free (immutability).
    let store = zed_cli::store::Store::new(&cfg.home);
    assert!(store.pkg_dir(&sha).is_dir());
    assert!(!store.pkg_dir(&sha).join("out.txt").exists());

    // And the build cache entry exists for this platform.
    let platform = zed_interfaces::paths::current_platform();
    let built = cfg
        .home
        .join(zed_interfaces::paths::build_entry_rel(&platform, &sha))
        .join("pkg");
    assert!(built.join("out.txt").is_file());
}

/// A consumer's [overrides.build."org/name"] replaces a broken upstream
/// build command.
#[test]
fn build_overrides_replace_broken_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let broken = fixture_package(
        tmp.path(),
        "acme",
        "broken-build",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "content\n")],
    );
    fs::write(
        broken.join(MANIFEST_FILE),
        format!(
            "{}\n[build]\ncommand = \"exit 1\"\n",
            fs::read_to_string(broken.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    publish_to(&registry, &broken);

    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/broken-build".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        format!(
            "{}\n[overrides.build.\"acme/broken-build\"]\ncommand = \"printf patched > out.txt\"\n",
            manifest_toml("zed-local", "consumer", "0.0.0", &deps, None)
        ),
    )
    .unwrap();

    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        true,
    )
    .unwrap();
    let dest = consumer.join(MODULES_DIR).join("acme").join("broken-build");
    assert_eq!(fs::read_to_string(dest.join("out.txt")).unwrap(), "patched");
}

/// Yanked versions are invisible to range resolution (next-best wins) but
/// exact pins still install, so existing lockfiles keep working.
#[test]
fn yanked_versions_skip_ranges_but_allow_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    for version in ["1.0.0", "1.1.0"] {
        let pkg = fixture_package(
            tmp.path(),
            "acme",
            "yankable",
            version,
            &BTreeMap::new(),
            None,
            &[("src/lib.txt", "content\n")],
        );
        publish_to(&registry, &pkg);
        fs::remove_dir_all(&pkg).unwrap();
    }

    // A lockfile pinned to 1.1.0 exists BEFORE the yank (a consumer that
    // installed it while it was live).
    let pinned = tmp.path().join("pinned");
    fs::create_dir_all(&pinned).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/yankable".to_string(), "^1".to_string());
    fs::write(
        pinned.join(MANIFEST_FILE),
        manifest_toml("zed-local", "pinned", "0.0.0", &deps, None),
    )
    .unwrap();
    ops::install(
        &pinned,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let lock = Lockfile::parse(&fs::read_to_string(pinned.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find("acme", "yankable").unwrap().version, "1.1.0");

    registry
        .yank("acme", "yankable", "1.1.0", true, None)
        .unwrap();

    // Fresh range resolution now skips the yanked 1.1.0 and picks 1.0.0.
    let ranged = tmp.path().join("ranged");
    fs::create_dir_all(&ranged).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/yankable".to_string(), "^1".to_string());
    fs::write(
        ranged.join(MANIFEST_FILE),
        manifest_toml("zed-local", "ranged", "0.0.0", &deps, None),
    )
    .unwrap();
    ops::install(
        &ranged,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let lock = Lockfile::parse(&fs::read_to_string(ranged.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find("acme", "yankable").unwrap().version, "1.0.0");

    // A fresh exact pin on a yanked version fails loudly...
    let fresh_pin = tmp.path().join("fresh_pin");
    fs::create_dir_all(&fresh_pin).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/yankable".to_string(), "=1.1.0".to_string());
    fs::write(
        fresh_pin.join(MANIFEST_FILE),
        manifest_toml("zed-local", "fresh-pin", "0.0.0", &deps, None),
    )
    .unwrap();
    let err = ops::install(
        &fresh_pin,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("yanked"), "unexpected: {err:#}");

    // ...but the pre-existing lockfile keeps working via --frozen.
    ops::install(
        &pinned,
        &cfg,
        true,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let lock = Lockfile::parse(&fs::read_to_string(pinned.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find("acme", "yankable").unwrap().version, "1.1.0");

    // Restoring the version makes it resolvable again.
    registry
        .yank("acme", "yankable", "1.1.0", false, None)
        .unwrap();
    fs::remove_file(ranged.join(LOCKFILE_FILE)).unwrap();
    ops::install(
        &ranged,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap();
    let lock = Lockfile::parse(&fs::read_to_string(ranged.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find("acme", "yankable").unwrap().version, "1.1.0");
}

/// `zed gc` removes unreferenced entries past the age cutoff and leaves
/// referenced ones alone.
#[test]
fn gc_collects_unreferenced_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let pkg = fixture_package(
        tmp.path(),
        "acme",
        "gc-target",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "content\n")],
    );
    publish_to(&registry, &pkg);

    let keeper = tmp.path().join("keeper");
    let goner = tmp.path().join("goner");
    for consumer in [&keeper, &goner] {
        fs::create_dir_all(consumer).unwrap();
        let mut deps = BTreeMap::new();
        deps.insert("acme/gc-target".to_string(), "^1".to_string());
        fs::write(
            consumer.join(MANIFEST_FILE),
            manifest_toml("zed-local", "consumer", "0.0.0", &deps, None),
        )
        .unwrap();
        ops::install(
            consumer,
            &cfg,
            false,
            InstallMode::Symlink,
            Adapter::None,
            false,
        )
        .unwrap();
    }

    let store = zed_cli::store::Store::new(&cfg.home);
    // Both projects alive: nothing to collect even at age 0.
    let (entries, _, _) = store.gc(0).unwrap();
    assert_eq!(entries, 0);
    assert_eq!(store.status().0, 1);

    // Delete both projects: the entry is unreferenced and age 0 collects it.
    fs::remove_dir_all(&keeper).unwrap();
    fs::remove_dir_all(&goner).unwrap();
    let (entries, _, _) = store.gc(0).unwrap();
    assert_eq!(entries, 1);
    assert_eq!(store.status().0, 0);
}

/// A malicious registry cannot traverse out of the store: bad org/name and
/// non-hex sha256 responses are rejected at the trust boundary, and
/// artifacts with escaping paths are refused during extraction.
#[test]
fn malicious_registry_responses_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let pkg = fixture_package(
        tmp.path(),
        "acme",
        "victim",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "content\n")],
    );
    publish_to(&registry, &pkg);

    // Corrupt the version metadata the way a hostile registry would.
    let vjson = registry_dir
        .join("packages")
        .join("acme")
        .join("victim")
        .join("versions")
        .join("1.0.0.json");
    let text = fs::read_to_string(&vjson).unwrap();

    let evil_org = text.replace("\"org\": \"acme\"", "\"org\": \"../../../evil\"");
    fs::write(&vjson, &evil_org).unwrap();
    let consumer = tmp.path().join("consumer");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/victim".to_string(), "=1.0.0".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("zed-local", "consumer", "0.0.0", &deps, None),
    )
    .unwrap();
    let err = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("invalid package identity"),
        "unexpected error: {err:#}"
    );

    // Non-hex sha256 (a path, say) must be refused before any disk use.
    let evil_sha = regex_replace_sha(&text, "../../escape");
    fs::write(&vjson, &evil_sha).unwrap();
    let err = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("invalid sha256"),
        "unexpected error: {err:#}"
    );
}

/// Replace the sha256 value in a version-metadata JSON blob.
fn regex_replace_sha(text: &str, replacement: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("\"sha256\"") {
            out.push_str(&format!("  \"sha256\": \"{replacement}\",\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Artifacts whose entries try to escape the extraction root are refused.
#[test]
fn traversal_artifacts_are_refused() {
    let tmp = tempfile::tempdir().unwrap();

    // Hand-craft a tar.gz with an entry that climbs out of the root. The
    // writer's set_path() rejects `..`, so a real attacker writes the raw
    // header name field directly — which is exactly what we simulate here.
    let evil = tmp.path().join("evil.tar.gz");
    {
        let file = fs::File::create(&evil).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let data = b"pwned\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let name = b"pkg/../../escape.txt";
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let (sha, _) = pack::sha256_file(&evil).unwrap();

    let store = zed_cli::store::Store::new(&tmp.path().join("home"));
    let err = store.add_artifact(&evil, &sha).unwrap_err();
    assert!(
        err.to_string().contains("escapes the extraction root"),
        "unexpected error: {err:#}"
    );
}
