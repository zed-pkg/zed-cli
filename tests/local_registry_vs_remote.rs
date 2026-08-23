//! Local registry versus remote registry.
//!
//! Every other local-registry test points at an empty registry, so a
//! successful install proves the local path worked. These tests do the
//! opposite: the same package exists in *both* places with different bytes, so
//! what lands in `zed_modules/` is an unambiguous answer to which source won.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use zed_cli::pack::pack;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::registry::{PackageMetadata, VersionMetadata};

const ORG: &str = "acme";
const NAME: &str = "widget";

fn zed(cwd: &Path, home: &Path, registry: &str, env: &BTreeMap<&str, String>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .current_dir(cwd)
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_REGISTRY", registry)
        .env_remove("ZED_PKG_LOCAL_REGISTRY")
        .env_remove("ZED_PKG_LOCAL_REGISTRY_FILE")
        .env_remove("ZED_PKG_LOCAL_REGISTRY_PATH_MAP")
        .env_remove("ZED_PKG_LOCAL_LINK_POLICY")
        .env_remove("ZED_PKG_LOCAL_REGISTRY_EPHEMERAL")
        .env_remove("ZED_PKG_INTERACTIVE")
        .env_remove("ZED_PKG_TOKEN")
        .env_remove("ZED_PKG_FROZEN");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn manifest_text(org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) -> String {
    let mut text = format!(
        "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\n\
         [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
    );
    if !dependencies.is_empty() {
        text.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            text.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    text
}

fn write_project(dir: &Path, name: &str, version: &str, marker: &str, dependencies: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(".zpkg.toml"),
        manifest_text(ORG, name, version, dependencies),
    )
    .unwrap();
    fs::write(dir.join("marker.txt"), marker).unwrap();
}

/// Publish a source tree into a directory-backed `file://` registry.
fn publish(registry: &Path, source: &Path, staging: &Path) {
    let text = fs::read_to_string(source.join(".zpkg.toml")).unwrap();
    let manifest = Manifest::parse(&text).unwrap();
    let packed = pack(source, &manifest, Some(staging)).unwrap();

    let artifacts = registry.join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    let artifact = artifacts.join(format!("{}.tar.gz", packed.sha256));
    fs::copy(&packed.path, &artifact).unwrap();

    let package_dir = registry
        .join("packages")
        .join(&manifest.package.org)
        .join(&manifest.package.name);
    fs::create_dir_all(package_dir.join("versions")).unwrap();
    let version = VersionMetadata {
        org: manifest.package.org.clone(),
        name: manifest.package.name.clone(),
        version: manifest.package.version.clone(),
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
        vcs_tag: format!("v{}", manifest.package.version),
        vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        download_url: format!("file://{}", artifact.display()),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
    };
    fs::write(
        package_dir
            .join("versions")
            .join(format!("{}.json", manifest.package.version)),
        serde_json::to_string_pretty(&version).unwrap(),
    )
    .unwrap();
    let metadata = PackageMetadata {
        org: manifest.package.org.clone(),
        name: manifest.package.name.clone(),
        description: manifest.package.description.clone(),
        vcs: manifest.package.repository.vcs,
        repo_url: manifest.package.repository.url.clone(),
        version_scheme: manifest.package.version_scheme,
        latest: Some(manifest.package.version.clone()),
        tags: Vec::new(),
        versions: vec![manifest.package.version.clone()],
    };
    fs::write(
        package_dir.join("package.json"),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

/// The same package published remotely and checked out locally, with
/// distinguishable payloads.
struct World {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    registry_dir: PathBuf,
    registry: String,
    checkout: PathBuf,
    app: PathBuf,
}

fn world(remote_version: &str, local_version: &str, requirement: &str) -> World {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("zed-home");
    let registry_dir = root.join("remote-registry");
    let checkout = root.join("checkout");
    let app = root.join("app");
    fs::create_dir_all(&home).unwrap();

    let published = root.join("published");
    write_project(&published, NAME, remote_version, "REMOTE\n", &[]);
    publish(&registry_dir, &published, &root.join("staging"));

    write_project(&checkout, NAME, local_version, "LOCAL\n", &[]);
    write_project(
        &app,
        "app",
        "0.1.0",
        "APP\n",
        &[(&format!("{ORG}/{NAME}"), requirement)],
    );

    World {
        _temp: temp,
        root,
        home,
        registry: format!("file://{}", registry_dir.display()),
        registry_dir,
        checkout,
        app,
    }
}

impl World {
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        zed(cwd, &self.home, &self.registry, &BTreeMap::new(), args)
    }

    fn register(&self) {
        assert_ok(
            &self.run(&self.root, &["local", "register", "checkout"]),
            "zed local register",
        );
    }

    fn installed_marker(&self) -> String {
        fs::read_to_string(
            self.app
                .join("zed_modules")
                .join(ORG)
                .join(NAME)
                .join("marker.txt"),
        )
        .unwrap()
    }
}

// ---------------------------------------------------------------------------

#[test]
fn a_registered_checkout_wins_over_the_published_artifact() {
    let world = world("1.2.0", "1.2.0", "^1");
    world.register();
    assert_ok(&world.run(&world.app, &["install"]), "install");
    assert_eq!(world.installed_marker(), "LOCAL\n");
}

#[test]
fn without_a_registration_the_remote_registry_is_used() {
    let world = world("1.2.0", "1.2.0", "^1");
    assert_ok(&world.run(&world.app, &["install"]), "install");
    assert_eq!(world.installed_marker(), "REMOTE\n");
}

#[test]
fn local_registry_off_falls_back_to_the_remote_registry() {
    let world = world("1.2.0", "1.2.0", "^1");
    world.register();
    assert_ok(
        &world.run(&world.app, &["install", "--local-registry", "off"]),
        "install --local-registry off",
    );
    assert_eq!(world.installed_marker(), "REMOTE\n");
}

#[test]
fn a_registered_checkout_installs_with_the_remote_registry_gone() {
    // The point of the feature: the registry may be down, unreachable, or (as
    // here) absent entirely, and a package already on this disk still installs.
    let world = world("1.2.0", "1.2.0", "^1");
    world.register();
    fs::remove_dir_all(&world.registry_dir).unwrap();
    assert_ok(&world.run(&world.app, &["install"]), "install");
    assert_eq!(world.installed_marker(), "LOCAL\n");
}

#[test]
fn a_checkout_that_does_not_satisfy_the_requirement_falls_through_to_remote() {
    // Remote publishes 1.2.0; the checkout is 2.0.0 and the consumer asks for
    // ^1. Preferring local must never mean ignoring the manifest.
    let world = world("1.2.0", "2.0.0", "^1");
    world.register();
    let installed = world.run(&world.app, &["install"]);
    assert_ok(&installed, "install");
    assert_eq!(world.installed_marker(), "REMOTE\n");
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&installed.stderr),
        stdout(&installed)
    );
    assert!(
        reported.contains("registered locally"),
        "the fallback should say a registration existed but did not match:\n{reported}"
    );
}

