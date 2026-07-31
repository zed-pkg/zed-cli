from __future__ import annotations

from pathlib import Path


def replace_exact(path: Path, old: str, new: str, *, count: int = 1) -> None:
    text = path.read_text(encoding="utf-8")
    actual = text.count(old)
    if actual != count:
        raise RuntimeError(
            f"{path}: expected {count} occurrence(s), found {actual}: {old[:120]!r}"
        )
    path.write_text(text.replace(old, new, count), encoding="utf-8")


main_rs = Path("src/main.rs")
replace_exact(
    main_rs,
    '''fn main() {
    let args = std::env::args_os().collect();
    if let Some(result) = dev::dispatch(args) {''',
    '''fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    if let Err(error) = zed_cli::flags::normalize_global_boolean_environment(&args) {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    if let Some(result) = dev::dispatch(args) {''',
)

flags_rs = Path("src/flags.rs")
replace_exact(
    flags_rs,
    "use std::ffi::OsStr;",
    "use std::ffi::{OsStr, OsString};",
)
replace_exact(
    flags_rs,
    '''const CONTRACT: &str = include_str!("../.cli-flags.toml");

/// Audit and apply the embedded flags2env contract.''',
    '''const CONTRACT: &str = include_str!("../.cli-flags.toml");

/// Validate inherited global booleans before any modular route can short-circuit.
///
/// Root help is rendered by the modular `develop` router, before the legacy
/// command parser runs. This preflight keeps malformed deployment environment
/// values fail-closed even for `zed --help`, while preserving explicit CLI
/// precedence (`--interactive` may intentionally replace a malformed inherited
/// `ZED_PKG_INTERACTIVE`). The full flags2env audit and parse still run for
/// established commands in [`apply_cli_flags`].
pub fn normalize_global_boolean_environment(args: &[OsString]) -> Result<()> {
    let argv = args
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags2env requires UTF-8 command-line arguments")
        })
        .collect::<Result<Vec<_>>>()?;
    let explicit_envs = explicit_env_keys(&argv, &[])?;
    normalize_active_boolean_environment(&[], &explicit_envs)
}

/// Audit and apply the embedded flags2env contract.''',
)

ci = Path(".github/workflows/ci.yml")
old_block = '''      - name: Interactive checkpoints fail closed and hard exits recover
        shell: bash
        run: |
          set -euo pipefail
          container="$(docker create zed-pkg/install-test)"
          trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
          docker cp "$container:/usr/local/bin/zed" "$RUNNER_TEMP/zed"
          chmod +x "$RUNNER_TEMP/zed"
          app="$RUNNER_TEMP/fixtures/node-app"

          if printf 'yes\\n' | (
            cd "$app"
            "$RUNNER_TEMP/zed" --interactive --home "$RUNNER_TEMP/zed-home" uninstall
          ); then
            echo "redirected stdin unexpectedly satisfied --interactive" >&2
            exit 1
          fi
          test -d "$app/.vendor/.zed/zed-pkg/docker-node-lib"
          test ! -e "$app/.zpkg-staging"

          (
            cd "$app"
            python3 "$GITHUB_WORKSPACE/zed-cli/tests/interactive_pty.py" kill-after=2 -- \\
              "$RUNNER_TEMP/zed" --interactive \\
                --registry "file://$RUNNER_TEMP/registry" \\
                --home "$RUNNER_TEMP/zed-home" \\
                install --frozen --install-mode copy
          )
          test -d "$app/.zpkg-staging"

          (
            cd "$app"
            "$RUNNER_TEMP/zed" \\
              --registry "file://$RUNNER_TEMP/registry" \\
              --home "$RUNNER_TEMP/zed-home" \\
              install --frozen --install-mode copy
            python3 "$GITHUB_WORKSPACE/zed-cli/tests/interactive_pty.py" yes -- \\
              "$RUNNER_TEMP/zed" --interactive \\
                --registry "file://$RUNNER_TEMP/registry" \\
                --home "$RUNNER_TEMP/zed-home" \\
                install --frozen --install-mode copy
            node src/main.js
          )
          test ! -e "$app/.zpkg-staging"
'''
new_block = '''      - name: Interactive checkpoints fail closed and hard exits recover
        shell: bash
        run: |
          set -euo pipefail
          container="$(docker create zed-pkg/install-test)"
          trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
          docker cp "$container:/usr/local/bin/zed" "$RUNNER_TEMP/zed"
          chmod +x "$RUNNER_TEMP/zed"
          app="$RUNNER_TEMP/fixtures/node-app"
          # Docker runs as root and owns the mounted store/lock directory. Keep
          # the host-extracted binary on an independent host-owned home so this
          # test exercises transaction recovery rather than fixture permissions.
          interactive_home="$RUNNER_TEMP/interactive-zed-home"
          mkdir -p "$interactive_home"

          if printf 'yes\\n' | (
            cd "$app"
            "$RUNNER_TEMP/zed" --interactive --home "$interactive_home" uninstall
          ); then
            echo "redirected stdin unexpectedly satisfied --interactive" >&2
            exit 1
          fi
          test -d "$app/.vendor/.zed/zed-pkg/docker-node-lib"
          test ! -e "$app/.zpkg-staging"

          (
            cd "$app"
            python3 "$GITHUB_WORKSPACE/zed-cli/tests/interactive_pty.py" kill-after=2 -- \\
              "$RUNNER_TEMP/zed" --interactive \\
                --registry "file://$RUNNER_TEMP/registry" \\
                --home "$interactive_home" \\
                install --frozen --install-mode copy
          )
          test -d "$app/.zpkg-staging"

          (
            cd "$app"
            "$RUNNER_TEMP/zed" \\
              --registry "file://$RUNNER_TEMP/registry" \\
              --home "$interactive_home" \\
              install --frozen --install-mode copy
            python3 "$GITHUB_WORKSPACE/zed-cli/tests/interactive_pty.py" yes -- \\
              "$RUNNER_TEMP/zed" --interactive \\
                --registry "file://$RUNNER_TEMP/registry" \\
                --home "$interactive_home" \\
                install --frozen --install-mode copy
            node src/main.js
          )
          test ! -e "$app/.zpkg-staging"
'''
replace_exact(ci, old_block, new_block)

print("DEN-1194 source and CI fixture transformations applied")
