//! End-to-end tests for the whole publish -> install loop, hermetic (a
//! file:// registry and a temp ZED_PKG_HOME; zero network). Includes
//! language-convention pruning checks (node/python/go/java trees) and the
//! container-safety guarantee for copy-mode installs.

use std::collections::BTreeMap;
use std::env;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        auth_url: "http://127.0.0.1:8120".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    }
}

const CLEAN_INSTALL_ENV: &[&str] = &[
    "NIX_BUILD_TOP",
    "NIX_STORE",
    "ZED_NATIVE_MANAGER",
    "ZED_NATIVE_PACKAGES",
    "ZED_PKG_ADAPTER",
    "ZED_PKG_ALLOW_BUILD",
    "ZED_PKG_ALLOW_ECOSYSTEM_MISMATCH",
    "ZED_PKG_ALLOW_INSTALL_HOOKS",
    "ZED_PKG_ALLOW_NATIVE_DEPS",
    "ZED_PKG_FROZEN",
    "ZED_PKG_HOME",
    "ZED_PKG_INSTALL_MODE",
    "ZED_PKG_INTERACTIVE",
    "ZED_PKG_NATIVE_DEPS_PROVIDED",
    "ZED_PKG_NATIVE_MANAGER",
    "ZED_PKG_REGISTRY",
    "ZED_PKG_TARGET",
    "ZED_PKG_TOKEN",
];

fn zed_install_command(project: &Path, cfg: &Config) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.current_dir(project);
    for key in CLEAN_INSTALL_ENV {
        command.env_remove(key);
    }
    command
        .arg("--registry")
        .arg(&cfg.registry)
        .arg("--home")
        .arg(&cfg.home)
        .arg("install");
    command
}

#[cfg(unix)]
fn path_with(directory: &Path) -> OsString {
    let mut entries = vec![directory.to_path_buf()];
    entries.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(entries).expect("construct fixture PATH")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_child_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_child_failure(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output_text(output)
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
    type FlagCase<'a> = (
        &'a str,
        &'a [(&'a str, &'a str)],
        &'a [&'a str],
        &'a [&'a str],
    );
    let cases: &[FlagCase<'_>] = &[
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
        None,
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
        None,
        false,
    )
    .unwrap();
    assert!(demo_link.join("src/lib.txt").exists());

    // Uninstall is the lock-preserving inverse. A selected package can be
    // unmaterialized independently, and a full uninstall removes the tree
    // without destroying the exact frozen-reinstall recipe.
    ops::uninstall(&consumer, &cfg, &["acme/demo".to_string()]).unwrap();
    assert!(!demo_link.exists());
    assert!(base_link.join("src/base.txt").exists());
    assert_eq!(
        fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap(),
        lock.to_toml_string().unwrap()
    );

    ops::uninstall(&consumer, &cfg, &[]).unwrap();
    assert!(!consumer.join(MODULES_DIR).exists());
    assert!(consumer.join(LOCKFILE_FILE).is_file());
    ops::install(
        &consumer,
        &cfg,
        true,
        InstallMode::Symlink,
        Adapter::None,
        false,
        None,
        false,
    )
    .unwrap();
    assert!(demo_link.join("src/lib.txt").exists());
    assert!(base_link.join("src/base.txt").exists());
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
        None,
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
        None,
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
        None,
        false,
    )
    .unwrap();

    let node_link = consumer.join("node_modules").join("@acme").join("nodelib");
    assert!(node_link.join("package.json").exists());
    assert!(node_link.join("index.js").exists());

    let lock_before = fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap();
    ops::uninstall(&consumer, &cfg, &[]).unwrap();
    assert!(!node_link.exists());
    assert!(!consumer.join(MODULES_DIR).exists());
    assert_eq!(
        fs::read_to_string(consumer.join(LOCKFILE_FILE)).unwrap(),
        lock_before
    );
    ops::install(
        &consumer,
        &cfg,
        true,
        InstallMode::Symlink,
        Adapter::Node,
        false,
        None,
        false,
    )
    .unwrap();
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
        None,
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
        None,
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
        None,
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
fn r2g_runs_smoke_test_like_a_consumer() {
    use zed_cli::r2g::{self, R2gOptions};

    let tmp = tempfile::tempdir().unwrap();
    // The r2g workspace defaults to <home>/r2g; the test config's home is a
    // temp dir, so the whole roundtrip stays hermetic (no real ~/.zed-pkg).
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
    r2g::run(&good, &cfg, &R2gOptions::default()).unwrap();

    let bad = fixture_package(
        tmp.path(),
        "acme",
        "smokefail",
        "0.1.0",
        &BTreeMap::new(),
        Some("exit 7"),
        BASE_FILES,
    );
    let err = r2g::run(&bad, &cfg, &R2gOptions::default()).unwrap_err();
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
                auth_url: "http://127.0.0.1:8120".to_string(),
                supabase_url: None,
                supabase_key: None,
                interactive: false,
            };
            ops::install(
                &consumer,
                &cfg,
                false,
                InstallMode::Symlink,
                Adapter::None,
                false,
                None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
        false,
    )
    .unwrap();

    let hoisted = consumer.join(MODULES_DIR).join(".bin").join("hello");
    assert!(hoisted.exists(), "hoisted bin link missing");
    let code = ops::run(&consumer, "hello", &[]).unwrap();
    assert_eq!(code, 0, "zed run should propagate a zero exit");

    let missing = ops::run(&consumer, "nope", &[]).unwrap_err();
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
        None,
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

