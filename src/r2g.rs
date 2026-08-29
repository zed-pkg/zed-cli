//! `zed r2g` — roundtrip-to-consumer pre-publish check, after
//! [r2g](https://github.com/oresoftware/r2g).
//!
//! The failure mode it kills is "works in my repo, breaks once installed."
//! Instead of testing the working tree, r2g exercises the *published
//! artifact* the way a real consumer would: it packs the package (the same
//! pruned, deterministic tarball `zed publish` uploads), publishes it either
//! to a throwaway `file://` registry (the safe default) or, only when
//! explicitly requested, to the configured HTTP(S) Zed server, installs it
//! into a mock consumer project, and runs the package's `publish.smoke_test`
//! against the installed copy — optionally inside a fresh OCI container so
//! the artifact is proven in a clean, host-independent environment (fresh
//! `$HOME`, distro libraries, no host toolchain leaking in). In isolated mode,
//! a configured `file://` registry is snapshotted into the throwaway registry
//! so declared dependencies can resolve without ever mutating the caller's
//! registry. If it passes here, it will pass for your users.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use uuid::Uuid;
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::VersionScheme;

use crate::cli::{Adapter, ContainerRuntime, InstallMode, R2gRegistryMode};
use crate::config::{Config, read_manifest, write_manifest};
use crate::interactive;
use crate::ops::{build_publish_meta, install};
use crate::pack;
use crate::registry::registry_for;
use crate::store::human_size;

/// Options for `zed r2g`, mirroring its CLI flags (all also `ZED_PKG_R2G_*`
/// environment variables per the flags-2-env convention).
#[derive(Debug, Clone)]
pub struct R2gOptions {
    /// Registry boundary to exercise. Isolated is the safe default.
    pub registry_mode: R2gRegistryMode,
    /// Run the install + smoke test inside a throwaway OCI container.
    pub docker: bool,
    /// Base image for container mode.
    pub image: String,
    /// Container runtime; auto-detected when `None`.
    pub runtime: Option<ContainerRuntime>,
    /// Parent dir for the throwaway workspace; defaults to `<home>/r2g`.
    pub root: Option<PathBuf>,
    /// Delete the workspace after a successful run instead of leaving it.
    pub clean: bool,
}

impl Default for R2gOptions {
    fn default() -> Self {
        Self {
            registry_mode: R2gRegistryMode::Isolated,
            docker: false,
            image: "debian:stable-slim".to_string(),
            runtime: None,
            root: None,
            clean: false,
        }
    }
}

/// Mount point for the mock consumer project inside the container.
const CONTAINER_CONSUMER: &str = "/r2g/consumer";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryTarget {
    url: String,
    token: Option<String>,
    persistent: bool,
}

fn prepare_registry_target(
    cfg: &Config,
    mode: R2gRegistryMode,
    registry_dir: &Path,
) -> Result<RegistryTarget> {
    match mode {
        R2gRegistryMode::Isolated => Ok(RegistryTarget {
            url: format!("file://{}", registry_dir.display()),
            token: None,
            persistent: false,
        }),
        R2gRegistryMode::Server => {
            if cfg.registry.starts_with("file://") {
                bail!(
                    "r2g --registry-mode server requires an HTTP(S) registry; \
                     omit the flag for the safe isolated file-registry roundtrip"
                );
            }
            if !(cfg.registry.starts_with("http://") || cfg.registry.starts_with("https://")) {
                bail!(
                    "r2g --registry-mode server requires an http:// or https:// registry, got {}",
                    cfg.registry
                );
            }
            Ok(RegistryTarget {
                url: cfg.registry.clone(),
                token: cfg.resolve_token()?,
                persistent: true,
            })
        }
    }
}

