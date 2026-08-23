//! The local registry on media that is not the system disk: external and
//! virtual drives, container bind mounts, and image builds.
//!
//! The configured registry is an empty `file://` directory throughout, so any
//! install that succeeds here succeeded because a local registration satisfied
//! it. What each test then checks is *how* that registration was materialized,
//! and whether an absent volume is diagnosed as absent rather than as deleted.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn project(dir: &Path, org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    let mut manifest = format!(
        "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\n\
         [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
    );
    if !dependencies.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            manifest.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    fs::write(dir.join(".zpkg.toml"), manifest).unwrap();
    fs::write(dir.join("marker.txt"), format!("{org}/{name}@{version}\n")).unwrap();
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    registry: String,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    // macOS hands out `/var/...` temp paths that canonicalize to
    // `/private/var/...`, and the index stores canonical paths.
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("zed-home");
    let empty = root.join("empty-registry");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&empty).unwrap();
    Fixture {
        _temp: temp,
        root,
        home,
        registry: format!("file://{}", empty.display()),
    }
}

impl Fixture {
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        zed(cwd, &self.home, &self.registry, &BTreeMap::new(), args)
    }

    fn run_env(&self, cwd: &Path, env: &[(&str, String)], args: &[&str]) -> Output {
        let env: BTreeMap<&str, String> = env.iter().cloned().collect();
        zed(cwd, &self.home, &self.registry, &env, args)
    }

    fn index_path(&self) -> PathBuf {
        self.home.join("local-registry").join("index.json")
    }

    fn index(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.index_path()).unwrap()).unwrap()
    }

    fn write_index(&self, value: &serde_json::Value) {
        fs::write(
            self.index_path(),
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }

    /// Rewrite one entry's recorded volume, which is how a test stands in for
    /// an external disk without needing one.
    fn set_volume(&self, name: &str, volume: serde_json::Value) {
        let mut index = self.index();
        for entry in index["entries"].as_array_mut().unwrap() {
            if entry["name"] == name {
                entry["volume"] = volume.clone();
            }
        }
        self.write_index(&index);
    }
}

fn removable(mount_point: &Path) -> serde_json::Value {
    serde_json::json!({
        "kind": "removable",
        "mount_point": mount_point.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// external and virtual disks

#[cfg(unix)]
#[test]
fn a_checkout_on_removable_media_is_copied_even_in_symlink_mode() {
    // A symlink into an external disk dangles the moment the disk is ejected,
    // and the breakage surfaces far from the install that caused it.
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "library"]), "register");

    // Pretend the checkout lives on a mounted external volume. The mount point
    // is the root itself, which exists, so the entry stays usable.
    fx.set_volume("widget", removable(&fx.root));

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run(&app, &["install"]);
    assert_ok(&installed, "install from removable media");
    assert!(
        stdout(&installed).contains("(copied)"),
        "the install must say it copied:\n{}",
        stdout(&installed)
    );

    let materialized = app.join("zed_modules").join("acme").join("widget");
    assert!(
        !fs::symlink_metadata(&materialized).unwrap().file_type().is_symlink(),
        "removable media must never be symlinked into a project"
    );

    // The disk goes away; the installed tree keeps working because it is a copy.
    fs::remove_dir_all(&library).unwrap();
    assert!(materialized.join("marker.txt").is_file());
}

#[cfg(unix)]
#[test]
fn an_explicit_symlink_policy_still_links_from_removable_media() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "library"]), "register");
    fx.set_volume("widget", removable(&fx.root));

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run_env(
        &app,
        &[("ZED_PKG_LOCAL_LINK_POLICY", "symlink".to_string())],
        &["install"],
    );
    assert_ok(&installed, "install with an explicit symlink policy");
    let materialized = app.join("zed_modules").join("acme").join("widget");
    assert!(
        fs::symlink_metadata(&materialized).unwrap().file_type().is_symlink(),
        "an operator who asks for links gets links"
    );
}

