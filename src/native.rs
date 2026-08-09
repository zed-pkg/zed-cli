use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::{NATIVE_PACKAGE_MANAGERS, NativeDependencies};

use crate::interactive;

/// One package's supported host-package-manager routes after target projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRequirement {
    pub package: String,
    pub managers: NativeDependencies,
}

impl NativeRequirement {
    pub fn new(package: impl Into<String>, managers: NativeDependencies) -> Self {
        Self {
            package: package.into(),
            managers,
        }
    }
}

/// A fixed argv invocation. Manifest data is appended only as separate args;
/// no native package spec is ever interpolated into a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeInvocation {
    pub program: OsString,
    pub args: Vec<OsString>,
}

/// Graph-wide native installation decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeInstallOutcome {
    pub manager: Option<String>,
    pub packages: Vec<String>,
    /// Zed-managed profile used by managers that can install without mutating
    /// global host state (currently Nix). Lifecycle commands receive its build
    /// paths through [`environment`].
    pub profile: Option<PathBuf>,
}

impl NativeInstallOutcome {
    pub fn packages_for(&self, dependencies: &NativeDependencies) -> Vec<String> {
        self.manager
            .as_ref()
            .and_then(|manager| dependencies.get(manager))
            .cloned()
            .unwrap_or_default()
    }
}

fn manager_program(manager: &str) -> &str {
    match manager {
        "apt" => "apt-get",
        "apk" => "apk",
        "brew" => "brew",
        "choco" => "choco",
        "dnf" => "dnf",
        "nix" => "nix",
        "pacman" => "pacman",
        "pkg" => "pkg",
        "port" => "port",
        "scoop" => "scoop",
        "winget" => "winget",
        "xbps" => "xbps-install",
        "yum" => "yum",
        "zypper" => "zypper",
        _ => manager,
    }
}

fn path_entries() -> Vec<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn executable_exists(program: &str) -> bool {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate);
    }
    path_entries().iter().any(|dir| {
        let path = dir.join(program);
        if is_executable_file(&path) {
            return true;
        }
        #[cfg(windows)]
        {
            ["exe", "cmd", "bat", "com"]
                .iter()
                .any(|ext| is_executable_file(&dir.join(format!("{program}.{ext}"))))
        }
        #[cfg(not(windows))]
        false
    })
}

fn platform_preference() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        &[
            "apt", "apk", "dnf", "yum", "pacman", "zypper", "xbps", "nix",
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &["brew", "port", "nix"]
    }
    #[cfg(target_os = "windows")]
    {
        &["winget", "choco", "scoop", "nix"]
    }
    #[cfg(all(
        not(target_os = "linux"),
        not(target_os = "macos"),
        not(target_os = "windows")
    ))]
    {
        &["pkg", "nix"]
    }
}

fn common_managers(requirements: &[NativeRequirement]) -> BTreeSet<String> {
    let mut requirements = requirements.iter().filter(|item| !item.managers.is_empty());
    let Some(first) = requirements.next() else {
        return BTreeSet::new();
    };
    let mut common: BTreeSet<String> = first.managers.keys().cloned().collect();
    for requirement in requirements {
        common.retain(|manager| requirement.managers.contains_key(manager));
    }
    common
}

