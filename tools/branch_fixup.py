#!/usr/bin/env python3
"""One-shot exact fixup for the role-contract branch; removed by CI after use."""

from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    actual = text.count(old)
    if actual != count:
        raise SystemExit(
            f"{path}: expected {count} occurrence(s), found {actual}: {old[:80]!r}"
        )
    target.write_text(text.replace(old, new), encoding="utf-8")


def patch_cli() -> None:
    path = Path("src/cli.rs")
    text = path.read_text(encoding="utf-8")
    old = '''        #[arg(
            long = "do-not-write-new-manifest",
            visible_aliases = ["allow-no-manifest", "skip-manifest"],
            env = "ZED_PKG_ALLOW_NO_MANIFEST"
        )]
        allow_no_manifest: bool,'''
    new = '''        #[arg(
            long = "do-not-write-new-manifest",
            visible_aliases = ["allow-no-manifest", "skip-manifest"],
            env = "ZED_PKG_ALLOW_NO_MANIFEST",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true",
            default_value = "false",
            value_parser = clap::builder::BoolishValueParser::new(),
            action = clap::ArgAction::Set
        )]
        allow_no_manifest: bool,'''
    if text.count(old) != 1:
        raise SystemExit("src/cli.rs: manifest-write flag block drifted")
    text = text.replace(old, new)

    marker = '''    use super::{AuthCmd, Cli, Cmd, EnvCmd, InstallMode, R2gRegistryMode};
'''
    helper = '''    use super::{AuthCmd, Cli, Cmd, EnvCmd, InstallMode, R2gRegistryMode};

    // These flags intentionally expose standard toolchain variables so a
    // Zed-managed environment can be projected into the native runtime.
    const STANDARD_TOOLCHAIN_ENVS: &[&str] = &[
        "CLASSPATH",
        "COMSPEC",
        "IN_NIX_SHELL",
        "NIX_BUILD_TOP",
        "PYTHONPATH",
        "XDG_CONFIG_HOME",
    ];

    fn registered_flag_env(env: &str) -> bool {
        env.starts_with("ZED_PKG_")
            || env.starts_with("ZED_TASK_")
            || STANDARD_TOOLCHAIN_ENVS.contains(&env)
    }
'''
    if text.count(marker) != 1:
        raise SystemExit("src/cli.rs: test import marker drifted")
    text = text.replace(marker, helper)

    namespace = 'env.starts_with("ZED_PKG_") || env.starts_with("ZED_TASK_")'
    if text.count(namespace) != 3:
        raise SystemExit(
            f"src/cli.rs: expected three namespace assertions, found {text.count(namespace)}"
        )
    text = text.replace(namespace, "registered_flag_env(&env)")

    test_marker = '''    #[test]
    fn init_accepts_a_project_directory_and_cli_tools_are_repeatable() {'''
    boolish_test = '''    #[test]
    fn manifest_write_switch_accepts_boolish_values() {
        for (value, expected) in [
            ("1", true),
            ("0", false),
            ("yes", true),
            ("no", false),
            ("on", true),
            ("off", false),
        ] {
            let argument = format!("--do-not-write-new-manifest={value}");
            let cli = Cli::try_parse_from([
                "zed",
                "install",
                "acme/http-kit@^1",
                argument.as_str(),
            ])
            .unwrap();
            match cli.cmd {
                Cmd::Install {
                    allow_no_manifest,
                    ..
                } => assert_eq!(allow_no_manifest, expected, "{value}"),
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn init_accepts_a_project_directory_and_cli_tools_are_repeatable() {'''
    if text.count(test_marker) != 1:
        raise SystemExit("src/cli.rs: boolish test insertion marker drifted")
    path.write_text(text.replace(test_marker, boolish_test), encoding="utf-8")


