//! Hermetic CLI round-trips for nested graphs, published self-dependencies,
//! workspace self-tests, and symlink/copy materialization transitions.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use zed_cli::config::Config;
use zed_cli::ops;
use zed_cli::pack;
use zed_cli::registry::{FileRegistry, Registry};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

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

fn manifest_toml(
    org: &str,
    name: &str,
    version: &str,
    dependencies: &BTreeMap<String, String>,
) -> String {
    let mut manifest = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"
description = "nested/self install-mode fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/{org}/{name}"
"#
    );
    if !dependencies.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            manifest.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    manifest
}

fn fixture_package(
    root: &Path,
    org: &str,
    name: &str,
    version: &str,
    dependencies: &BTreeMap<String, String>,
    payload: &str,
) -> PathBuf {
    let directory = root.join(format!("{org}-{name}-{}", version.replace(['.', '+'], "-")));
    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(
        directory.join(MANIFEST_FILE),
        manifest_toml(org, name, version, dependencies),
    )
    .unwrap();
    fs::write(directory.join("src/payload.txt"), payload).unwrap();
    directory
}

fn write_workspace_manifest(root: &Path) {
    fs::create_dir_all(root.join("packages")).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        r#"[package]
org = "workspace"
name = "nested-self-fixtures"
version = "0.0.0"
description = "workspace root for nested/self install-mode tests"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/zed-pkg/zed-cli"

[workspace]
members = ["packages/*"]
"#,
    )
    .unwrap();
}

fn publish_to(registry: &FileRegistry, project: &Path) {
    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let packed = pack::pack(project, &manifest, None).unwrap();
    let metadata = ops::build_publish_meta(&manifest, &packed, Some("deadbeef".into()));
    registry.publish(&metadata, &packed.path, None).unwrap();
}

fn registry_uri(path: &Path) -> String {
    fs::create_dir_all(path).unwrap();
    let absolute = path.canonicalize().unwrap();
    #[cfg(windows)]
    {
        format!("file:///{}", absolute.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        format!("file://{}", absolute.display())
    }
}

fn test_config(root: &Path, registry: &Path) -> Config {
    Config {
        registry: registry_uri(registry),
        home: root.join("zed-home"),
        token: None,
        auth_url: "http://127.0.0.1:8120".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    }
}

fn install_command(project: &Path, config: &Config, mode: &str, frozen: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.current_dir(project);
    for key in CLEAN_INSTALL_ENV {
        command.env_remove(key);
    }
    command
        .arg("--registry")
        .arg(&config.registry)
        .arg("--home")
        .arg(&config.home)
        .arg("install")
        .arg("--install-mode")
        .arg(mode)
        .arg("--adapter")
        .arg("none");
    if frozen {
        command.arg("--frozen");
    }
    command
}

fn run_install(project: &Path, config: &Config, mode: &str, frozen: bool) -> Output {
    install_command(project, config, mode, frozen)
        .output()
        .unwrap()
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "zed install failed with {:?}:\n{}",
        output.status.code(),
        output_text(output)
    );
}

fn assert_failure(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "zed install unexpectedly succeeded:\n{}",
        output_text(output)
    );
    output_text(output)
}

fn locked(project: &Path) -> Lockfile {
    Lockfile::parse(&fs::read_to_string(project.join(LOCKFILE_FILE)).unwrap()).unwrap()
}

fn package_path(project: &Path, org: &str, name: &str) -> PathBuf {
    project.join(MODULES_DIR).join(org).join(name)
}

fn remove_if_present(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).unwrap();
    } else {
        fs::remove_file(path).unwrap();
    }
}

fn assert_copy(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(
        metadata.is_dir(),
        "copy target is not a directory: {path:?}"
    );
    assert!(
        !metadata.file_type().is_symlink(),
        "copy target is unexpectedly a symlink: {path:?}"
    );
}

fn assert_symlink_or_platform_copy(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    #[cfg(unix)]
    assert!(
        metadata.file_type().is_symlink(),
        "symlink mode did not create a symlink: {path:?}"
    );
    #[cfg(not(unix))]
    assert!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "non-Unix symlink mode must normalize to copy mode: {path:?}"
    );
}

