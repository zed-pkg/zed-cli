#!/usr/bin/env python3
"""Apply the DEN-567 frozen manifestless lock-only fix exactly once."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    content = target.read_text(encoding="utf-8")
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    target.write_text(content.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/ops.rs",
    '''#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    install_locked(
        project,
        cfg,
        &store,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
    )
}
''',
    '''#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    install_with_frozen_policy(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
        true,
    )
}

/// Restore every package pinned by an existing lockfile when there is no
/// persistent consumer manifest. The lock still verifies package identities,
/// versions, registry metadata, and artifact hashes; only manifest-drift
/// validation is inapplicable because no manifest exists to compare against.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_frozen_lock_only(
    project: &Path,
    cfg: &Config,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    install_with_frozen_policy(
        project,
        cfg,
        true,
        mode,
        adapter,
        allow_build,
        target,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn install_with_frozen_policy(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    validate_manifest_requirements: bool,
) -> Result<InstallOutcome> {
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    install_locked(
        project,
        cfg,
        &store,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
        validate_manifest_requirements,
    )
}
''',
)

replace_once(
    "src/ops.rs",
    '''/// Install body, called with the store lock already held. Split out so the
/// build-hook path can install `[build-dependencies]` into a staging dir
/// under the same lock without deadlocking on a re-acquire.
#[allow(clippy::too_many_arguments)]
fn install_locked(
    project: &Path,
    cfg: &Config,
    store: &Store,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
''',
    '''fn validate_frozen_manifest_requirements(
    manifest: &Manifest,
    lock: &Lockfile,
    workspace: Option<&WorkspaceInfo>,
    enforce: bool,
) -> Result<()> {
    if !enforce {
        return Ok(());
    }
    for (key, req_str) in &manifest.dependencies {
        let (org, name) = split_key(key)?;
        if workspace.is_some_and(|ws| ws.members.contains_key(key)) {
            continue;
        }
        let entry = lock
            .find(&org, &name)
            .with_context(|| format!("--frozen: `{key}` is not in {LOCKFILE_FILE}"))?;
        let req = Requirement::parse(req_str);
        if !req.matches(&entry.version) {
            bail!(
                "--frozen: lockfile pins {key}@{} which no longer satisfies `{req_str}`",
                entry.version
            );
        }
    }
    Ok(())
}

/// Install body, called with the store lock already held. Split out so the
/// build-hook path can install `[build-dependencies]` into a staging dir
/// under the same lock without deadlocking on a re-acquire.
#[allow(clippy::too_many_arguments)]
fn install_locked(
    project: &Path,
    cfg: &Config,
    store: &Store,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    validate_manifest_requirements: bool,
) -> Result<InstallOutcome> {
''',
)

replace_once(
    "src/ops.rs",
    '''        let lock = Lockfile::parse(&text)?;
        for (key, req_str) in &manifest.dependencies {
            let (org, name) = split_key(key)?;
            if workspace
                .as_ref()
                .is_some_and(|ws| ws.members.contains_key(key))
            {
                continue;
            }
            let entry = lock
                .find(&org, &name)
                .with_context(|| format!("--frozen: `{key}` is not in {LOCKFILE_FILE}"))?;
            let req = Requirement::parse(req_str);
            if !req.matches(&entry.version) {
                bail!(
                    "--frozen: lockfile pins {key}@{} which no longer satisfies `{req_str}`",
                    entry.version
                );
            }
        }
''',
    '''        let lock = Lockfile::parse(&text)?;
        validate_frozen_manifest_requirements(
            &manifest,
            &lock,
            workspace.as_ref(),
            validate_manifest_requirements,
        )?;
''',
)

replace_once(
    "src/ops.rs",
    '''            // Build dependencies are toolchain, not the consumer's language:
            // take them whole rather than slicing them to a target.
            None,
        )?;
''',
    '''            // Build dependencies are toolchain, not the consumer's language:
            // take them whole rather than slicing them to a target.
            None,
            true,
        )?;
''',
)

replace_once(
    "src/ops.rs",
    '''    #[test]
    fn split_key_accepts_org_name_and_keeps_nested_slashes_in_name() {
''',
    '''    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "consumer"
name = "app"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/consumer/app"

[dependencies]
"acme/http-kit" = "^1"
"#,
        )
        .unwrap();
        let empty_lock = Lockfile::default();

        let enforced = validate_frozen_manifest_requirements(
            &manifest,
            &empty_lock,
            None,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(enforced.contains("acme/http-kit"));
        assert!(
            validate_frozen_manifest_requirements(
                &manifest,
                &empty_lock,
                None,
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn split_key_accepts_org_name_and_keeps_nested_slashes_in_name() {
''',
)

replace_once(
    "src/manifestless.rs",
    '''    // Consent intentionally precedes registry access. An unversioned package
    // can be shown as “latest compatible release” without contacting a server.
    confirm_manifestless(&plan, allow_no_manifest)?;
    let dependencies = match requested {
''',
    '''    // Consent intentionally precedes registry access. An unversioned package
    // can be shown as “latest compatible release” without contacting a server.
    confirm_manifestless(&plan, allow_no_manifest)?;
    let lock_only = matches!(&requested, RequestedDependencies::Locked(_));
    let dependencies = match requested {
''',
)

replace_once(
    "src/manifestless.rs",
    '''    config::with_manifest_override(&selection.root, manifest_text, || {
        ops::install(
            &selection.root,
            cfg,
            frozen,
            mode,
            adapter,
            allow_build,
            inferred_target.as_deref(),
        )
    })
''',
    '''    config::with_manifest_override(&selection.root, manifest_text, || {
        if lock_only {
            ops::install_frozen_lock_only(
                &selection.root,
                cfg,
                mode,
                adapter,
                allow_build,
                inferred_target.as_deref(),
            )
        } else {
            ops::install(
                &selection.root,
                cfg,
                frozen,
                mode,
                adapter,
                allow_build,
                inferred_target.as_deref(),
            )
        }
    })
''',
)

print("DEN-567 frozen lock-only fix applied")