/// Workspace members stay live-linked only when they are inert. A member with
/// package lifecycle hooks is prepared in a disposable copy, and the finalized
/// tree is copied into the consumer without mutating the workspace source.
#[test]
fn workspace_install_hooks_use_a_staging_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let cfg = test_config(tmp.path(), &registry_dir);

    let root = tmp.path().join("workspace-hooks");
    fs::create_dir_all(root.join("packages")).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        format!(
            "{}\n[workspace]\nmembers = [\"packages/*\"]\n",
            manifest_toml("acme", "workspace-hooks", "0.0.0", &BTreeMap::new(), None)
        ),
    )
    .unwrap();

    let library = root.join("packages").join("library");
    fs::create_dir_all(library.join("src")).unwrap();
    fs::write(library.join("src/lib.txt"), "workspace v1\n").unwrap();
    fs::write(
        library.join(MANIFEST_FILE),
        format!(
            r#"{}
[hooks]
pre-install = ['printf pre > generated.txt']
post-install = ['printf post >> generated.txt']
"#,
            manifest_toml("acme", "library", "1.0.0", &BTreeMap::new(), None)
        ),
    )
    .unwrap();

    let app = root.join("packages").join("app");
    fs::create_dir_all(&app).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/library".to_string(), "^1".to_string());
    fs::write(
        app.join(MANIFEST_FILE),
        manifest_toml("acme", "workspace-app", "1.0.0", &deps, None),
    )
    .unwrap();

    let permissions = ops::InstallPermissions {
        allow_install_hooks: true,
        ..ops::InstallPermissions::default()
    };
    ops::install_with_permissions(
        &app,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        &permissions,
        None,
        false,
    )
    .unwrap();

    let installed = app.join(MODULES_DIR).join("acme").join("library");
    assert_eq!(
        fs::read_to_string(installed.join("generated.txt")).unwrap(),
        "prepost"
    );
    assert_eq!(
        fs::read_to_string(installed.join("src/lib.txt")).unwrap(),
        "workspace v1\n"
    );
    assert!(!library.join("generated.txt").exists());

    fs::write(library.join("src/lib.txt"), "workspace v2\n").unwrap();
    assert_eq!(
        fs::read_to_string(installed.join("src/lib.txt")).unwrap(),
        "workspace v1\n",
        "a lifecycle-prepared workspace package must not point at staging or source"
    );
    let lock = Lockfile::parse(&fs::read_to_string(app.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert!(lock.find("acme", "library").is_none());
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
        None,
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
        None,
        false,
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

    // And the build cache is populated for this platform (the entry key
    // folds in the build command hash, so don't recompute the exact path —
    // the linked module already proved the built tree's contents above).
    let platform = zed_interfaces::paths::current_platform();
    assert!(
        store.builds_root().join("v1").join(&platform).is_dir(),
        "build cache entry missing for {platform}"
    );
    assert!(store.build_size() > 0, "build cache should be populated");
}

/// Native packages are selected once for the complete graph, de-duplicated,
/// installed before package staging, and exposed to ordered lifecycle hooks.
/// A lifecycle cache hit never re-runs author code.
#[test]
#[cfg(unix)]
fn native_dependencies_and_install_hooks_are_graph_wide_staged_and_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let native_b = fixture_package(
        tmp.path(),
        "acme",
        "native-b",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "native b\n")],
    );
    fs::write(
        native_b.join(MANIFEST_FILE),
        format!(
            "{}\n[native-dependencies]\napt = [\"libssl-dev\", \"zlib1g-dev\"]\n",
            fs::read_to_string(native_b.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    publish_to(&registry, &native_b);

    let mut native_a_deps = BTreeMap::new();
    native_a_deps.insert("acme/native-b".to_string(), "^1".to_string());
    let native_a = fixture_package(
        tmp.path(),
        "acme",
        "native-a",
        "1.0.0",
        &native_a_deps,
        None,
        &[("src/lib.txt", "native a\n")],
    );
    fs::write(
        native_a.join(MANIFEST_FILE),
        format!(
            r#"{}
[native-dependencies]
apt = ["pkg-config", "libssl-dev"]

[hooks]
pre-install = ['printf "pre\n" >> "$HOOK_CAPTURE"; printf "pre\n" > lifecycle.txt; printf "%s\n" "$ZED_INSTALL_PHASE" > pre-phase.txt; printf "%s\n" "$ZED_NATIVE_MANAGER" > native-manager.txt; printf "%s\n" "$ZED_NATIVE_PACKAGES" > native-packages.json']
post-install = ['printf "post\n" >> "$HOOK_CAPTURE"; printf "post\n" >> lifecycle.txt']

[build]
command = 'printf "build\n" >> "$HOOK_CAPTURE"; printf "build\n" >> lifecycle.txt'
"#,
            fs::read_to_string(native_a.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    let native_a_sha = publish_to(&registry, &native_a);

    let consumer = tmp.path().join("consumer-native-lifecycle");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/native-a".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml(
            "zed-local",
            "consumer-native-lifecycle",
            "0.0.0",
            &deps,
            None,
        ),
    )
    .unwrap();

    let fake_bin = tmp.path().join("fake-native-bin");
    let native_capture = tmp.path().join("native-manager.capture");
    let hook_capture = tmp.path().join("hooks.capture");
    write_executable(
        &fake_bin.join("apt-get"),
        r#"#!/bin/sh
set -eu
{
  printf 'call\n'
  for arg do
    printf 'arg=%s\n' "$arg"
  done
} >> "$NATIVE_CAPTURE"
"#,
    );

    let mut denied = zed_install_command(&consumer, &cfg);
    denied
        .args(["--native-manager", "apt"])
        .env("PATH", path_with(&fake_bin))
        .env("NATIVE_CAPTURE", &native_capture)
        .env("HOOK_CAPTURE", &hook_capture);
    let denied_output = denied.output().expect("run denied native install");
    let denied_text = assert_child_failure(&denied_output);
    assert!(denied_text.contains("--allow-native-deps"), "{denied_text}");
    assert!(!consumer.join(MODULES_DIR).exists());
    assert!(!native_capture.exists());
    assert!(!hook_capture.exists());

    let mut hooks_denied = zed_install_command(&consumer, &cfg);
    hooks_denied
        .args(["--allow-native-deps", "--native-manager", "apt"])
        .env("PATH", path_with(&fake_bin))
        .env("NATIVE_CAPTURE", &native_capture)
        .env("HOOK_CAPTURE", &hook_capture);
    let hooks_denied_output = hooks_denied.output().expect("run denied hook install");
    let hooks_denied_text = assert_child_failure(&hooks_denied_output);
    assert!(
        hooks_denied_text.contains("--allow-install-hooks"),
        "{hooks_denied_text}"
    );
    assert!(
        !native_capture.exists(),
        "native package manager ran before lifecycle consent preflight"
    );

    let mut build_denied = zed_install_command(&consumer, &cfg);
    build_denied
        .args([
            "--allow-native-deps",
            "--allow-install-hooks",
            "--native-manager",
            "apt",
        ])
        .env("PATH", path_with(&fake_bin))
        .env("NATIVE_CAPTURE", &native_capture)
        .env("HOOK_CAPTURE", &hook_capture);
    let build_denied_output = build_denied.output().expect("run denied build install");
    let build_denied_text = assert_child_failure(&build_denied_output);
    assert!(
        build_denied_text.contains("--allow-build"),
        "{build_denied_text}"
    );
    assert!(
        !native_capture.exists(),
        "native package manager ran before complete lifecycle consent preflight"
    );

    let run_install = || {
        let mut command = zed_install_command(&consumer, &cfg);
        command
            .args([
                "--allow-native-deps",
                "--allow-install-hooks",
                "--allow-build",
                "--native-manager",
                "apt",
            ])
            .env("PATH", path_with(&fake_bin))
            .env("NATIVE_CAPTURE", &native_capture)
            .env("HOOK_CAPTURE", &hook_capture);
        command.output().expect("run native lifecycle install")
    };

    let first = run_install();
    assert_child_success(&first);

    let native_log = fs::read_to_string(&native_capture).unwrap();
    assert_eq!(native_log.lines().filter(|line| *line == "call").count(), 1);
    for package in ["pkg-config", "libssl-dev", "zlib1g-dev"] {
        let expected = format!("arg={package}");
        assert_eq!(
            native_log
                .lines()
                .filter(|line| *line == expected.as_str())
                .count(),
            1,
            "native package `{package}` was not installed exactly once: {native_log}"
        );
    }
    assert!(native_log.contains("arg=--\n"), "{native_log}");

    let installed = consumer.join(MODULES_DIR).join("acme").join("native-a");
    assert_eq!(
        fs::read_to_string(installed.join("lifecycle.txt")).unwrap(),
        "pre\nbuild\npost\n"
    );
    assert_eq!(
        fs::read_to_string(installed.join("pre-phase.txt")).unwrap(),
        "pre-install\n"
    );
    assert_eq!(
        fs::read_to_string(installed.join("native-manager.txt")).unwrap(),
        "apt\n"
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(
            fs::read_to_string(installed.join("native-packages.json"))
                .unwrap()
                .trim()
        )
        .unwrap(),
        vec!["pkg-config", "libssl-dev"]
    );
    assert_eq!(
        fs::read_to_string(&hook_capture).unwrap(),
        "pre\nbuild\npost\n"
    );

    let store = zed_cli::store::Store::new(&cfg.home);
    assert!(store.pkg_dir(&native_a_sha).is_dir());
    assert!(!store.pkg_dir(&native_a_sha).join("lifecycle.txt").exists());

    let second = run_install();
    assert_child_success(&second);
    assert_eq!(
        fs::read_to_string(&hook_capture).unwrap(),
        "pre\nbuild\npost\n",
        "cache hit re-ran package-authored lifecycle code"
    );
    let native_log = fs::read_to_string(&native_capture).unwrap();
    assert_eq!(
        native_log.lines().filter(|line| *line == "call").count(),
        2,
        "native prerequisites are checked once per install transaction"
    );
}

/// A failed post-install hook aborts the install transaction. The previous
/// modules tree is restored and neither the immutable source store nor the
/// lifecycle cache receives the staged mutation.
#[test]
fn failed_install_hook_rolls_back_project_and_does_not_promote_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let package = fixture_package(
        tmp.path(),
        "acme",
        "hook-failure",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "immutable source\n")],
    );
    fs::write(
        package.join(MANIFEST_FILE),
        format!(
            r#"{}
[hooks]
pre-install = ['printf "pre\n" >> "$HOOK_CAPTURE"; printf staged > staged.txt']
post-install = ['exit 17']
"#,
            fs::read_to_string(package.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    let source_sha = publish_to(&registry, &package);

    let consumer = tmp.path().join("consumer-hook-failure");
    fs::create_dir_all(consumer.join(MODULES_DIR)).unwrap();
    fs::write(consumer.join(MODULES_DIR).join("sentinel.txt"), "keep me\n").unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/hook-failure".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("zed-local", "consumer-hook-failure", "0.0.0", &deps, None),
    )
    .unwrap();

    let hook_capture = tmp.path().join("failed-hooks.capture");
    let mut command = zed_install_command(&consumer, &cfg);
    command
        .arg("--allow-install-hooks")
        .env("HOOK_CAPTURE", &hook_capture);
    let output = command.output().expect("run failing lifecycle install");
    let text = assert_child_failure(&output);
    assert!(text.contains("post-install hook 1"), "{text}");

    assert_eq!(
        fs::read_to_string(consumer.join(MODULES_DIR).join("sentinel.txt")).unwrap(),
        "keep me\n"
    );
    assert!(
        !consumer
            .join(MODULES_DIR)
            .join("acme")
            .join("hook-failure")
            .exists()
    );
    assert!(!consumer.join(LOCKFILE_FILE).exists());
    assert_eq!(fs::read_to_string(&hook_capture).unwrap(), "pre\n");

    let store = zed_cli::store::Store::new(&cfg.home);
    assert!(!store.pkg_dir(&source_sha).join("staged.txt").exists());
    let promoted_staged_file = if store.builds_root().exists() {
        walkdir::WalkDir::new(store.builds_root())
            .into_iter()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name() == "staged.txt")
    } else {
        false
    };
    assert!(
        !promoted_staged_file,
        "failed staging tree reached the lifecycle cache"
    );
}

/// An interactive Nix shell may expose `NIX_STORE` without being a derivation
/// build. Zed must still use its own content-addressed profile, inject that
/// profile into hooks, and reuse both the native profile and lifecycle cache.
#[test]
#[cfg(unix)]
fn nix_shell_uses_a_zed_managed_profile_and_reuses_it() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let package = fixture_package(
        tmp.path(),
        "acme",
        "nix-profile-native",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "managed nix profile\n")],
    );
    fs::write(
        package.join(MANIFEST_FILE),
        format!(
            r#"{}
[native-dependencies]
nix = ["hello"]

[hooks]
post-install = [
  'printf "%s\n" "$ZED_NATIVE_PROFILE" > native-profile.txt',
  'printf "%s\n" "$ZED_NATIVE_PACKAGES" > native-packages.txt',
  'native-tool > native-tool-output.txt',
]
"#,
            fs::read_to_string(package.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    publish_to(&registry, &package);

    let consumer = tmp.path().join("consumer-nix-profile-native");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/nix-profile-native".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml(
            "zed-local",
            "consumer-nix-profile-native",
            "0.0.0",
            &deps,
            None,
        ),
    )
    .unwrap();

    let fake_bin = tmp.path().join("fake-nix-bin");
    let fake_store = tmp.path().join("fake-nix-store");
    let capture = tmp.path().join("nix.capture");
    write_executable(
        &fake_bin.join("nix"),
        r#"#!/bin/sh
set -eu
printf 'call\n' >> "$NIX_CAPTURE"
profile=''
while [ "$#" -gt 0 ]; do
  printf 'arg=%s\n' "$1" >> "$NIX_CAPTURE"
  if [ "$1" = '--profile' ]; then
    shift
    profile="$1"
    printf 'arg=%s\n' "$1" >> "$NIX_CAPTURE"
  fi
  shift
done
test -n "$profile"
mkdir -p "$(dirname "$profile")"
ln -s "$NIX_FAKE_STORE" "$profile"
"#,
    );
    write_executable(
        &fake_store.join("bin/native-tool"),
        "#!/bin/sh\nprintf 'native tool from managed profile\\n'\n",
    );

    let run = || {
        let mut command = zed_install_command(&consumer, &cfg);
        command
            .args([
                "--allow-native-deps",
                "--allow-install-hooks",
                "--native-manager",
                "nix",
            ])
            .env("PATH", path_with(&fake_bin))
            .env("NIX_STORE", "/nix/store")
            .env("NIX_CAPTURE", &capture)
            .env("NIX_FAKE_STORE", &fake_store);
        command.output().expect("run managed Nix profile install")
    };

    let first = run();
    assert_child_success(&first);
    let installed = consumer
        .join(MODULES_DIR)
        .join("acme")
        .join("nix-profile-native");
    let profile = fs::read_to_string(installed.join("native-profile.txt")).unwrap();
    let profile = PathBuf::from(profile.trim());
    assert!(profile.starts_with(cfg.home.join("native/nix/v1")));
    assert_eq!(
        fs::read_to_string(installed.join("native-packages.txt")).unwrap(),
        "[\"hello\"]\n"
    );
    assert_eq!(
        fs::read_to_string(installed.join("native-tool-output.txt")).unwrap(),
        "native tool from managed profile\n"
    );

    let first_capture = fs::read_to_string(&capture).unwrap();
    assert_eq!(
        first_capture.lines().filter(|line| *line == "call").count(),
        1
    );
    assert!(first_capture.contains("arg=--profile\n"), "{first_capture}");
    assert!(
        first_capture.contains("arg=nixpkgs#hello\n"),
        "{first_capture}"
    );

    let second = run();
    assert_child_success(&second);
    let second_capture = fs::read_to_string(&capture).unwrap();
    assert_eq!(
        second_capture
            .lines()
            .filter(|line| *line == "call")
            .count(),
        1,
        "the content-addressed managed profile should be reused: {second_capture}"
    );
}