#[test]
fn an_unmounted_volume_is_unavailable_and_survives_prune() {
    // An external drive that is not attached this afternoon is not a mistake
    // to clean up. A deleted directory is.
    let fx = fixture();
    let unplugged = fx.root.join("unplugged");
    let deleted = fx.root.join("deleted");
    project(&unplugged, "acme", "unplugged", "1.0.0", &[]);
    project(&deleted, "acme", "deleted", "1.0.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "unplugged"]), "register");
    assert_ok(&fx.run(&fx.root, &["local", "register", "deleted"]), "register");

    fx.set_volume("unplugged", removable(&fx.root.join("no-such-mount")));
    fs::remove_dir_all(&deleted).unwrap();

    let listed = fx.run(&fx.root, &["local", "list", "--json"]);
    assert_ok(&listed, "zed local list --json");
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    let health: Vec<(String, String)> = report
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["package"].as_str().unwrap().to_string(),
                entry["health"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let unplugged_health = &health
        .iter()
        .find(|(package, _)| package == "acme/unplugged")
        .unwrap()
        .1;
    assert!(
        unplugged_health.starts_with("unavailable"),
        "an unmounted volume must not read as a deleted directory: {unplugged_health}"
    );

    let pruned = fx.run(&fx.root, &["local", "prune"]);
    assert_ok(&pruned, "zed local prune");
    let listed = stdout(&fx.run(&fx.root, &["local", "list"]));
    assert!(
        listed.contains("acme/unplugged"),
        "prune must keep an entry whose disk is merely unplugged: {listed}"
    );
    assert!(
        !listed.contains("acme/deleted"),
        "prune must drop an entry whose directory is gone: {listed}"
    );
}

#[test]
fn an_install_whose_only_registration_is_unavailable_says_so_and_falls_back() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "library"]), "register");
    fx.set_volume("widget", removable(&fx.root.join("no-such-mount")));

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run(&app, &["install"]);
    // The registry is empty, so the fallback fails — but it must fail as a
    // registry miss, after naming the unavailable volume, not as a mysterious
    // absence.
    assert!(!installed.status.success());
    let stderr = String::from_utf8_lossy(&installed.stderr);
    assert!(
        stderr.contains("unavailable"),
        "the unusable registration must be reported: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn register_records_a_per_entry_link_preference() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    let registered = fx.run(&fx.root, &["local", "register", "library", "--link", "copy"]);
    assert_ok(&registered, "register --link copy");

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run(&app, &["install"]);
    assert_ok(&installed, "install with a per-entry copy preference");
    let materialized = app.join("zed_modules").join("acme").join("widget");
    assert!(!fs::symlink_metadata(&materialized).unwrap().file_type().is_symlink());
}

// ---------------------------------------------------------------------------
// containers and docker volumes

#[cfg(unix)]
#[test]
fn a_host_shaped_index_resolves_through_a_bind_mount_path_map() {
    // The host registered `/host/codes/library`; in the container the same
    // bytes are under `<root>/codes`. One path map makes the shared index
    // usable on both sides — and a registration made in the container writes
    // the host spelling back, so the host still understands it.
    let fx = fixture();
    let codes = fx.root.join("codes");
    let library = codes.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    let map = format!("/host/codes={}", codes.display());

    let registered = fx.run_env(
        &fx.root,
        &[("ZED_PKG_LOCAL_REGISTRY_PATH_MAP", map.clone())],
        &["local", "register", "codes/library"],
    );
    assert_ok(&registered, "register through a path map");
    let raw = fs::read_to_string(fx.index_path()).unwrap();
    assert!(
        raw.contains("/host/codes/library"),
        "the index must hold the host spelling:\n{raw}"
    );

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run_env(
        &app,
        &[("ZED_PKG_LOCAL_REGISTRY_PATH_MAP", map)],
        &["install"],
    );
    assert_ok(&installed, "install through a path map");
    assert!(stdout(&installed).contains("local acme/widget@1.2.0"));
    assert_eq!(
        fs::read_link(app.join("zed_modules").join("acme").join("widget")).unwrap(),
        library
    );
}