fn supported_manager_summary(requirements: &[NativeRequirement]) -> String {
    requirements
        .iter()
        .filter(|item| !item.managers.is_empty())
        .map(|item| {
            let managers = item
                .managers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}: [{}]", item.package, managers)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn select_manager_impl(
    requirements: &[NativeRequirement],
    requested: Option<&str>,
    require_executable: bool,
) -> Result<Option<String>> {
    if requirements.iter().all(|item| item.managers.is_empty()) {
        return Ok(None);
    }

    let common = common_managers(requirements);
    if common.is_empty() {
        bail!(
            "native dependency requirements have no common package manager ({})",
            supported_manager_summary(requirements)
        );
    }

    if let Some(requested) = requested {
        if !NATIVE_PACKAGE_MANAGERS.contains(&requested) {
            bail!(
                "native package manager `{requested}` is unsupported; expected one of {}",
                NATIVE_PACKAGE_MANAGERS.join(", ")
            );
        }
        if !common.contains(requested) {
            bail!(
                "native package manager `{requested}` is not supported by the whole dependency graph ({})",
                supported_manager_summary(requirements)
            );
        }
        let program = manager_program(requested);
        let packages = aggregate_packages(requirements, requested);
        if require_executable && !packages.is_empty() && !executable_exists(program) {
            bail!(
                "native package manager `{requested}` was selected, but `{program}` is not on PATH"
            );
        }
        return Ok(Some(requested.to_string()));
    }

    for manager in platform_preference() {
        if common.contains(*manager) {
            let packages = aggregate_packages(requirements, manager);
            if !require_executable
                || packages.is_empty()
                || executable_exists(manager_program(manager))
            {
                return Ok(Some((*manager).to_string()));
            }
        }
    }
    for manager in &common {
        let packages = aggregate_packages(requirements, manager);
        if !require_executable || packages.is_empty() || executable_exists(manager_program(manager))
        {
            return Ok(Some(manager.clone()));
        }
    }

    bail!(
        "none of the dependency graph's compatible native package managers are available on PATH (compatible: {}; requirements: {})",
        common.into_iter().collect::<Vec<_>>().join(", "),
        supported_manager_summary(requirements)
    )
}

pub fn select_manager(
    requirements: &[NativeRequirement],
    requested: Option<&str>,
) -> Result<Option<String>> {
    select_manager_impl(requirements, requested, true)
}

pub fn aggregate_packages(requirements: &[NativeRequirement], manager: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut seen = BTreeSet::new();
    for requirement in requirements {
        if let Some(items) = requirement.managers.get(manager) {
            for package in items {
                if seen.insert(package.clone()) {
                    packages.push(package.clone());
                }
            }
        }
    }
    packages
}

const NIX_PROFILE_MARKER: &str = "packages.json";

fn nix_profile_path(home: &Path, packages: &[String]) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"zed-native-nix-profile-v1");
    for package in packages {
        hasher.update([0]);
        hasher.update(package.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    home.join("native")
        .join("nix")
        .join("v1")
        .join(&digest[..24])
        .join("profile")
}

fn prefixed(prefix: &[&str], packages: &[String]) -> Vec<OsString> {
    prefix
        .iter()
        .map(OsString::from)
        .chain(packages.iter().map(OsString::from))
        .collect()
}

pub fn invocations(
    manager: &str,
    packages: &[String],
    nix_profile: Option<&Path>,
) -> Result<Vec<NativeInvocation>> {
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    let program = OsString::from(manager_program(manager));
    let invocations = match manager {
        "apt" => vec![NativeInvocation {
            program,
            args: prefixed(
                &["install", "-y", "--no-install-recommends", "--"],
                packages,
            ),
        }],
        "apk" => vec![NativeInvocation {
            program,
            args: prefixed(&["add", "--no-cache", "--"], packages),
        }],
        "brew" => vec![NativeInvocation {
            program,
            args: prefixed(&["install"], packages),
        }],
        "choco" => vec![NativeInvocation {
            program,
            args: prefixed(&["install", "-y"], packages),
        }],
        "dnf" | "yum" => vec![NativeInvocation {
            program,
            args: prefixed(&["install", "-y"], packages),
        }],
        "nix" => {
            let profile = nix_profile.context("a Zed-managed Nix profile path is required")?;
            vec![NativeInvocation {
                program,
                args: [
                    OsString::from("--extra-experimental-features"),
                    OsString::from("nix-command flakes"),
                    OsString::from("profile"),
                    OsString::from("install"),
                    OsString::from("--profile"),
                    profile.as_os_str().to_owned(),
                ]
                .into_iter()
                .chain(packages.iter().map(|package| {
                    if package.contains('#') || package.contains(':') {
                        OsString::from(package)
                    } else {
                        OsString::from(format!("nixpkgs#{package}"))
                    }
                }))
                .collect(),
            }]
        }
        "pacman" => vec![NativeInvocation {
            program,
            args: prefixed(&["-S", "--needed", "--noconfirm", "--"], packages),
        }],
        "pkg" => vec![NativeInvocation {
            program,
            args: prefixed(&["install", "-y"], packages),
        }],
        "port" => vec![NativeInvocation {
            program,
            args: prefixed(&["install"], packages),
        }],
        "scoop" => vec![NativeInvocation {
            program,
            args: prefixed(&["install"], packages),
        }],
        "winget" => packages
            .iter()
            .map(|package| NativeInvocation {
                program: program.clone(),
                args: [
                    "install",
                    "--id",
                    package.as_str(),
                    "--exact",
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
            })
            .collect(),
        "xbps" => vec![NativeInvocation {
            program,
            args: prefixed(&["-Sy"], packages),
        }],
        "zypper" => vec![NativeInvocation {
            program,
            args: prefixed(
                &["--non-interactive", "install", "--no-recommends"],
                packages,
            ),
        }],
        _ => bail!("unsupported native package manager `{manager}`"),
    };
    Ok(invocations)
}

fn in_nix_build() -> bool {
    // `NIX_STORE` and `IN_NIX_SHELL` may also be present in an interactive
    // `nix develop`/`nix shell`, where a Zed-managed profile remains valid.
    // `NIX_BUILD_TOP` is the stdenv build-sandbox boundary in which profile
    // mutation must fail closed.
    env::var_os("NIX_BUILD_TOP").is_some_and(|value| !value.is_empty())
}

fn truthy_env(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Validate the complete graph-wide native install decision without mutating
/// the host. The installer repeats these checks immediately before execution
/// so a manager disappearing between preflight and execution still fails
/// closed, while lifecycle-hook/build consent is resolved before any mutation.
pub fn preflight(
    requirements: &[NativeRequirement],
    allow: bool,
    requested_manager: Option<&str>,
) -> Result<()> {
    if requirements.iter().all(|item| item.managers.is_empty()) {
        return Ok(());
    }

    let nix_build = in_nix_build();
    let effective_request = if nix_build {
        match requested_manager {
            Some("nix") | None => Some("nix"),
            Some(other) => {
                bail!(
                    "native package manager `{other}` cannot be executed inside a Nix build; declare a `[native-dependencies].nix` route and select `nix`"
                )
            }
        }
    } else {
        requested_manager
    };
    let manager = select_manager_impl(requirements, effective_request, !nix_build)?
        .context("native dependency graph unexpectedly selected no manager")?;
    let packages = aggregate_packages(requirements, &manager);
    if packages.is_empty() {
        return Ok(());
    }

    if !allow {
        bail!(
            "the resolved package graph declares native dependencies; re-run with --allow-native-deps or ZED_PKG_ALLOW_NATIVE_DEPS=1 ({})",
            supported_manager_summary(requirements)
        );
    }

    if nix_build && !truthy_env("ZED_PKG_NATIVE_DEPS_PROVIDED") {
        bail!(
            "native dependencies are declared for `{manager}` inside a Nix build; place them in nativeBuildInputs/buildInputs and set ZED_PKG_NATIVE_DEPS_PROVIDED=1 instead of mutating a Nix profile ({})",
            packages.join(", ")
        );
    }

    Ok(())
}

pub fn install(
    requirements: &[NativeRequirement],
    allow: bool,
    requested_manager: Option<&str>,
    interactive_mode: bool,
    home: &Path,
) -> Result<NativeInstallOutcome> {
    if requirements.iter().all(|item| item.managers.is_empty()) {
        return Ok(NativeInstallOutcome::default());
    }

    let nix_build = in_nix_build();
    let effective_request = if nix_build {
        match requested_manager {
            Some("nix") | None => Some("nix"),
            Some(other) => {
                bail!(
                    "native package manager `{other}` cannot be executed inside a Nix build; declare a `[native-dependencies].nix` route and select `nix`"
                )
            }
        }
    } else {
        requested_manager
    };
    let manager = select_manager_impl(requirements, effective_request, !nix_build)?
        .context("native dependency graph unexpectedly selected no manager")?;
    let packages = aggregate_packages(requirements, &manager);
    if packages.is_empty() {
        return Ok(NativeInstallOutcome {
            manager: Some(manager),
            packages,
            profile: None,
        });
    }

    if !allow {
        bail!(
            "the resolved package graph declares native dependencies; re-run with --allow-native-deps or ZED_PKG_ALLOW_NATIVE_DEPS=1 ({})",
            supported_manager_summary(requirements)
        );
    }

    if nix_build {
        if truthy_env("ZED_PKG_NATIVE_DEPS_PROVIDED") {
            println!(
                "validated {} native prerequisite(s) for `{manager}`; Nix derivation declared ZED_PKG_NATIVE_DEPS_PROVIDED=1",
                packages.len()
            );
            return Ok(NativeInstallOutcome {
                manager: Some(manager),
                packages,
                profile: None,
            });
        }
        bail!(
            "native dependencies are declared for `{manager}` inside a Nix build; place them in nativeBuildInputs/buildInputs and set ZED_PKG_NATIVE_DEPS_PROVIDED=1 instead of mutating a Nix profile ({})",
            packages.join(", ")
        );
    }

    let profile = if manager == "nix" {
        let profile = nix_profile_path(home, &packages);
        let root = profile.parent().context("Nix profile has a parent")?;
        fs::create_dir_all(root)?;
        let marker = root.join(NIX_PROFILE_MARKER);
        let expected = serde_json::to_vec(&packages)?;
        if profile.exists() {
            let actual = fs::read(&marker).with_context(|| {
                format!(
                    "Zed-managed Nix profile {} exists without a readable package marker; remove {} and retry",
                    profile.display(),
                    root.display()
                )
            })?;
            if actual != expected {
                bail!(
                    "Zed-managed Nix profile marker does not match its package identity; remove {} and retry",
                    root.display()
                );
            }
            println!(
                "reusing Zed-managed Nix profile for {} native package(s): {}",
                packages.len(),
                profile.display()
            );
            return Ok(NativeInstallOutcome {
                manager: Some(manager),
                packages,
                profile: Some(profile),
            });
        }
        Some(profile)
    } else {
        None
    };

    interactive::confirm(
        interactive_mode,
        &format!(
            "install {} native package(s) with {manager}: {}",
            packages.len(),
            packages.join(", ")
        ),
    )?;

    for invocation in invocations(&manager, &packages, profile.as_deref())? {
        let mut command = Command::new(&invocation.program);
        command.args(&invocation.args);
        if manager == "apt" {
            command.env("DEBIAN_FRONTEND", "noninteractive");
        }
        let display_program = invocation.program.to_string_lossy();
        let status = match command.status() {
            Ok(status) => status,
            Err(error) => {
                if let Some(profile) = &profile
                    && let Some(root) = profile.parent()
                {
                    let _ = fs::remove_dir_all(root);
                }
                return Err(error).with_context(|| {
                    format!("running native package manager `{display_program}`")
                });
            }
        };
        if !status.success() {
            if let Some(profile) = &profile
                && let Some(root) = profile.parent()
            {
                let _ = fs::remove_dir_all(root);
            }
            bail!(
                "native package manager `{display_program}` failed with {status}; no Zed packages were materialized"
            );
        }
    }

    if let Some(profile) = &profile {
        let root = profile.parent().context("Nix profile has a parent")?;
        let marker = root.join(NIX_PROFILE_MARKER);
        let temporary_marker =
            root.join(format!(".{NIX_PROFILE_MARKER}.{}.tmp", std::process::id()));
        fs::write(&temporary_marker, serde_json::to_vec(&packages)?)?;
        fs::rename(&temporary_marker, &marker).with_context(|| {
            format!(
                "committing Zed-managed Nix profile marker {}",
                marker.display()
            )
        })?;
    }
    println!(
        "installed {} native package(s) with {manager}",
        packages.len()
    );
    Ok(NativeInstallOutcome {
        manager: Some(manager),
        packages,
        profile,
    })
}

fn prepend_command_paths(command: &mut Command, name: &str, additions: &[PathBuf]) -> Result<()> {
    let mut paths = additions.to_vec();
    if let Some(existing) = env::var_os(name) {
        paths.extend(env::split_paths(&existing));
    }
    command.env(
        name,
        env::join_paths(paths).with_context(|| format!("constructing {name}"))?,
    );
    Ok(())
}

pub fn environment(
    command: &mut Command,
    outcome: &NativeInstallOutcome,
    dependencies: &NativeDependencies,
) -> Result<()> {
    let packages = outcome.packages_for(dependencies);
    if let Some(manager) = &outcome.manager {
        command.env("ZED_NATIVE_MANAGER", manager);
    }
    command.env(
        "ZED_NATIVE_PACKAGES",
        serde_json::to_string(&packages).unwrap_or_else(|_| "[]".to_string()),
    );
    if let Some(profile) = &outcome.profile {
        command.env("ZED_NATIVE_PROFILE", profile);
        prepend_command_paths(
            command,
            "PKG_CONFIG_PATH",
            &[
                profile.join("lib/pkgconfig"),
                profile.join("share/pkgconfig"),
            ],
        )?;
        prepend_command_paths(command, "CMAKE_PREFIX_PATH", std::slice::from_ref(profile))?;
        prepend_command_paths(command, "CPATH", &[profile.join("include")])?;
        prepend_command_paths(command, "LIBRARY_PATH", &[profile.join("lib")])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    use super::*;

    fn deps(entries: &[(&str, &[&str])]) -> NativeDependencies {
        entries
            .iter()
            .map(|(manager, packages)| {
                (
                    (*manager).to_string(),
                    packages
                        .iter()
                        .map(|package| (*package).to_string())
                        .collect(),
                )
            })
            .collect::<BTreeMap<_, _>>()
    }

    #[test]
    fn common_manager_is_an_intersection_not_a_union() {
        let requirements = vec![
            NativeRequirement::new("acme/a", deps(&[("apt", &["a-dev"]), ("brew", &["a"])])),
            NativeRequirement::new("acme/b", deps(&[("apt", &["b-dev"]), ("apk", &["b-dev"])])),
        ];
        assert_eq!(
            common_managers(&requirements),
            BTreeSet::from(["apt".to_string()])
        );
    }

    #[test]
    fn aggregate_preserves_graph_order_and_deduplicates() {
        let requirements = vec![
            NativeRequirement::new("acme/a", deps(&[("apt", &["pkg-config", "ssl"])])),
            NativeRequirement::new("acme/b", deps(&[("apt", &["ssl", "zlib"])])),
        ];
        assert_eq!(
            aggregate_packages(&requirements, "apt"),
            vec!["pkg-config", "ssl", "zlib"]
        );
    }

    #[test]
    fn apt_plan_uses_separate_args_and_option_terminator() {
        let plan = invocations("apt", &["libssl-dev=3.0".to_string()], None).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].program, OsStr::new("apt-get"));
        assert_eq!(
            plan[0].args,
            [
                "install",
                "-y",
                "--no-install-recommends",
                "--",
                "libssl-dev=3.0"
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn winget_uses_one_exact_invocation_per_package() {
        let plan = invocations(
            "winget",
            &["Git.Git".to_string(), "LLVM.LLVM".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan[0].args.iter().any(|arg| arg == OsStr::new("Git.Git")));
        assert!(
            plan[1]
                .args
                .iter()
                .any(|arg| arg == OsStr::new("LLVM.LLVM"))
        );
    }

    #[test]
    fn nix_specs_are_qualified_without_touching_explicit_flake_refs() {
        let profile = Path::new("/tmp/zed-native-profile");
        let plan = invocations(
            "nix",
            &["openssl".to_string(), "github:acme/tools#clang".to_string()],
            Some(profile),
        )
        .unwrap();
        let args: Vec<String> = plan[0]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"nixpkgs#openssl".to_string()));
        assert!(args.contains(&"github:acme/tools#clang".to_string()));
        assert!(args.contains(&"--profile".to_string()));
        assert!(args.contains(&profile.display().to_string()));
    }

    #[test]
    fn nix_profile_identity_and_environment_are_zed_managed() {
        let first = nix_profile_path(
            Path::new("/zed-home"),
            &["pkg-config".to_string(), "openssl".to_string()],
        );
        let same = nix_profile_path(
            Path::new("/zed-home"),
            &["pkg-config".to_string(), "openssl".to_string()],
        );
        let reordered = nix_profile_path(
            Path::new("/zed-home"),
            &["openssl".to_string(), "pkg-config".to_string()],
        );
        assert_eq!(first, same);
        assert_ne!(first, reordered);
        assert!(first.starts_with("/zed-home/native/nix/v1"));

        let outcome = NativeInstallOutcome {
            manager: Some("nix".to_string()),
            packages: vec!["pkg-config".to_string(), "openssl".to_string()],
            profile: Some(first.clone()),
        };
        let mut command = Command::new("true");
        environment(&mut command, &outcome, &deps(&[("nix", &["openssl"])])).unwrap();
        let pkg_config_path = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("PKG_CONFIG_PATH"))
            .and_then(|(_, value)| value)
            .expect("pkg-config paths are injected for a managed Nix profile");
        let entries: Vec<PathBuf> = std::env::split_paths(pkg_config_path).collect();
        assert_eq!(entries.first(), Some(&first.join("lib/pkgconfig")));
        let native_profile = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("ZED_NATIVE_PROFILE"))
            .and_then(|(_, value)| value)
            .expect("managed profile path is exposed");
        assert_eq!(native_profile, first.as_os_str());
        let native_packages = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("ZED_NATIVE_PACKAGES"))
            .and_then(|(_, value)| value)
            .expect("native package metadata is exposed");
        assert_eq!(native_packages, OsStr::new("[\"openssl\"]"));
    }

    #[test]
    fn incompatible_graph_has_actionable_error() {
        let requirements = vec![
            NativeRequirement::new("acme/a", deps(&[("apt", &["a"])])),
            NativeRequirement::new("acme/b", deps(&[("brew", &["b"])])),
        ];
        let error = select_manager(&requirements, None).unwrap_err().to_string();
        assert!(error.contains("no common package manager"), "{error}");
        assert!(error.contains("acme/a"), "{error}");
        assert!(error.contains("acme/b"), "{error}");
    }

    #[test]
    fn nix_selection_can_validate_without_a_host_nix_executable() {
        let requirements = vec![NativeRequirement::new(
            "acme/a",
            deps(&[("nix", &["pkg-config", "openssl"])]),
        )];
        assert_eq!(
            select_manager_impl(&requirements, Some("nix"), false).unwrap(),
            Some("nix".to_string())
        );
    }

    #[test]
    fn an_empty_selected_route_needs_no_executable_or_consent() {
        let requirements = vec![NativeRequirement::new("acme/a", deps(&[("apt", &[])]))];
        assert_eq!(
            select_manager_impl(&requirements, Some("apt"), true).unwrap(),
            Some("apt".to_string())
        );
        let outcome = install(
            &requirements,
            false,
            Some("apt"),
            false,
            Path::new("/tmp/zed-native-empty"),
        )
        .unwrap();
        assert_eq!(outcome.manager.as_deref(), Some("apt"));
        assert!(outcome.packages.is_empty());
    }

    #[test]
    fn empty_declarations_need_no_manager() {
        assert_eq!(select_manager(&[], None).unwrap(), None);
        assert_eq!(invocations("apt", &[], None).unwrap(), Vec::new());
    }
}