/// Nix derivations provide native inputs declaratively. Zed validates the
/// `.zpkg.toml` route but never mutates a Nix profile inside the sandbox.
#[test]
fn nix_build_requires_declared_native_inputs_acknowledgement_without_running_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let cfg = test_config(tmp.path(), &registry_dir);

    let package = fixture_package(
        tmp.path(),
        "acme",
        "nix-native",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("src/lib.txt", "nix native\n")],
    );
    fs::write(
        package.join(MANIFEST_FILE),
        format!(
            "{}\n[native-dependencies]\nnix = [\"pkg-config\", \"openssl\"]\n",
            fs::read_to_string(package.join(MANIFEST_FILE)).unwrap()
        ),
    )
    .unwrap();
    publish_to(&registry, &package);

    let consumer = tmp.path().join("consumer-nix-native");
    fs::create_dir_all(&consumer).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/nix-native".to_string(), "^1".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("zed-local", "consumer-nix-native", "0.0.0", &deps, None),
    )
    .unwrap();

    let mut denied = zed_install_command(&consumer, &cfg);
    denied
        .args(["--allow-native-deps", "--native-manager", "nix"])
        .env("NIX_BUILD_TOP", tmp.path().join("nix-build-top"));
    let denied_output = denied.output().expect("run Nix native validation");
    let denied_text = assert_child_failure(&denied_output);
    assert!(
        denied_text.contains("nativeBuildInputs/buildInputs"),
        "{denied_text}"
    );
    assert!(!consumer.join(MODULES_DIR).exists());

    let mut allowed = zed_install_command(&consumer, &cfg);
    allowed
        .args(["--allow-native-deps", "--native-manager", "nix"])
        .env("NIX_BUILD_TOP", tmp.path().join("nix-build-top"))
        .env("ZED_PKG_NATIVE_DEPS_PROVIDED", "1");
    let allowed_output = allowed.output().expect("run acknowledged Nix install");
    assert_child_success(&allowed_output);
    let allowed_text = output_text(&allowed_output);
    assert!(
        allowed_text.contains("validated 2 native prerequisite"),
        "{allowed_text}"
    );
    assert!(
        consumer
            .join(MODULES_DIR)
            .join("acme")
            .join("nix-native")
            .join("src/lib.txt")
            .is_file()
    );
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
        None,
        false,
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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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
            None,
            false,
        )
        .unwrap();
    }

    let store = zed_cli::store::Store::new(&cfg.home);
    // Both projects alive: nothing to collect even at age 0.
    let report = store.gc(std::time::Duration::ZERO, false).unwrap();
    assert_eq!(report.entries_removed, 0);
    assert_eq!(store.status().0, 1);

    // Delete both projects: the entry is unreferenced and age 0 collects it.
    fs::remove_dir_all(&keeper).unwrap();
    fs::remove_dir_all(&goner).unwrap();
    let report = store.gc(std::time::Duration::ZERO, false).unwrap();
    assert_eq!(report.entries_removed, 1);
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
        None,
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
        None,
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