#[test]
fn a_deleted_checkout_falls_through_to_remote_instead_of_failing() {
    let world = world("1.2.0", "1.2.0", "^1");
    world.register();
    fs::remove_dir_all(&world.checkout).unwrap();
    assert_ok(&world.run(&world.app, &["install"]), "install");
    assert_eq!(world.installed_marker(), "REMOTE\n");
}

#[test]
fn local_registry_only_refuses_to_reach_the_registry_at_all() {
    let world = world("1.2.0", "2.0.0", "^1");
    world.register();
    let installed = world.run(&world.app, &["install", "--local-registry", "only"]);
    assert!(
        !installed.status.success(),
        "an unsatisfiable dependency must be an error, not a quiet download"
    );
    let stderr = String::from_utf8_lossy(&installed.stderr);
    assert!(stderr.contains("local-registry=only"), "{stderr}");
    assert!(
        !world.app.join("zed_modules").join(ORG).join(NAME).exists(),
        "nothing may be materialized from the network in `only` mode"
    );
}

// -- frozen replay ----------------------------------------------------------

#[test]
fn a_frozen_install_replays_the_lock_rather_than_the_checkout() {
    // `--frozen` promises exactly what `.zpkg.lock` pins. Machine-global
    // registrations must not silently substitute source for a pinned artifact,
    // or "frozen" would mean something different on every laptop.
    let world = world("1.2.0", "1.2.0", "^1");
    assert_ok(&world.run(&world.app, &["install"]), "install");
    assert_eq!(world.installed_marker(), "REMOTE\n");
    assert!(world.app.join(".zpkg.lock").is_file());

    world.register();
    assert_ok(&world.run(&world.app, &["install", "--frozen"]), "install --frozen");
    assert_eq!(world.installed_marker(), "REMOTE\n");
}

#[test]
fn local_registry_prefer_lets_a_checkout_satisfy_a_frozen_install() {
    let world = world("1.2.0", "1.2.0", "^1");
    assert_ok(&world.run(&world.app, &["install"]), "install");
    world.register();
    assert_ok(
        &world.run(
            &world.app,
            &["install", "--frozen", "--local-registry", "prefer"],
        ),
        "install --frozen --local-registry prefer",
    );
    assert_eq!(world.installed_marker(), "LOCAL\n");
}

// -- transitive resolution --------------------------------------------------

#[test]
fn a_checkouts_own_dependencies_keep_resolving_remotely() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("zed-home");
    let registry_dir = root.join("remote-registry");
    fs::create_dir_all(&home).unwrap();

    // Remote holds acme/leaf@1.0.0 only.
    let leaf = root.join("published-leaf");
    write_project(&leaf, "leaf", "1.0.0", "REMOTE-LEAF\n", &[]);
    publish(&registry_dir, &leaf, &root.join("staging"));

    // Local holds acme/widget@1.2.0, which depends on acme/leaf.
    let checkout = root.join("checkout");
    write_project(
        &checkout,
        NAME,
        "1.2.0",
        "LOCAL\n",
        &[(&format!("{ORG}/leaf"), "^1")],
    );

    let app = root.join("app");
    write_project(
        &app,
        "app",
        "0.1.0",
        "APP\n",
        &[(&format!("{ORG}/{NAME}"), "^1")],
    );

    let registry = format!("file://{}", registry_dir.display());
    let env = BTreeMap::new();
    assert_ok(
        &zed(&root, &home, &registry, &env, &["local", "register", "checkout"]),
        "zed local register",
    );
    assert_ok(&zed(&app, &home, &registry, &env, &["install"]), "install");

    let modules = app.join("zed_modules").join(ORG);
    assert_eq!(
        fs::read_to_string(modules.join(NAME).join("marker.txt")).unwrap(),
        "LOCAL\n"
    );
    assert_eq!(
        fs::read_to_string(modules.join("leaf").join("marker.txt")).unwrap(),
        "REMOTE-LEAF\n",
        "a local checkout's own dependencies still come from the registry"
    );
}