pub fn run(project: &Path, cfg: &Config, opts: &R2gOptions) -> Result<()> {
    let manifest = read_manifest(project)?;
    let full = manifest.full_name();

    // 1. A throwaway UUID-v4 workspace under the user's home directory
    //    (r2g-style). A unique run root prevents stale state and makes
    //    concurrent host/container checks independent without deleting
    //    another run's diagnostics.
    let root = opts.root.clone().unwrap_or_else(|| cfg.home.join("r2g"));
    let workspace = root.join(format!(
        "{}-{}-{}",
        manifest.package.org,
        manifest.package.name,
        Uuid::new_v4()
    ));
    let registry_dir = workspace.join("registry");
    let consumer_dir = workspace.join("consumer");
    let home_dir = workspace.join("home");
    println!("r2g: workspace {}", workspace.display());

    // 2. Isolated mode treats a configured file:// registry as a dependency
    //    input, never as the output registry. Server mode intentionally uses
    //    the configured HTTP(S) registry itself for both publish and install.
    if opts.registry_mode == R2gRegistryMode::Isolated {
        snapshot_configured_file_registry(&cfg.registry, &registry_dir)?;
    }
    let registry_target = prepare_registry_target(cfg, opts.registry_mode, &registry_dir)?;
    if registry_target.persistent {
        println!(
            "r2g: SERVER MODE — publishing to {}; this version persists after the run",
            registry_target.url
        );
    }

    // 3. Pack the exact artifact `zed publish` would upload (tarball roundtrip).
    interactive::confirm(
        cfg.interactive,
        &format!("r2g step 1/5: pack {} into {}", full, workspace.display()),
    )?;
    let packed = pack::pack(project, &manifest, Some(&workspace.join("pack")))?;
    println!(
        "r2g: packed {} ({} files, {} excluded by publish rules)",
        human_size(packed.size),
        packed.file_count,
        packed.excluded_count
    );

    // 4. Publish the exact tarball through the same registry abstraction as
    //    `zed publish`: private file storage by default, HTTP Rust server only
    //    after the explicit server-mode flag. Preserve `zed publish` retry
    //    semantics: byte-identical same-version retries are accepted, while a
    //    changed artifact at an immutable version is rejected before upload.
    let meta = build_publish_meta(&manifest, &packed, None);
    let registry = registry_for(&registry_target.url)?;
    let identity = &meta.manifest.package;
    let already_published =
        match registry.get_version(&identity.org, &identity.name, &identity.version) {
            Ok(existing) if existing.sha256 == meta.sha256 => {
                println!(
                    "r2g: already published {}/{}@{} with identical sha256; reusing it",
                    identity.org, identity.name, identity.version
                );
                true
            }
            Ok(existing) => {
                bail!(
                    "{}/{}@{} already exists with sha256 {}; refusing to replace it with {}",
                    identity.org,
                    identity.name,
                    identity.version,
                    existing.sha256,
                    meta.sha256
                );
            }
            Err(_) => false,
        };
    if !already_published {
        interactive::confirm(
            cfg.interactive,
            &format!("r2g step 2/5: publish {full} to {}", registry_target.url),
        )?;
        registry.publish(&meta, &packed.path, registry_target.token.as_deref())?;
    }

    // 5. Synthesize a mock consumer that depends on exactly this version.
    let mut dependencies = BTreeMap::new();
    dependencies.insert(full.clone(), format!("={}", manifest.package.version));
    let consumer_manifest = Manifest {
        workspace: None,
        package: PackageSection {
            org: "zed-local".to_string(),
            name: "consumer".to_string(),
            version: "0.0.0".to_string(),
            version_scheme: VersionScheme::Semver,
            description: Some(format!("r2g mock consumer of {full}")),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: "https://localhost/zed-local/consumer".to_string(),
            },
            keywords: Vec::new(),
            // The mock consumer is deliberately language-neutral: r2g drives
            // the target/ecosystem it is testing through the install flags.
            language: Default::default(),
            ecosystem: Default::default(),
            artifacts: Default::default(),
        },
        dependencies,
        build_dependencies: BTreeMap::new(),
        native_dependencies: Default::default(),
        hooks: Default::default(),
        lifecycle: Default::default(),
        build: None,
        overrides: Default::default(),
        bin: BTreeMap::new(),
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
        install: Default::default(),
        interop: Default::default(),
        targets: Default::default(),
    };
    interactive::confirm(
        cfg.interactive,
        &format!("r2g step 3/5: create a fresh mock consumer of {full}"),
    )?;
    fs::create_dir_all(&consumer_dir)?;
    write_manifest(&consumer_dir, &consumer_manifest)?;

    // 6. Install it the way a consumer would, from the selected registry into
    //    a throwaway store. Container mode uses copy install so the installed
    //    files are self-contained (no store symlinks) and can be bind-mounted
    //    into the container — the same guarantee `--install-mode copy` gives
    //    OCI image builds.
    let mode = if opts.docker {
        InstallMode::Copy
    } else {
        InstallMode::Symlink
    };
    let test_cfg = Config {
        registry: registry_target.url.clone(),
        home: home_dir,
        token: registry_target.token.clone(),
        auth_url: cfg.auth_url.clone(),
        supabase_url: cfg.supabase_url.clone(),
        supabase_key: cfg.supabase_key.clone(),
        interactive: cfg.interactive,
    };
    // The author is roundtripping their own package, so running its [build]
    // step is consented — that's part of "as close to a real install as
    // possible".
    // No target: the mock consumer is language-agnostic, so a polyglot package
    // roundtrips as its whole tree. `zed r2g --target <t>` could narrow this
    // once there is a reason to test one slice in isolation.
    interactive::confirm(
        cfg.interactive,
        &format!(
            "r2g step 4/5: install {full} into the mock consumer ({})",
            match mode {
                InstallMode::Symlink => "host symlink mode",
                InstallMode::Copy => "OCI-safe copy mode",
            }
        ),
    )?;
    install(
        &consumer_dir,
        &test_cfg,
        false,
        mode,
        Adapter::None,
        true,
        None,
        // r2g deliberately installs whatever package is under test into a
        // synthetic consumer that has no toolchain of its own, so the
        // ecosystem guard has nothing meaningful to check here.
        true,
    )?;

    // Ask the consumer manifest where install materialized the tree rather
    // than assuming the default, so this keeps working if the mock consumer
    // ever declares an [install].dir.
    let modules_dir = consumer_manifest.modules_dir();
    let target = consumer_dir
        .join(modules_dir)
        .join(&manifest.package.org)
        .join(&manifest.package.name);
    if !target.join(MANIFEST_FILE).exists() {
        bail!("installed package is missing {MANIFEST_FILE}; artifact is broken");
    }
    println!(
        "r2g: installed {full}@{} into mock consumer ({})",
        manifest.package.version,
        match mode {
            InstallMode::Symlink => "symlinked from a throwaway store",
            InstallMode::Copy => "copied, container-safe",
        }
    );

    // 7. Run the smoke test — on the host, or inside a fresh container.
    let smoke = manifest.publish.smoke_test.clone();
    interactive::confirm(
        cfg.interactive,
        &format!(
            "r2g step 5/5: run the smoke test {}",
            if opts.docker {
                "inside a fresh OCI container"
            } else {
                "on the host"
            }
        ),
    )?;
    if opts.docker {
        run_in_container(
            opts,
            &consumer_dir,
            modules_dir,
            &manifest,
            smoke.as_deref(),
        )?;
    } else {
        run_on_host(&consumer_dir, &target, smoke.as_deref())?;
    }

    // 8. Leave the workspace for inspection (r2g-style) unless asked to clean.
    if opts.clean {
        interactive::confirm(
            cfg.interactive,
            &format!("remove successful r2g workspace {}", workspace.display()),
        )?;
        fs::remove_dir_all(&workspace)?;
        if registry_target.persistent {
            println!(
                "r2g: local workspace removed; {}@{} remains published on {}",
                full, manifest.package.version, registry_target.url
            );
        }
    } else {
        println!(
            "r2g: workspace left at {} (pass --clean to remove it)",
            workspace.display()
        );
    }
    Ok(())
}