// ---------------------------------------------------------------------------
// grafted coverage: container-safe bins, LRU gc, workspace lockfile shape,
// build-cache hit semantics, consumer overrides (merged from the parallel
// feature branch)

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
fn bin_fixture(root: &Path, name: &str, bin_name: &str, script: &str) -> PathBuf {
    let dir = root.join(format!("binf-{name}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(MANIFEST_FILE),
        format!(
            r#"[package]
org = "acme"
name = "{name}"
version = "1.0.0"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/{name}"

[bin]
{bin_name} = "bin/{bin_name}"
"#
        ),
    )
    .unwrap();
    fs::write(dir.join("LICENSE"), "MIT\n").unwrap();
    write_executable(&dir.join("bin").join(bin_name), script);
    dir
}

#[cfg(unix)]
fn bin_consumer(root: &Path, name: &str, deps: &[&str]) -> PathBuf {
    let mut map = BTreeMap::new();
    for d in deps {
        map.insert(format!("acme/{d}"), "^1".to_string());
    }
    fixture_package(root, "consumerorg", name, "0.0.1", &map, None, &[])
}

#[cfg(unix)]
#[test]
fn hoisted_bins_are_container_safe_in_copy_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    publish_to(
        &registry,
        &bin_fixture(tmp.path(), "ctool", "ctool", "#!/bin/sh\nexit 0\n"),
    );

    let consumer = bin_consumer(tmp.path(), "copybinapp", &["ctool"]);
    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Copy,
        Adapter::None,
        false,
        None,
        false,
    )
    .unwrap();

    let shim = consumer.join(MODULES_DIR).join(".bin").join("ctool");
    assert!(
        !fs::symlink_metadata(&shim)
            .unwrap()
            .file_type()
            .is_symlink(),
        "copy mode must materialize a real bin file, not a symlink"
    );
    assert_eq!(ops::run(&consumer, "ctool", &[]).unwrap(), 0);
}