#[test]
fn deep_nested_graph_replays_and_transitions_between_install_modes() {
    const DEPTH: usize = 32;

    let temporary = tempfile::tempdir().unwrap();
    let registry_directory = temporary.path().join("registry");
    let registry = FileRegistry::new(registry_directory.clone());
    let package_sources = temporary.path().join("published");

    for index in (0..DEPTH).rev() {
        let name = format!("nested-{index:02}");
        let mut dependencies = BTreeMap::new();
        if index + 1 < DEPTH {
            dependencies.insert(
                format!("acme/nested-{:02}", index + 1),
                "=1.0.0".to_string(),
            );
        }
        let package = fixture_package(
            &package_sources,
            "acme",
            &name,
            "1.0.0",
            &dependencies,
            &format!("payload-{index:02}\n"),
        );
        publish_to(&registry, &package);
    }

    let mut dependencies = BTreeMap::new();
    dependencies.insert("acme/nested-00".to_string(), "=1.0.0".to_string());
    let consumer = fixture_package(
        temporary.path(),
        "consumer",
        "deep-graph",
        "0.1.0",
        &dependencies,
        "consumer\n",
    );
    let config = test_config(temporary.path(), &registry_directory);

    let symlink_install = run_install(&consumer, &config, "symlink", false);
    assert_success(&symlink_install);
    let lock_before = fs::read(consumer.join(LOCKFILE_FILE)).unwrap();
    assert_eq!(locked(&consumer).packages.len(), DEPTH);
    for index in 0..DEPTH {
        let path = package_path(&consumer, "acme", &format!("nested-{index:02}"));
        assert_symlink_or_platform_copy(&path);
        assert_eq!(
            fs::read_to_string(path.join("src/payload.txt")).unwrap(),
            format!("payload-{index:02}\n")
        );
    }

    let frozen_copy = run_install(&consumer, &config, "copy", true);
    assert_success(&frozen_copy);
    assert_eq!(fs::read(consumer.join(LOCKFILE_FILE)).unwrap(), lock_before);
    for index in 0..DEPTH {
        assert_copy(&package_path(
            &consumer,
            "acme",
            &format!("nested-{index:02}"),
        ));
    }

    // A copied project is self-contained after the global store disappears.
    remove_if_present(&config.home);
    assert_eq!(
        fs::read_to_string(package_path(&consumer, "acme", "nested-31").join("src/payload.txt"),)
            .unwrap(),
        "payload-31\n"
    );

    // Cold frozen copy replay must re-fetch exact lock artifacts without
    // re-resolving or changing the lock.
    remove_if_present(&consumer.join(MODULES_DIR));
    let cold_frozen_copy = run_install(&consumer, &config, "copy", true);
    assert_success(&cold_frozen_copy);
    assert_eq!(fs::read(consumer.join(LOCKFILE_FILE)).unwrap(), lock_before);
    assert_eq!(locked(&consumer).packages.len(), DEPTH);

    // The same immutable lock can safely transition back to symlink mode.
    let frozen_symlink = run_install(&consumer, &config, "symlink", true);
    assert_success(&frozen_symlink);
    assert_eq!(fs::read(consumer.join(LOCKFILE_FILE)).unwrap(), lock_before);
    assert_symlink_or_platform_copy(&package_path(&consumer, "acme", "nested-00"));
}

#[test]
fn published_package_self_dependency_resolves_once_in_both_modes() {
    let temporary = tempfile::tempdir().unwrap();
    let registry_directory = temporary.path().join("registry");
    let registry = FileRegistry::new(registry_directory.clone());

    let mut self_dependencies = BTreeMap::new();
    self_dependencies.insert("acme/self-loop".to_string(), "=1.0.0".to_string());
    let self_package = fixture_package(
        &temporary.path().join("published"),
        "acme",
        "self-loop",
        "1.0.0",
        &self_dependencies,
        "published-self\n",
    );
    publish_to(&registry, &self_package);

    let mut consumer_dependencies = BTreeMap::new();
    consumer_dependencies.insert("acme/self-loop".to_string(), "=1.0.0".to_string());
    let consumer = fixture_package(
        temporary.path(),
        "consumer",
        "self-loop-consumer",
        "0.1.0",
        &consumer_dependencies,
        "consumer\n",
    );
    let config = test_config(temporary.path(), &registry_directory);

    let symlink_install = run_install(&consumer, &config, "symlink", false);
    assert_success(&symlink_install);
    let lock_before = fs::read(consumer.join(LOCKFILE_FILE)).unwrap();
    let lock = locked(&consumer);
    assert_eq!(lock.packages.len(), 1);
    assert_eq!(lock.packages[0].org, "acme");
    assert_eq!(lock.packages[0].name, "self-loop");
    assert_eq!(lock.packages[0].version, "1.0.0");
    assert_symlink_or_platform_copy(&package_path(&consumer, "acme", "self-loop"));

    let frozen_copy = run_install(&consumer, &config, "copy", true);
    assert_success(&frozen_copy);
    assert_eq!(fs::read(consumer.join(LOCKFILE_FILE)).unwrap(), lock_before);
    assert_copy(&package_path(&consumer, "acme", "self-loop"));
    assert_eq!(
        fs::read_to_string(package_path(&consumer, "acme", "self-loop").join("src/payload.txt"),)
            .unwrap(),
        "published-self\n"
    );
}