/// Copy a configured file:// registry into r2g's private registry before the
/// package under test is published. The source is treated as an immutable input:
/// no hard links or symlinks are retained, so later writes stay inside r2g.
fn snapshot_configured_file_registry(configured_registry: &str, destination: &Path) -> Result<()> {
    let Some(source) = configured_registry.strip_prefix("file://") else {
        return Ok(());
    };
    let source = PathBuf::from(source);
    if !source.exists() {
        return Ok(());
    }
    if !source.is_dir() {
        bail!(
            "configured file registry {} is not a directory",
            source.display()
        );
    }

    let source = fs::canonicalize(&source)
        .with_context(|| format!("resolving file registry {}", source.display()))?;
    let destination = resolve_path_allow_missing(destination)?;

    if destination == source || destination.starts_with(&source) || source.starts_with(&destination)
    {
        bail!(
            "r2g registry destination {} must be separate from configured registry {}",
            destination.display(),
            source.display()
        );
    }

    copy_registry_tree(&source, &destination)?;
    println!(
        "r2g: seeded dependency registry snapshot from {}",
        source.display()
    );
    Ok(())
}

fn resolve_path_allow_missing(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = absolute.as_path();
    let mut missing = Vec::new();
    while !cursor.exists() {
        missing.push(
            cursor
                .file_name()
                .context("path has no existing ancestor")?
                .to_os_string(),
        );
        cursor = cursor.parent().context("path has no existing ancestor")?;
    }
    let mut resolved = fs::canonicalize(cursor)
        .with_context(|| format!("resolving path ancestor {}", cursor.display()))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn copy_registry_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("creating registry snapshot {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("reading registry directory {}", source.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "refusing symbolic link {} in configured file registry",
                entry_path.display()
            );
        }
        if file_type.is_dir() {
            copy_registry_tree(&entry_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&entry_path, &destination_path).with_context(|| {
                format!(
                    "copying registry file {} to {}",
                    entry_path.display(),
                    destination_path.display()
                )
            })?;
        } else {
            bail!(
                "unsupported entry {} in configured file registry",
                entry_path.display()
            );
        }
    }
    Ok(())
}