#[test]
fn gc_reclaims_entries_older_than_threshold() {
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let lib = fixture_package(
        tmp.path(),
        "acme",
        "gclib",
        "0.1.0",
        &BTreeMap::new(),
        None,
        &[("f.txt", "x\n"), ("LICENSE", "MIT\n")],
    );
    publish_to(&registry, &lib);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "gcapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/gclib".to_string(), "^0.1".to_string());
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
        None,
        false,
    )
    .unwrap();

    let store = zed_cli::store::Store::new(&cfg.home);
    assert_eq!(store.status().0, 1);

    // A huge threshold reclaims nothing (fresh, still-referenced entry);
    // dry-run never deletes.
    let long = store.gc(Duration::from_secs(3600), true).unwrap();
    assert_eq!(long.entries_removed, 0);
    assert_eq!(store.status().0, 1);

    // Drop the referencing project: age 0 treats everything as stale.
    fs::remove_dir_all(&consumer).unwrap();
    let zero = store.gc(Duration::from_secs(0), false).unwrap();
    assert!(
        zero.entries_removed >= 1,
        "expected at least the store entry reclaimed"
    );
    assert!(zero.freed > 0);
    assert_eq!(store.status().0, 0, "store entry should be gone");
}

#[test]
fn workspace_installs_members_against_one_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    // A published external dependency.
    let ext = fixture_package(
        tmp.path(),
        "acme",
        "ext",
        "1.0.0",
        &BTreeMap::new(),
        None,
        &[("e.txt", "e\n"), ("LICENSE", "MIT\n")],
    );
    publish_to(&registry, &ext);

    // Workspace root: a package that also declares members.
    let root = tmp.path().join("mono");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        r#"[package]
org = "acme"
name = "mono"
version = "0.0.0"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/mono"