#[test]
fn a_host_shaped_index_without_a_map_falls_back_instead_of_resolving_wrongly() {
    let fx = fixture();
    let codes = fx.root.join("codes");
    let library = codes.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    let map = format!("/host/codes={}", codes.display());
    assert_ok(
        &fx.run_env(
            &fx.root,
            &[("ZED_PKG_LOCAL_REGISTRY_PATH_MAP", map)],
            &["local", "register", "codes/library"],
        ),
        "register through a path map",
    );

    // Same index, no mapping: the recorded path does not exist here.
    let listed = fx.run(&fx.root, &["local", "list", "--json"]);
    assert_ok(&listed, "zed local list --json");
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(report[0]["path"], "/host/codes/library");
    assert_eq!(report[0]["selectable"], false);
}

#[cfg(unix)]
#[test]
fn the_ephemeral_flag_copies_every_registration_for_image_builds() {
    // `docker build` with `RUN --mount=type=bind,...`: the source tree exists
    // for one step and is absent from the image, so nothing may be linked.
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "library"]), "register");

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run_env(
        &app,
        &[("ZED_PKG_LOCAL_REGISTRY_EPHEMERAL", "1".to_string())],
        &["install"],
    );
    assert_ok(&installed, "install with the ephemeral flag");
    let materialized = app.join("zed_modules").join("acme").join("widget");
    assert!(
        !fs::symlink_metadata(&materialized).unwrap().file_type().is_symlink(),
        "an image build layer must be self-contained"
    );

    // The build mount goes away when the step ends; the layer still works.
    fs::remove_dir_all(&library).unwrap();
    assert!(materialized.join("marker.txt").is_file());
}

#[test]
fn an_index_shared_through_a_volume_is_read_from_that_volume() {
    // `docker run -v .../local-registry:/zed-local` plus
    // ZED_PKG_LOCAL_REGISTRY_FILE. Nothing else about resolution changes.
    let fx = fixture();
    let mounted = fx.root.join("mounted").join("index.json");
    fs::create_dir_all(mounted.parent().unwrap()).unwrap();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);

    let env = [(
        "ZED_PKG_LOCAL_REGISTRY_FILE",
        mounted.display().to_string(),
    )];
    assert_ok(
        &fx.run_env(&fx.root, &env, &["local", "register", "library"]),
        "register into a mounted index",
    );
    assert!(mounted.is_file(), "the index must live on the mount");
    assert!(!fx.index_path().exists(), "and not in the default location");

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run_env(&app, &env, &["install"]);
    assert_ok(&installed, "install from a mounted index");
    assert!(stdout(&installed).contains("local acme/widget@1.2.0"));
}

// ---------------------------------------------------------------------------
// doctor

#[test]
fn doctor_explains_this_machines_view() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(&fx.run(&fx.root, &["local", "register", "library"]), "register");

    let report = fx.run(&fx.root, &["local", "doctor"]);
    assert_ok(&report, "zed local doctor");
    let text = stdout(&report);
    assert!(
        text.contains(&fx.index_path().display().to_string()),
        "the report must name the index it read:\n{text}"
    );
    assert!(text.contains("link policy    auto"), "{text}");
    assert!(text.contains("acme/widget"), "{text}");

    let json = fx.run_env(
        &fx.root,
        &[(
            "ZED_PKG_LOCAL_REGISTRY_PATH_MAP",
            format!("/host/codes={}", fx.root.display()),
        )],
        &["local", "doctor", "--json"],
    );
    assert_ok(&json, "zed local doctor --json");
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(value["link_policy"], "auto");
    assert_eq!(value["ephemeral"], false);
    assert_eq!(value["path_map"][0]["from"], "/host/codes");
    assert_eq!(value["path_map"][0]["to"], fx.root.display().to_string());
    assert_eq!(value["entries"][0]["package"], "acme/widget");
}

#[test]
fn doctor_reports_the_ephemeral_and_copy_settings_it_was_given() {
    let fx = fixture();
    let json = fx.run_env(
        &fx.root,
        &[
            ("ZED_PKG_LOCAL_REGISTRY_EPHEMERAL", "1".to_string()),
            ("ZED_PKG_LOCAL_LINK_POLICY", "copy".to_string()),
        ],
        &["local", "doctor", "--json"],
    );
    assert_ok(&json, "zed local doctor --json");
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(value["ephemeral"], true);
    assert_eq!(value["link_policy"], "copy");
}