/// Run `publish.smoke_test` on the host against the installed package.
fn run_on_host(consumer_dir: &Path, target: &Path, smoke: Option<&str>) -> Result<()> {
    match smoke {
        Some(command) => {
            println!("r2g: running smoke_test: {command}");
            let status = Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(consumer_dir)
                .env("ZED_PKG_TEST_TARGET", target)
                .status()
                .with_context(|| format!("running smoke_test `{command}`"))?;
            if !status.success() {
                bail!("smoke_test failed with {status}");
            }
            println!("r2g: PASS — artifact installs and its smoke_test succeeds");
        }
        None => println!(
            "r2g: PASS — artifact installs cleanly \
             (no publish.smoke_test configured; consider adding one)"
        ),
    }
    Ok(())
}

/// Run the smoke test inside a throwaway OCI container. The mock consumer
/// directory (which already holds the copy-installed package) is bind-mounted
/// in, and the smoke test runs against it with `ZED_PKG_TEST_TARGET` pointing
/// at the installed package inside the container.
fn run_in_container(
    opts: &R2gOptions,
    consumer_dir: &Path,
    modules_dir: &str,
    manifest: &Manifest,
    smoke: Option<&str>,
) -> Result<()> {
    let runtime = resolve_runtime(opts.runtime)?;
    // Canonicalize so the bind mount uses a real absolute path the runtime's
    // file sharing can resolve (macOS maps ~ and /private by default).
    let consumer_host = fs::canonicalize(consumer_dir)
        .with_context(|| format!("resolving {}", consumer_dir.display()))?;
    let args = container_args(
        &opts.image,
        &consumer_host,
        modules_dir,
        &manifest.package.org,
        &manifest.package.name,
        smoke,
    );

    println!(
        "r2g: running smoke test in a throwaway {} container ({})",
        runtime.program(),
        opts.image
    );
    match smoke {
        Some(s) => println!("r2g: smoke_test: {s}"),
        None => println!(
            "r2g: no publish.smoke_test set; checking the artifact is present in-container"
        ),
    }

    let status = Command::new(runtime.program())
        .args(&args)
        .status()
        .with_context(|| {
            format!(
                "launching `{}` (is it installed and on PATH?)",
                runtime.program()
            )
        })?;
    if !status.success() {
        bail!(
            "r2g container smoke test failed with {status} (image {})",
            opts.image
        );
    }
    println!(
        "r2g: PASS — artifact installs and passes its smoke test inside {}",
        opts.image
    );
    Ok(())
}