[workspace]
members = ["packages/*", "apps/*"]
"#,
    )
    .unwrap();

    // Local member with no deps.
    let util = root.join("packages").join("util");
    fs::create_dir_all(&util).unwrap();
    fs::write(
        util.join(MANIFEST_FILE),
        manifest_toml("acme", "util", "0.1.0", &BTreeMap::new(), None),
    )
    .unwrap();
    write_files(&util, &[("util.txt", "u\n")]);

    // Local member depending on the local util (path) and the external ext.
    let web = root.join("apps").join("web");
    fs::create_dir_all(&web).unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/util".to_string(), "^0.1".to_string());
    deps.insert("acme/ext".to_string(), "^1".to_string());
    fs::write(
        web.join(MANIFEST_FILE),
        manifest_toml("acme", "web", "0.1.0", &deps, None),
    )
    .unwrap();
    write_files(&web, &[("web.txt", "w\n")]);

    let cfg = test_config(tmp.path(), &registry_dir);
    // Install from the member that pulls both a sibling and an external dep.
    ops::install(
        &web,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
        None,
        false,
    )
    .unwrap();

    // The local member is path-linked to its source (live editing)...
    let util_link = web.join(MODULES_DIR).join("acme").join("util");
    assert!(
        fs::symlink_metadata(&util_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(util_link.join("util.txt").exists());
    assert_eq!(
        fs::canonicalize(&util_link).unwrap(),
        fs::canonicalize(&util).unwrap(),
        "local member must link to its source dir, not the store"
    );

    // ...while the external dep resolves from the shared store.
    let ext_link = web.join(MODULES_DIR).join("acme").join("ext");
    assert!(ext_link.join("e.txt").exists());
    assert!(
        fs::canonicalize(&ext_link)
            .unwrap()
            .starts_with(fs::canonicalize(cfg.home.join("store")).unwrap())
    );

    // The lockfile pins the external dep but not the path-linked member.
    let lock = Lockfile::parse(&fs::read_to_string(web.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert!(
        lock.find("acme", "ext").is_some(),
        "external must be locked"
    );
    assert!(
        lock.find("acme", "util").is_none(),
        "workspace-local members are path-linked, not locked"
    );
}

fn build_fixture(root: &Path, name: &str, version: &str, command: &str) -> PathBuf {
    let dir = root.join(format!("bf-{name}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(MANIFEST_FILE),
        format!(
            r#"[package]
org = "acme"
name = "{name}"
version = "{version}"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/{name}"

[build]
command = '''{command}'''
"#
        ),
    )
    .unwrap();
    write_files(&dir, &[("src.txt", "source\n"), ("LICENSE", "MIT\n")]);
    dir
}

#[test]
fn build_step_compiles_and_is_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    // Each build appends to a counter outside the sandbox, so we can prove the
    // build ran exactly once across two installs (the second is a cache hit).
    let counter = tmp.path().join("build-runs.log");
    let command = format!(
        "echo run >> \"{}\"; echo compiled > built.txt",
        counter.display()
    );
    let pkg = build_fixture(tmp.path(), "cachednative", "1.0.0", &command);
    publish_to(&registry, &pkg);

    let consumer = fixture_package(
        tmp.path(),
        "consumerorg",
        "buildapp",
        "0.0.1",
        &{
            let mut deps = BTreeMap::new();
            deps.insert("acme/cachednative".to_string(), "^1".to_string());
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
        true,
        None,
        false,
    )
    .unwrap();

    let module = consumer.join(MODULES_DIR).join("acme").join("cachednative");
    assert!(module.join("built.txt").exists(), "compiled output missing");
    assert!(module.join("src.txt").exists(), "source should remain too");
    let store = zed_cli::store::Store::new(&cfg.home);
    assert!(store.build_size() > 0, "build cache should be populated");
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "build should have run once"
    );

    // Re-install after wiping modules: a build-cache hit, no rebuild.
    fs::remove_dir_all(consumer.join(MODULES_DIR)).unwrap();
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        true,
        None,
        false,
    )
    .unwrap();
    assert!(module.join("built.txt").exists());
    assert_eq!(
        fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "second install must hit the build cache, not rebuild"
    );
}

#[test]
fn consumer_can_override_dependency_build() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    // The package's own build produces built.txt...
    let pkg = build_fixture(
        tmp.path(),
        "patchable",
        "1.0.0",
        "echo compiled > built.txt",
    );
    publish_to(&registry, &pkg);

    // ...but the consumer patches it to produce a different artifact instead.
    let consumer = tmp.path().join("override-consumer");
    fs::create_dir_all(&consumer).unwrap();
    fs::write(
        consumer.join(MANIFEST_FILE),
        r#"[package]
org = "consumerorg"
name = "overrideapp"
version = "0.0.1"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/consumerorg/overrideapp"

[dependencies]
"acme/patchable" = "^1"

[overrides.build."acme/patchable"]
command = '''echo overridden > overridden.txt'''
"#,
    )
    .unwrap();

    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        true,
        None,
        false,
    )
    .unwrap();

    let module = consumer.join(MODULES_DIR).join("acme").join("patchable");
    assert!(
        module.join("overridden.txt").exists(),
        "consumer override should have run"
    );
    assert!(
        !module.join("built.txt").exists(),
        "upstream build must not run when overridden"
    );
}

/// `[install].dir` relocates the installed tree. Every command that locates
/// that tree must agree on where it is — install writes it, `zed run` finds
/// hoisted bins in it, and `zed remove` unlinks from it. A command still
/// hardcoding `zed_modules/` would silently look in the wrong place (bins
/// unrunnable, removed deps left on disk).
#[cfg(unix)]
#[test]
fn install_dir_is_honored_by_install_run_and_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    publish_to(
        &registry,
        &bin_fixture(tmp.path(), "reloctool", "reloctool", "#!/bin/sh\nexit 0\n"),
    );

    let consumer = tmp.path().join("reloc-consumer");
    fs::create_dir_all(&consumer).unwrap();
    fs::write(
        consumer.join(MANIFEST_FILE),
        r#"[package]
org = "consumerorg"
name = "relocapp"
version = "0.0.1"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/consumerorg/relocapp"

[dependencies]
"acme/reloctool" = "^1"

[install]
dir = ".vendor/.zed"
"#,
    )
    .unwrap();

    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
        false,
        None,
        false,
    )
    .unwrap();

    // install: the tree lands in the configured dir, not the default.
    let relocated = consumer.join(".vendor/.zed").join("acme").join("reloctool");
    assert!(
        relocated.join(MANIFEST_FILE).exists(),
        "package should install under [install].dir"
    );
    assert!(
        !consumer.join(MODULES_DIR).exists(),
        "default zed_modules/ must not be created when [install].dir is set"
    );

    // run: the hoisted bin resolves from the relocated .bin dir.
    assert!(
        consumer
            .join(".vendor/.zed")
            .join(".bin")
            .join("reloctool")
            .exists(),
        "bins should hoist into the relocated tree"
    );
    assert_eq!(
        ops::run(&consumer, "reloctool", &[]).unwrap(),
        0,
        "zed run must find bins under [install].dir"
    );

    // remove: unlinks from the relocated dir rather than leaving it behind.
    ops::remove(&consumer, &cfg, "acme/reloctool").unwrap();
    assert!(
        !relocated.exists(),
        "zed remove must unlink from [install].dir, not leave a stale copy"
    );
}