def patch_cli_flags() -> None:
    path = Path(".cli-flags.toml")
    text = path.read_text(encoding="utf-8")
    definitions = (
        ("zed-pkg-key-id", "ZED_PKG_KEY_ID", "string", None, "Publisher signing key identifier."),
        (
            "zed-pkg-mirror-bootstrap-url",
            "ZED_PKG_MIRROR_BOOTSTRAP_URL",
            "string",
            None,
            "Bootstrap URL for signed mirror discovery metadata.",
        ),
        (
            "zed-pkg-mirror-json",
            "ZED_PKG_MIRROR_JSON",
            "bool",
            "false",
            "Emit mirror command output as deterministic JSON.",
        ),
        (
            "zed-pkg-mirror-output",
            "ZED_PKG_MIRROR_OUTPUT",
            "string",
            None,
            "Output path for generated mirror configuration.",
        ),
        (
            "zed-pkg-mirror-package",
            "ZED_PKG_MIRROR_PACKAGE",
            "string",
            None,
            "Package coordinate used by mirror inspection commands.",
        ),
        (
            "zed-pkg-no-mirrors",
            "ZED_PKG_NO_MIRRORS",
            "bool",
            "false",
            "Disable mirror fallback and use configured registries only.",
        ),
        (
            "zed-pkg-trust-mirror-metadata",
            "ZED_PKG_TRUST_MIRROR_METADATA",
            "bool",
            "false",
            "Allow explicitly trusted unsigned local mirror metadata.",
        ),
    )
    additions: list[str] = []
    for flag_name, env_name, value_type, default, help_text in definitions:
        marker = f'env = "{env_name}"'
        if marker in text:
            continue
        block = [
            f"[flags.{flag_name}]",
            marker,
            f'long = "{flag_name}"',
            f'type = "{value_type}"',
        ]
        if default is not None:
            block.append(f'default = "{default}"')
        block.append(f'help = "{help_text}"')
        additions.append("\n".join(block))
    if additions:
        path.write_text(
            text.rstrip() + "\n\n" + "\n\n".join(additions) + "\n",
            encoding="utf-8",
        )


def main() -> None:
    patch_cli()
    patch_cli_flags()
    replace("src/binary_archive.rs", "use crate::registry::registry_for;\n", "")
    replace(
        "src/install_graph.rs",
        "use crate::registry::{Registry, registry_for};",
        "use crate::registry::Registry;",
    )
    replace(
        "src/install_graph/hardening_tests.rs",
        "use crate::pack::pack;",
        "use crate::pack::pack;\nuse crate::registry::registry_for;",
    )
    replace(
        "src/publisher_keys.rs",
        '''    fn a_pin_refuses_a_different_key_from_the_same_org() {
        let (stored, public) = key_pair("acme-2026");''',
        '''    fn a_pin_refuses_a_different_key_from_the_same_org() {
        let (_stored, public) = key_pair("acme-2026");''',
    )
    replace(
        "src/binary_archive/registry_io.rs",
        "    Legacy(VersionMetadata),",
        "    Legacy(Box<VersionMetadata>),",
    )
    replace(
        "src/binary_archive/registry_io.rs",
        '''        BinaryRegistryRoute::Legacy => registry
            .get_version(org, name, version)
            .map(ResolvedBinaryMetadata::Legacy),''',
        '''        BinaryRegistryRoute::Legacy => registry
            .get_version(org, name, version)
            .map(Box::new)
            .map(ResolvedBinaryMetadata::Legacy),''',
    )
    replace(
        "src/forge_publish.rs",
        "    fn upload_asset(\n",
        '''    // This private boundary mirrors the GitHub release-asset API: repository,
    // release, tag, identity, payload, media type, and immutable replacement policy.
    #[allow(clippy::too_many_arguments)]
    fn upload_asset(
''',
    )

    mirror = Path("src/mirror_cmd.rs")
    text = mirror.read_text(encoding="utf-8")
    marker = '''/// Everything that could serve this project, ambient and per-package.
fn project_mirrors(
    cwd: &Path,
    cfg: &Config,
) -> Result<(
    Vec<MirrorDescriptorV1>,
    BTreeMap<String, Vec<MirrorDescriptorV1>>,
)> {'''
    replacement = '''type ProjectMirrors = (
    Vec<MirrorDescriptorV1>,
    BTreeMap<String, Vec<MirrorDescriptorV1>>,
);

/// Everything that could serve this project, ambient and per-package.
fn project_mirrors(cwd: &Path, cfg: &Config) -> Result<ProjectMirrors> {'''
    if text.count(marker) != 1:
        raise SystemExit("src/mirror_cmd.rs: project_mirrors signature drifted")
    mirror.write_text(text.replace(marker, replacement), encoding="utf-8")


if __name__ == "__main__":
    main()