/// Build the `run` argument vector for docker/podman. Pure (no IO) so it can
/// be unit-tested without a container runtime present. `modules_dir` is the
/// consumer's installed-tree directory (`[install].dir`, default
/// `zed_modules`), so the in-container target path matches what install wrote.
fn container_args(
    image: &str,
    consumer_host: &Path,
    modules_dir: &str,
    org: &str,
    name: &str,
    smoke: Option<&str>,
) -> Vec<String> {
    let target_in = format!("{CONTAINER_CONSUMER}/{modules_dir}/{org}/{name}");
    // With no smoke test, still exercise the container path by proving the
    // installed artifact is present and well-formed inside the container.
    let script = smoke
        .map(str::to_string)
        .unwrap_or_else(|| format!("test -f \"$ZED_PKG_TEST_TARGET/{MANIFEST_FILE}\""));
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        format!("{}:{CONTAINER_CONSUMER}", consumer_host.display()),
        "-w".to_string(),
        CONTAINER_CONSUMER.to_string(),
        "-e".to_string(),
        format!("ZED_PKG_TEST_TARGET={target_in}"),
        image.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ]
}

/// Pick the container runtime: the explicit choice if given (and installed),
/// otherwise the first of docker/podman found on `PATH`.
fn resolve_runtime(explicit: Option<ContainerRuntime>) -> Result<ContainerRuntime> {
    if let Some(runtime) = explicit {
        if !program_on_path(runtime.program()) {
            bail!(
                "--runtime {} requested but `{}` was not found on PATH",
                runtime.program(),
                runtime.program()
            );
        }
        return Ok(runtime);
    }
    for runtime in [ContainerRuntime::Docker, ContainerRuntime::Podman] {
        if program_on_path(runtime.program()) {
            return Ok(runtime);
        }
    }
    bail!(
        "no container runtime found for --docker: install docker or podman, \
         or set --runtime / ZED_PKG_R2G_RUNTIME (drop --docker to test on the host)"
    );
}

fn program_on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths)
        .any(|dir| dir.join(program).is_file() || dir.join(format!("{program}.exe")).is_file())
}

#[cfg(test)]
mod tests {
    use zed_interfaces::paths::MODULES_DIR;

    use super::*;

    fn test_config(registry: String, home: PathBuf, token: Option<String>) -> Config {
        Config {
            registry,
            home,
            token,
            auth_url: "https://auth.example.test".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        }
    }