/// A relocated `[install].dir` must never be published. The static default
/// excludes only cover `zed_modules/**`, so pack has to derive the pattern
/// from the manifest — otherwise an author using `.vendor/.zed` ships their
/// whole dependency tree (and, in symlink mode, links into their own store).
#[test]
fn a_relocated_install_dir_is_never_published() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("reloc-publisher");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(MANIFEST_FILE),
        r#"[package]
org = "acme"
name = "relocpub"
version = "1.0.0"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/relocpub"

[install]
dir = ".vendor/.zed"
"#,
    )
    .unwrap();
    write_files(
        &project,
        &[
            ("src/lib.txt", "real source\n"),
            ("LICENSE", "MIT\n"),
            // A dependency tree sitting in the relocated install dir.
            (".vendor/.zed/acme/dep/.zpkg.toml", "[package]\n"),
            (".vendor/.zed/acme/dep/huge.bin", "vendored bytes\n"),
            (".vendor/.zed/.bin/tool", "#!/bin/sh\n"),
        ],
    );

    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let packed = pack::pack(&project, &manifest, None).unwrap();
    let entries = archive_entries(&packed.path);

    assert!(entries.contains(&"pkg/src/lib.txt".to_string()));
    assert!(entries.contains(&"pkg/LICENSE".to_string()));
    assert!(
        !entries.iter().any(|e| e.contains(".vendor")),
        "relocated install dir leaked into the artifact: {entries:?}"
    );
}

// --- per-language packages: publish by language, install by language --------

/// A repository that ships the same client for several languages, declaring one
/// target per language subtree. Publishing fans this out to one package per
/// language, each named `<name>-<target>`.
const POLYGLOT_CLIENTS: &str = r#"[package]
org = "acme"
name = "acme-clients"
version = "1.1.2"
description = "Acme API clients"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/acme-clients"

[targets.nodejs]
dir = "clients/ts"

[targets.java]
dir = "clients/java"

[targets.golang]
dir = "clients/go"
"#;

/// Publish every per-language package a polyglot repo fans out to, returning
/// `(published name, sha256)` pairs.
fn publish_polyglot(registry: &FileRegistry, project: &Path) -> Vec<(String, String)> {
    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let packed = pack::pack_all(project, &manifest, None).unwrap();
    let mut out = Vec::new();
    for target in &packed {
        let meta =
            ops::build_publish_meta(&target.manifest, &target.packed, Some("deadbeef".into()));
        registry.publish(&meta, &target.packed.path, None).unwrap();
        out.push((
            target.manifest.package.name.clone(),
            target.packed.sha256.clone(),
        ));
    }
    out
}

fn polyglot_clients_repo(root: &Path) -> PathBuf {
    let dir = root.join("acme-clients");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(MANIFEST_FILE), POLYGLOT_CLIENTS).unwrap();
    write_files(
        &dir,
        &[
            ("clients/ts/index.ts", "export const client = 1;\n"),
            ("clients/ts/package.json", "{\"name\":\"acme-clients\"}\n"),
            ("clients/java/Client.java", "class Client {}\n"),
            ("clients/java/pom.xml", "<project/>\n"),
            ("clients/go/client.go", "package client\n"),
            ("clients/go/go.mod", "module acme/clients\n"),
            ("LICENSE", "MIT\n"),
        ],
    );
    dir
}

#[test]
fn one_repo_publishes_one_isolated_package_per_language() {
    // The premise of the whole design: a Java consumer must download Java bytes
    // only. Each artifact is re-rooted at its own language subtree and carries
    // none of the others.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let repo = polyglot_clients_repo(tmp.path());

    let manifest = Manifest::parse(POLYGLOT_CLIENTS).unwrap();
    let packed = pack::pack_all(&repo, &manifest, None).unwrap();
    assert_eq!(packed.len(), 3, "one artifact per declared target");

    let names: Vec<&str> = packed
        .iter()
        .map(|p| p.manifest.package.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "acme-clients-golang",
            "acme-clients-java",
            "acme-clients-nodejs"
        ],
        "packages are named <repo>-<language>"
    );

    for target in &packed {
        let entries = archive_entries(&target.packed.path);
        let joined = entries.join("\n");
        match target.manifest.package.name.as_str() {
            "acme-clients-java" => {
                assert!(joined.contains("Client.java"), "{joined}");
                // The decisive assertion: no other language's source rides along.
                assert!(
                    !joined.contains("index.ts"),
                    "java artifact leaked ts: {joined}"
                );
                assert!(
                    !joined.contains("client.go"),
                    "java artifact leaked go: {joined}"
                );
            }
            "acme-clients-nodejs" => {
                assert!(joined.contains("index.ts"), "{joined}");
                assert!(
                    !joined.contains("Client.java"),
                    "node artifact leaked java: {joined}"
                );
            }
            "acme-clients-golang" => {
                assert!(joined.contains("client.go"), "{joined}");
                assert!(
                    !joined.contains("index.ts"),
                    "go artifact leaked ts: {joined}"
                );
            }
            other => panic!("unexpected package {other}"),
        }
    }
    // And every one of them is publishable and self-describing.
    let published = publish_polyglot(&registry, &repo);
    assert_eq!(published.len(), 3);
}