#[test]
fn workspace_member_can_test_its_own_published_coordinate() {
    let temporary = tempfile::tempdir().unwrap();
    let registry_directory = temporary.path().join("registry");
    let registry = FileRegistry::new(registry_directory.clone());

    let published = fixture_package(
        &temporary.path().join("published"),
        "acme",
        "self-test",
        "1.0.0",
        &BTreeMap::new(),
        "published-v1\n",
    );
    publish_to(&registry, &published);

    let workspace = temporary.path().join("workspace");
    write_workspace_manifest(&workspace);
    let control = fixture_package(
        &workspace.join("packages"),
        "acme",
        "workspace-control",
        "1.0.0",
        &BTreeMap::new(),
        "workspace-control\n",
    );
    let mut dependencies = BTreeMap::new();
    dependencies.insert("acme/self-test".to_string(), "=1.0.0".to_string());
    dependencies.insert("acme/workspace-control".to_string(), "=1.0.0".to_string());
    let member = fixture_package(
        &workspace.join("packages"),
        "acme",
        "self-test",
        "2.0.0",
        &dependencies,
        "workspace-v2\n",
    );
    let config = test_config(temporary.path(), &registry_directory);

    let symlink_install = run_install(&member, &config, "symlink", false);
    assert_success(&symlink_install);
    let installed = package_path(&member, "acme", "self-test");
    let installed_control = package_path(&member, "acme", "workspace-control");
    assert_symlink_or_platform_copy(&installed);
    assert_symlink_or_platform_copy(&installed_control);
    assert_eq!(
        fs::read_to_string(installed.join("src/payload.txt")).unwrap(),
        "published-v1\n",
        "a root self-dependency must test the published artifact, not source"
    );
    assert_eq!(
        fs::read_to_string(installed_control.join("src/payload.txt")).unwrap(),
        "workspace-control\n",
        "the sibling control proves workspace discovery remained active"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(&installed_control).unwrap(),
        control.canonicalize().unwrap(),
        "ordinary workspace dependencies must remain source-linked"
    );
    let lock_before = fs::read(member.join(LOCKFILE_FILE)).unwrap();
    let lock = locked(&member);
    assert_eq!(lock.packages.len(), 1);
    assert_eq!(lock.packages[0].version, "1.0.0");

    #[cfg(unix)]
    {
        let target = fs::read_link(&installed).unwrap();
        assert!(target.is_absolute(), "workspace install link was relative");
        assert_ne!(
            target.canonicalize().unwrap(),
            member.canonicalize().unwrap(),
            "self-test dependency was silently linked back to workspace source"
        );
    }

    let frozen_copy = run_install(&member, &config, "copy", true);
    assert_success(&frozen_copy);
    assert_eq!(fs::read(member.join(LOCKFILE_FILE)).unwrap(), lock_before);
    assert_copy(&installed);
    assert_copy(&installed_control);
    assert_eq!(
        fs::read_to_string(installed.join("src/payload.txt")).unwrap(),
        "published-v1\n"
    );
    assert_eq!(
        fs::read_to_string(installed_control.join("src/payload.txt")).unwrap(),
        "workspace-control\n"
    );
}

#[cfg(unix)]
#[test]
fn workspace_source_copy_is_symlink_free_and_rejects_escapes_atomically() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let registry_directory = temporary.path().join("registry");
    let workspace = temporary.path().join("workspace");
    write_workspace_manifest(&workspace);

    let library = fixture_package(
        &workspace.join("packages"),
        "acme",
        "workspace-lib",
        "1.0.0",
        &BTreeMap::new(),
        "workspace-source\n",
    );
    fs::create_dir_all(library.join("real")).unwrap();
    fs::write(library.join("real/data.txt"), "internal-link-data\n").unwrap();
    symlink("real", library.join("alias")).unwrap();

    let mut dependencies = BTreeMap::new();
    dependencies.insert("acme/workspace-lib".to_string(), "=1.0.0".to_string());
    let application = fixture_package(
        &workspace.join("packages"),
        "acme",
        "workspace-app",
        "1.0.0",
        &dependencies,
        "workspace-app\n",
    );
    let config = test_config(temporary.path(), &registry_directory);

    let symlink_install = run_install(&application, &config, "symlink", false);
    assert_success(&symlink_install);
    let installed = package_path(&application, "acme", "workspace-lib");
    assert!(
        fs::symlink_metadata(&installed)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&installed).unwrap(),
        library.canonicalize().unwrap(),
        "workspace symlink target must be absolute and canonical"
    );

    let copy_install = run_install(&application, &config, "copy", false);
    assert_success(&copy_install);
    assert_copy(&installed);
    assert_eq!(
        fs::read_to_string(installed.join("alias/data.txt")).unwrap(),
        "internal-link-data\n"
    );
    assert!(
        !fs::symlink_metadata(installed.join("alias"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "copy mode must not emit package symlinks"
    );

    // An external workspace symlink is rejected before replacing the previous
    // good copy. This prevents both package-root escape and destructive partial
    // updates.
    let outside = temporary.path().join("outside-secret.txt");
    fs::write(&outside, "outside\n").unwrap();
    symlink(&outside, library.join("escape")).unwrap();
    let rejected = run_install(&application, &config, "copy", false);
    let error = assert_failure(&rejected);
    assert!(error.contains("outside package root"), "{error}");
    assert_eq!(
        fs::read_to_string(installed.join("alias/data.txt")).unwrap(),
        "internal-link-data\n",
        "failed replacement must preserve the previous complete copy"
    );
    assert!(!installed.join("escape").exists());
}