    #[test]
    fn isolated_registry_target_remains_private_and_credential_free() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cfg = test_config(
            "https://registry.example.test".to_string(),
            temp.path().join("home"),
            Some("secret".to_string()),
        );
        let private = temp.path().join("workspace/registry");
        let target = prepare_registry_target(&cfg, R2gRegistryMode::Isolated, &private)?;
        assert_eq!(target.url, format!("file://{}", private.display()));
        assert_eq!(target.token, None);
        assert!(!target.persistent);
        Ok(())
    }

    #[test]
    fn server_registry_target_uses_the_configured_http_endpoint_and_token() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cfg = test_config(
            "http://127.0.0.1:48080".to_string(),
            temp.path().join("home"),
            Some("zpkg_test".to_string()),
        );
        let target =
            prepare_registry_target(&cfg, R2gRegistryMode::Server, &temp.path().join("unused"))?;
        assert_eq!(target.url, "http://127.0.0.1:48080");
        assert_eq!(target.token.as_deref(), Some("zpkg_test"));
        assert!(target.persistent);
        Ok(())
    }

    #[test]
    fn server_registry_mode_rejects_file_registries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cfg = test_config(
            format!("file://{}", temp.path().join("shared").display()),
            temp.path().join("home"),
            None,
        );
        let error =
            prepare_registry_target(&cfg, R2gRegistryMode::Server, &temp.path().join("unused"))
                .expect_err("server mode must fail closed for file registries");
        assert!(error.to_string().contains("requires an HTTP(S) registry"));
        Ok(())
    }

    #[test]
    fn configured_file_registry_is_snapshotted_without_mutating_source() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source-registry");
        let package = source.join("packages/acme/widget/package.json");
        let artifact = source.join("artifacts/abc.tar.gz");
        fs::create_dir_all(package.parent().context("package parent")?)?;
        fs::create_dir_all(artifact.parent().context("artifact parent")?)?;
        fs::write(&package, br#"{"latest":"1.0.0"}"#)?;
        fs::write(&artifact, b"artifact-bytes")?;

        let destination = temp.path().join("workspace/r2g/registry");
        snapshot_configured_file_registry(&format!("file://{}", source.display()), &destination)?;

        assert_eq!(
            fs::read(destination.join("packages/acme/widget/package.json"))?,
            br#"{"latest":"1.0.0"}"#
        );
        assert_eq!(
            fs::read(destination.join("artifacts/abc.tar.gz"))?,
            b"artifact-bytes"
        );

        fs::write(
            destination.join("packages/acme/widget/package.json"),
            b"changed only in snapshot",
        )?;
        assert_eq!(fs::read(package)?, br#"{"latest":"1.0.0"}"#);
        Ok(())
    }

    #[test]
    fn non_file_registry_does_not_create_a_snapshot() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let destination = temp.path().join("registry");
        snapshot_configured_file_registry("https://registry.example.test", &destination)?;
        assert!(!destination.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn configured_file_registry_rejects_symbolic_links() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source-registry");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&source)?;
        fs::write(&outside, b"do not copy")?;
        symlink(&outside, source.join("escape"))?;

        let destination = temp.path().join("workspace/registry");
        let error = snapshot_configured_file_registry(
            &format!("file://{}", source.display()),
            &destination,
        )
        .expect_err("registry symlink should be rejected");
        assert!(error.to_string().contains("symbolic link"));
        Ok(())
    }

    #[test]
    fn configured_registry_cannot_contain_the_r2g_destination() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source-registry");
        fs::create_dir_all(&source)?;
        let destination = source.join("nested/registry");
        let error = snapshot_configured_file_registry(
            &format!("file://{}", source.display()),
            &destination,
        )
        .expect_err("nested destination should be rejected");
        assert!(error.to_string().contains("must be separate"));
        Ok(())
    }

    #[test]
    fn container_args_mount_workdir_and_target() {
        let args = container_args(
            "node:22-slim",
            Path::new("/home/u/.zed-pkg/r2g/acme-widget/consumer"),
            MODULES_DIR,
            "acme",
            "widget",
            Some("node -e \"require('@acme/widget')\""),
        );
        // The consumer is mounted, workdir is set there, and the smoke test
        // sees the installed package via ZED_PKG_TEST_TARGET.
        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.windows(2).any(|w| w[0] == "-v"
            && w[1] == format!("/home/u/.zed-pkg/r2g/acme-widget/consumer:{CONTAINER_CONSUMER}")));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-w" && w[1] == CONTAINER_CONSUMER)
        );
        assert!(args.windows(2).any(|w| w[0] == "-e"
            && w[1]
                == format!("ZED_PKG_TEST_TARGET={CONTAINER_CONSUMER}/{MODULES_DIR}/acme/widget")));
        assert_eq!(args.last().unwrap(), "node -e \"require('@acme/widget')\"");
        // Image precedes the `sh -c <script>` trailer.
        let sh = args.iter().position(|a| a == "sh").unwrap();
        assert_eq!(args[sh - 1], "node:22-slim");
        assert_eq!(args[sh + 1], "-c");
    }

    #[test]
    fn container_args_default_checks_artifact_presence() {
        let args = container_args(
            "debian:stable-slim",
            Path::new("/tmp/consumer"),
            MODULES_DIR,
            "acme",
            "widget",
            None,
        );
        assert_eq!(
            args.last().unwrap(),
            &format!("test -f \"$ZED_PKG_TEST_TARGET/{MANIFEST_FILE}\"")
        );
    }

    /// A relocated install tree ([install].dir) must be reflected in the
    /// in-container target path, or the smoke test would look in the wrong
    /// place for the package it is supposed to exercise.
    #[test]
    fn container_args_honor_a_relocated_install_dir() {
        let args = container_args(
            "debian:stable-slim",
            Path::new("/tmp/consumer"),
            ".vendor/.zed",
            "acme",
            "widget",
            None,
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-e"
                && w[1]
                    == format!(
                        "ZED_PKG_TEST_TARGET={CONTAINER_CONSUMER}/.vendor/.zed/acme/widget"
                    )),
            "relocated install dir missing from the container target: {args:?}"
        );
    }
}