#[test]
fn each_published_language_package_declares_its_ecosystem() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = polyglot_clients_repo(tmp.path());
    let manifest = Manifest::parse(POLYGLOT_CLIENTS).unwrap();
    let packed = pack::pack_all(&repo, &manifest, None).unwrap();

    for target in &packed {
        let pkg = &target.manifest.package;
        let expected = match pkg.name.as_str() {
            "acme-clients-java" => "jvm",
            "acme-clients-nodejs" => "npm",
            "acme-clients-golang" => "gomod",
            other => panic!("unexpected {other}"),
        };
        assert_eq!(
            pkg.ecosystem().as_str(),
            expected,
            "{} must declare its ecosystem so consumers can be guarded",
            pkg.name
        );
    }
}

#[test]
fn a_wrong_language_package_is_refused_in_a_real_project() {
    // The headline requirement: installing the Java client into a Node project
    // must fail, naming the package that would have worked — not quietly drop
    // unusable files into zed_modules/.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let repo = polyglot_clients_repo(tmp.path());
    publish_polyglot(&registry, &repo);

    // A Node consumer, identified by its package.json.
    let consumer = tmp.path().join("node-app");
    fs::create_dir_all(&consumer).unwrap();
    fs::write(consumer.join("package.json"), "{\"name\":\"app\"}\n").unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/acme-clients-java".to_string(), "^1.1.2".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("acme", "node-app", "0.1.0", &deps, None),
    )
    .unwrap();

    let cfg = test_config(tmp.path(), &registry_dir);
    let err = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
        None,
        false,
    )
    .expect_err("a jvm package must not install into a node project");
    let msg = format!("{err:#}");
    assert!(msg.contains("jvm"), "{msg}");
    assert!(msg.contains("npm"), "{msg}");
    assert!(
        msg.contains("acme/acme-clients-nodejs"),
        "the error must name the package that would work: {msg}"
    );

    // The escape hatch works, for the rare deliberate case.
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
        None,
        true,
    )
    .expect("--allow-ecosystem-mismatch must override the guard");
}

#[test]
fn the_matching_language_package_installs_and_wires_its_toolchain() {
    // The other half: the right package installs cleanly and leaves behind the
    // wiring its ecosystem needs.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());
    let repo = polyglot_clients_repo(tmp.path());
    publish_polyglot(&registry, &repo);

    let consumer = tmp.path().join("go-app");
    fs::create_dir_all(&consumer).unwrap();
    fs::write(consumer.join("go.mod"), "module app\n").unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/acme-clients-golang".to_string(), "^1.1.2".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("acme", "go-app", "0.1.0", &deps, None),
    )
    .unwrap();

    let cfg = test_config(tmp.path(), &registry_dir);
    let outcome = ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
        None,
        false,
    )
    .expect("the golang package belongs in a go project");
    assert_eq!(outcome.installed.len(), 1);

    // Only the Go source landed.
    let installed = consumer
        .join(MODULES_DIR)
        .join("acme")
        .join("acme-clients-golang");
    assert!(
        installed.join("client.go").exists(),
        "go source must be present"
    );
    assert!(!installed.join("index.ts").exists(), "no ts source");

    // Go wiring: a go.work the toolchain can be pointed at.
    let go_work = consumer.join(".zed").join("go.work");
    assert!(go_work.exists(), "a go project must get .zed/go.work");
    let work = fs::read_to_string(&go_work).unwrap();
    assert!(work.contains("acme-clients-golang"), "{work}");

    // And the adapter-independent index every build system can read.
    let index = fs::read_to_string(consumer.join(".zed").join("paths.json")).unwrap();
    assert!(index.contains("acme/acme-clients-golang"), "{index}");
    assert!(index.contains("\"ecosystem\": \"gomod\""), "{index}");
    assert!(index.contains("\"language\": \"golang\""), "{index}");
}

#[test]
fn a_node_project_resolves_a_nodejs_target_despite_the_spelling_difference() {
    // Project inference yields `node`; this repo spells its target `nodejs`.
    // Without synonym resolution the install would fail despite the package
    // shipping exactly what the consumer needs.
    let tmp = tempfile::tempdir().unwrap();
    let registry_dir = tmp.path().join("registry");
    let registry = FileRegistry::new(registry_dir.clone());

    // A single polyglot package (not fanned out), installed with slicing.
    let repo = polyglot_clients_repo(tmp.path());
    let manifest = Manifest::parse(POLYGLOT_CLIENTS).unwrap();
    let packed = pack::pack(&repo, &manifest, None).unwrap();
    let meta = ops::build_publish_meta(&manifest, &packed, Some("deadbeef".into()));
    registry.publish(&meta, &packed.path, None).unwrap();

    let consumer = tmp.path().join("node-app");
    fs::create_dir_all(&consumer).unwrap();
    fs::write(consumer.join("package.json"), "{\"name\":\"app\"}\n").unwrap();
    let mut deps = BTreeMap::new();
    deps.insert("acme/acme-clients".to_string(), "^1.1.2".to_string());
    fs::write(
        consumer.join(MANIFEST_FILE),
        manifest_toml("acme", "node-app", "0.1.0", &deps, None),
    )
    .unwrap();

    let cfg = test_config(tmp.path(), &registry_dir);
    ops::install(
        &consumer,
        &cfg,
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
        // No explicit target: inference must find `nodejs` from `node`.
        None,
        false,
    )
    .expect("a node project must resolve the `nodejs` target");

    let installed = consumer.join(MODULES_DIR).join("acme").join("acme-clients");
    assert!(
        installed.join("index.ts").exists(),
        "the ts subtree must be at the install root"
    );
    assert!(
        !installed.join("Client.java").exists(),
        "the java subtree must not be materialized"
    );
}
