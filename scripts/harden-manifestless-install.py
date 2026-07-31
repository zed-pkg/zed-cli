#!/usr/bin/env python3
"""Tighten the manifestless implementation to the DEN-564 acceptance gate."""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    source = file.read_text(encoding="utf-8")
    count = source.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match in {path}, found {count}")
    file.write_text(source.replace(old, new, 1), encoding="utf-8")


MANIFESTLESS = r'''//! Manifestless dependency installation.
//!
//! A missing `.zpkg.toml` is an explicit consent boundary, not a separate
//! resolver. This module builds a scoped in-memory consumer manifest and then
//! delegates to the normal installer so target inference, adapter behavior,
//! integrity checks, locking, the global store, and build consent cannot drift.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::version;

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};
use crate::ops;
use crate::registry::registry_for;

#[derive(Debug, Clone)]
struct InstallPlan {
    root: PathBuf,
    target: Option<String>,
    adapter: Adapter,
    package_specs: Vec<String>,
    source: String,
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_no_manifest: bool,
) -> Result<ops::InstallOutcome> {
    if project.join(MANIFEST_FILE).is_file() {
        if !specs.is_empty() {
            bail!(
                "package specs on `zed install` are only for folders without {MANIFEST_FILE}; use `zed add <org>/<name>[@requirement]` to update this manifest"
            );
        }
        return ops::install(
            project,
            cfg,
            frozen,
            mode,
            adapter,
            allow_build,
            target,
        );
    }

    let requested = if specs.is_empty() {
        if !frozen {
            bail!(
                "no {MANIFEST_FILE} and no package specs were provided; pass package specs, or use `zed install --frozen --allow-no-manifest` with an existing {LOCKFILE_FILE}"
            );
        }
        RequestedDependencies::Locked(dependencies_from_lock(project)?)
    } else {
        if frozen {
            bail!(
                "--frozen cannot be combined with package specs when no {MANIFEST_FILE} exists; install the specs first, then use --frozen for locked reinstalls"
            );
        }
        RequestedDependencies::Specs(parse_requested_specs(specs)?)
    };

    let inferred_target = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| ops::detect_target(project));
    let inferred_adapter = infer_adapter(project, inferred_target.as_deref(), adapter);
    let effective_adapter = match adapter {
        Adapter::Auto if inferred_adapter == Adapter::None => Adapter::Auto,
        Adapter::Auto => inferred_adapter,
        explicit => explicit,
    };
    let plan = InstallPlan {
        root: fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf()),
        target: inferred_target,
        adapter: inferred_adapter,
        package_specs: requested.display_specs(),
        source: requested.source().to_string(),
    };

    confirm_manifestless(&plan, allow_no_manifest)?;
    let dependencies = match requested {
        RequestedDependencies::Locked(dependencies) => dependencies,
        RequestedDependencies::Specs(specs) => resolve_specs(cfg, &specs)?,
    };
    let manifest_text = synthetic_manifest(&dependencies)?;
    Manifest::parse(&manifest_text).context("building the in-memory manifestless install plan")?;

    eprintln!(
        "manifestless install: no {MANIFEST_FILE} will be written; {LOCKFILE_FILE} and the inferred dependency layout will be materialized by the normal installer"
    );

    config::with_manifest_override(project, manifest_text, || {
        ops::install(
            project,
            cfg,
            frozen,
            mode,
            effective_adapter,
            allow_build,
            target,
        )
    })
}

#[derive(Debug)]
enum RequestedDependencies {
    Locked(BTreeMap<String, String>),
    Specs(BTreeMap<String, Option<String>>),
}

impl RequestedDependencies {
    fn source(&self) -> &'static str {
        match self {
            Self::Locked(_) => LOCKFILE_FILE,
            Self::Specs(_) => "the command line",
        }
    }

    fn display_specs(&self) -> Vec<String> {
        match self {
            Self::Locked(dependencies) => dependencies
                .iter()
                .map(|(key, requirement)| format!("{key}@{requirement}"))
                .collect(),
            Self::Specs(specs) => specs
                .iter()
                .map(|(key, requirement)| match requirement {
                    Some(requirement) => format!("{key}@{requirement}"),
                    None => format!("{key} (latest compatible release)"),
                })
                .collect(),
        }
    }
}

fn parse_requested_specs(specs: &[String]) -> Result<BTreeMap<String, Option<String>>> {
    let mut requested = BTreeMap::new();
    for spec in specs {
        let (key, requirement) = split_dependency_spec(spec)?;
        if let Some(previous) = requested.insert(key.clone(), requirement.clone())
            && previous != requirement
        {
            bail!(
                "conflicting requirements for {key}: `{}` and `{}`",
                display_requirement(previous.as_deref()),
                display_requirement(requirement.as_deref())
            );
        }
    }
    Ok(requested)
}

fn resolve_specs(
    cfg: &Config,
    specs: &BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let registry = if specs.values().any(Option::is_none) {
        Some(registry_for(&cfg.registry)?)
    } else {
        None
    };
    let mut dependencies = BTreeMap::new();
    for (key, explicit) in specs {
        let requirement = match explicit {
            Some(requirement) => requirement.clone(),
            None => {
                let (org, name) = ops::split_key(key)?;
                let package = registry
                    .as_ref()
                    .expect("registry exists when an unversioned spec exists")
                    .get_package(&org, &name)?;
                let latest = package
                    .latest
                    .with_context(|| format!("{key} has no published versions"))?;
                if version::parse_version(&latest).is_some() {
                    format!("^{latest}")
                } else {
                    latest
                }
            }
        };
        dependencies.insert(key.clone(), requirement);
    }
    Ok(dependencies)
}

fn split_dependency_spec(spec: &str) -> Result<(String, Option<String>)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("dependency spec cannot be empty");
    }
    let (key, requirement) = match trimmed.rsplit_once('@') {
        Some((key, requirement)) if key.contains('/') => {
            if requirement.trim().is_empty() {
                bail!("empty requirement for {key}");
            }
            (key, Some(requirement.trim().to_string()))
        }
        _ => (trimmed, None),
    };
    let (org, name) = ops::split_key(key)?;
    Ok((format!("{org}/{name}"), requirement))
}

fn display_requirement(requirement: Option<&str>) -> &str {
    requirement.unwrap_or("latest")
}

fn dependencies_from_lock(project: &Path) -> Result<BTreeMap<String, String>> {
    let path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "--frozen manifestless install requires an existing {LOCKFILE_FILE} in {}",
            project.display()
        )
    })?;
    let lock = Lockfile::parse(&text).with_context(|| format!("invalid {}", path.display()))?;
    if lock.packages.is_empty() {
        bail!("{LOCKFILE_FILE} contains no packages to install");
    }
    Ok(lock
        .packages
        .iter()
        .map(|package| {
            (
                format!("{}/{}", package.org, package.name),
                package.version.clone(),
            )
        })
        .collect())
}

fn synthetic_manifest(dependencies: &BTreeMap<String, String>) -> Result<String> {
    let mut repository = toml::map::Map::new();
    repository.insert("vcs".to_string(), toml::Value::String("git".to_string()));
    repository.insert(
        "url".to_string(),
        toml::Value::String("https://example.invalid/manifestless/consumer".to_string()),
    );

    let mut package = toml::map::Map::new();
    package.insert(
        "org".to_string(),
        toml::Value::String("manifestless".to_string()),
    );
    package.insert(
        "name".to_string(),
        toml::Value::String("consumer".to_string()),
    );
    package.insert(
        "version".to_string(),
        toml::Value::String("0.0.0".to_string()),
    );
    package.insert("repository".to_string(), toml::Value::Table(repository));

    let dependency_table: toml::map::Map<String, toml::Value> = dependencies
        .iter()
        .map(|(key, requirement)| {
            (
                key.clone(),
                toml::Value::String(requirement.clone()),
            )
        })
        .collect();

    let mut root = toml::map::Map::new();
    root.insert("package".to_string(), toml::Value::Table(package));
    root.insert(
        "dependencies".to_string(),
        toml::Value::Table(dependency_table),
    );
    Ok(toml::to_string_pretty(&toml::Value::Table(root))?)
}

fn infer_adapter(project: &Path, target: Option<&str>, requested: Adapter) -> Adapter {
    match requested {
        Adapter::Auto => match target {
            Some("node") => Adapter::Node,
            Some("java") => Adapter::Java,
            _ => ops::detect_adapter(project),
        },
        explicit => explicit,
    }
}

fn adapter_label(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Auto => "auto",
        Adapter::None => "none (universal zed_modules layout; package-declared adapters still apply)",
        Adapter::Node => "node (also node_modules/@<org>/<name>)",
        Adapter::Java => "java (also .zed/classpath for installed jars)",
    }
}

fn confirm_manifestless(plan: &InstallPlan, allow_no_manifest: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_manifestless_with(
        plan,
        stdin.is_terminal(),
        allow_no_manifest,
        &mut input,
        &mut output,
    )
}

fn confirm_manifestless_with<R: BufRead, W: Write>(
    plan: &InstallPlan,
    interactive: bool,
    allow_no_manifest: bool,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    writeln!(output, "No {MANIFEST_FILE} was found.")?;
    writeln!(output, "Install root: {}", plan.root.display())?;
    writeln!(
        output,
        "Language target: {}",
        plan.target
            .as_deref()
            .unwrap_or("whole artifact (no language marker inferred)")
    )?;
    writeln!(output, "Adapter: {}", adapter_label(plan.adapter))?;
    writeln!(
        output,
        "Universal dependency tree: {}",
        plan.root.join(MODULES_DIR).display()
    )?;
    writeln!(output, "Packages from {}:", plan.source)?;
    for spec in &plan.package_specs {
        writeln!(output, "  - {spec}")?;
    }
    writeln!(
        output,
        "Zed will write {LOCKFILE_FILE} and dependency outputs, but will not create {MANIFEST_FILE}."
    )?;

    if allow_no_manifest {
        writeln!(
            output,
            "Proceeding non-interactively because --allow-no-manifest/--skip-manifest was supplied."
        )?;
        return Ok(());
    }
    if !interactive {
        bail!(
            "stdin is not interactive; no files were changed. Re-run with --allow-no-manifest or --skip-manifest after reviewing the plan"
        );
    }

    write!(output, "Continue? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        bail!(
            "confirmation input ended before `y`/`yes`; no files were changed. Re-run with --allow-no-manifest or --skip-manifest after reviewing the plan"
        );
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }
    bail!(
        "manifestless install cancelled; no files were changed. Re-run and answer `y`, or use --allow-no-manifest/--skip-manifest"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn plan(root: &Path) -> InstallPlan {
        InstallPlan {
            root: root.to_path_buf(),
            target: Some("node".to_string()),
            adapter: Adapter::Node,
            package_specs: vec!["acme/http-kit@^1".to_string()],
            source: "the command line".to_string(),
        }
    }

    #[test]
    fn transient_requirement_parser_preserves_explicit_and_latest_specs() {
        assert_eq!(
            split_dependency_spec("acme/http-kit@^1.2").unwrap(),
            ("acme/http-kit".to_string(), Some("^1.2".to_string()))
        );
        assert_eq!(
            split_dependency_spec("acme/http-kit").unwrap(),
            ("acme/http-kit".to_string(), None)
        );
    }

    #[test]
    fn synthetic_manifest_uses_real_dependency_keys_without_touching_disk() {
        let project = tempfile::tempdir().unwrap();
        let dependencies = BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]);
        let text = synthetic_manifest(&dependencies).unwrap();
        let manifest = Manifest::parse(&text).unwrap();
        assert_eq!(
            manifest.dependencies.get("acme/http-kit").map(String::as_str),
            Some("^1")
        );
        assert!(!project.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn project_markers_and_common_structure_select_language_and_adapter() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("package.json"), "{}").unwrap();
        assert_eq!(ops::detect_target(project.path()).as_deref(), Some("node"));
        assert_eq!(ops::detect_adapter(project.path()), Adapter::Node);

        fs::remove_file(project.path().join("package.json")).unwrap();
        fs::create_dir_all(project.path().join("src")).unwrap();
        fs::write(project.path().join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(ops::detect_target(project.path()).as_deref(), Some("rust"));
        assert_eq!(ops::detect_adapter(project.path()), Adapter::None);
    }

    #[test]
    fn interactive_confirmation_prints_the_exact_plan_and_accepts_yes() {
        let root = Path::new("/tmp/example");
        let mut output = Vec::new();
        confirm_manifestless_with(
            &plan(root),
            true,
            false,
            &mut Cursor::new(b"yes\n"),
            &mut output,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Install root: /tmp/example"));
        assert!(output.contains("Language target: node"));
        assert!(output.contains("Adapter: node"));
        assert!(output.contains("acme/http-kit@^1"));
        assert!(output.contains("Continue?"));
    }

    #[test]
    fn negative_eof_and_noninteractive_input_fail_closed() {
        let plan = plan(Path::new("/tmp/example"));
        let negative = confirm_manifestless_with(
            &plan,
            true,
            false,
            &mut Cursor::new(b"no\n"),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(negative.contains("cancelled"));

        let eof = confirm_manifestless_with(
            &plan,
            true,
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(eof.contains("input ended"));
        assert!(eof.contains("--skip-manifest"));

        let redirected = confirm_manifestless_with(
            &plan,
            false,
            false,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(redirected.contains("not interactive"));
        assert!(redirected.contains("--allow-no-manifest"));
    }

    #[test]
    fn explicit_bypass_never_reads_confirmation_input() {
        confirm_manifestless_with(
            &plan(Path::new("/tmp/example")),
            false,
            true,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap();
    }
}
'''

Path("src/manifestless.rs").write_text(MANIFESTLESS, encoding="utf-8")

replace_once(
    "src/ops.rs",
    '''        ("package.json", "node"),
        ("Cargo.toml", "rust"),''',
    '''        ("package.json", "node"),
        ("tsconfig.json", "node"),
        ("Cargo.toml", "rust"),''',
    "recognize TypeScript project manifests",
)
replace_once(
    "src/ops.rs",
    '''fn detect_target(project: &Path) -> Option<String> {''',
    '''pub(crate) fn detect_target(project: &Path) -> Option<String> {''',
    "expose target inference to the consent plan",
)
replace_once(
    "src/ops.rs",
    '''    MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}''',
    '''    if let Some((_, target)) = MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
    {
        return Some((*target).to_string());
    }

    // A consumer folder may be intentionally pre-manifest (for example a
    // generated app skeleton). Keep the fallback shallow and deterministic so
    // a large unrelated checkout is never recursively scanned.
    const STRUCTURE_MARKERS: &[(&str, &str)] = &[
        ("src/main.rs", "rust"),
        ("src/lib.rs", "rust"),
        ("src/index.ts", "node"),
        ("src/main.ts", "node"),
        ("src/index.js", "node"),
        ("src/main.js", "node"),
        ("main.go", "go"),
        ("cmd/main.go", "go"),
        ("main.py", "python"),
        ("app.py", "python"),
        ("src/main.py", "python"),
        ("lib/main.dart", "dart"),
        ("src/main.gleam", "gleam"),
        ("src/main/java", "java"),
        ("src/main/kotlin", "java"),
    ];
    STRUCTURE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}''',
    "add bounded folder-structure inference",
)
replace_once(
    "src/ops.rs",
    '''fn detect_adapter(project: &Path) -> Adapter {
    if project.join("package.json").exists() {
        Adapter::Node
    } else if project.join("pom.xml").exists()
        || project.join("build.gradle").exists()
        || project.join("build.gradle.kts").exists()
    {
        Adapter::Java
    } else {
        Adapter::None
    }
}''',
    '''pub(crate) fn detect_adapter(project: &Path) -> Adapter {
    match detect_target(project).as_deref() {
        Some("node") => Adapter::Node,
        Some("java") => Adapter::Java,
        _ => Adapter::None,
    }
}''',
    "share language-aware adapter inference",
)

replace_once(
    "src/cli.rs",
    '''        /// Package specs to install (`org/name[@requirement]`). With a manifest
        /// they are added before installation; without one they form an
        /// in-memory install plan and no manifest is created.
        specs: Vec<String>,''',
    '''        /// Package specs to install (`org/name[@requirement]`) when this
        /// folder has no `.zpkg.toml`. Manifest-backed projects should use
        /// `zed add` so dependency declarations remain explicit.
        #[arg(value_name = "PACKAGE")]
        specs: Vec<String>,''',
    "scope positional package specs to manifestless installs",
)

replace_once(
    "README.md",
    '''A later manifestless invocation with no
package specs uses the existing lockfile as a frozen reinstall.

With an existing manifest, `zed install <spec>...` adds those dependencies to
`[dependencies]` once and then runs the normal install. Use `zed install`
without specs to install the manifest as before.''',
    '''A later manifestless invocation with no package specs must explicitly use
`--frozen` and an existing lockfile, for example:

```sh
zed install --frozen --skip-manifest
```

Manifest-backed projects keep declarations explicit: use `zed add <spec>` to
change `[dependencies]`, then use `zed install` without positional specs.''',
    "document explicit frozen and manifest-backed boundaries",
)

PTY = r'''#!/usr/bin/env python3
"""Run a command under a real pseudo-terminal and answer zed's consent prompt."""

import os
import select
import signal
import sys
import time


def main() -> int:
    if len(sys.argv) < 4 or "--" not in sys.argv:
        raise SystemExit("usage: manifestless_pty.py <yes|no|eof> -- <command> [args...]")
    mode = sys.argv[1]
    if mode not in {"yes", "no", "eof"}:
        raise SystemExit(f"unsupported mode: {mode}")
    split = sys.argv.index("--")
    command = sys.argv[split + 1 :]
    if not command:
        raise SystemExit("missing command")

    pid, fd = os.forkpty()
    if pid == 0:
        os.execvp(command[0], command)

    deadline = time.monotonic() + 45
    output = bytearray()
    answered = False
    status = None
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    chunk = b""
                if chunk:
                    output.extend(chunk)
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                    if not answered and b"Continue? [y/N]" in output:
                        answer = {"yes": b"yes\n", "no": b"no\n", "eof": b"\x04"}[mode]
                        os.write(fd, answer)
                        answered = True
            waited, raw = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = raw
                break
        if status is None:
            os.kill(pid, signal.SIGKILL)
            _, status = os.waitpid(pid, 0)
            raise SystemExit("pseudo-terminal command timed out")
    finally:
        try:
            os.close(fd)
        except OSError:
            pass

    if not answered:
        raise SystemExit("command exited before showing the manifestless consent prompt")
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    raise SystemExit(main())
'''
Path("tests/manifestless_pty.py").write_text(PTY, encoding="utf-8")

replace_once(
    ".github/workflows/ci.yml",
    '''      - name: Symlink install works when the store is mounted
        run: |''',
    '''      - name: Prepare manifestless Node consumers
        run: |
          for name in redirected negative eof interactive automated; do
            cp -R "$RUNNER_TEMP/fixtures/node-app" "$RUNNER_TEMP/manifestless-$name"
            rm -f \
              "$RUNNER_TEMP/manifestless-$name/.zpkg.toml" \
              "$RUNNER_TEMP/manifestless-$name/.zpkg.lock"
          done

      - name: Redirected input fails closed without an explicit bypass
        run: |
          if docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-redirected:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --registry file:///registry --home /zed-home
          then
            echo "expected redirected manifestless install to require a bypass flag"
            exit 1
          fi
          test ! -e "$RUNNER_TEMP/manifestless-redirected/.zpkg.toml"
          test ! -e "$RUNNER_TEMP/manifestless-redirected/.zpkg.lock"
          test ! -e "$RUNNER_TEMP/manifestless-redirected/zed_modules"

      - name: Interactive negative and EOF responses fail closed
        run: |
          for mode in no eof; do
            dir="$RUNNER_TEMP/manifestless-${mode/no/negative}"
            if python3 zed-cli/tests/manifestless_pty.py "$mode" -- \
              docker run --rm -it \
                --volume "$dir:/work" \
                --volume "$RUNNER_TEMP/registry:/registry:ro" \
                --volume "$RUNNER_TEMP/zed-home:/zed-home" \
                --workdir /work \
                zed-pkg/install-test \
                zed install zed-pkg/docker-node-lib@^1 \
                  --registry file:///registry --home /zed-home
            then
              echo "expected $mode response to reject manifestless installation"
              exit 1
            fi
            test ! -e "$dir/.zpkg.toml"
            test ! -e "$dir/.zpkg.lock"
            test ! -e "$dir/zed_modules"
          done

      - name: Interactive acceptance infers the Node target and adapter
        run: |
          python3 zed-cli/tests/manifestless_pty.py yes -- \
            docker run --rm -it \
              --volume "$RUNNER_TEMP/manifestless-interactive:/work" \
              --volume "$RUNNER_TEMP/registry:/registry:ro" \
              --volume "$RUNNER_TEMP/zed-home:/zed-home" \
              --workdir /work \
              zed-pkg/install-test \
              zed install zed-pkg/docker-node-lib@^1 \
                --registry file:///registry --home /zed-home
          test ! -e "$RUNNER_TEMP/manifestless-interactive/.zpkg.toml"
          test -f "$RUNNER_TEMP/manifestless-interactive/.zpkg.lock"
          test -L "$RUNNER_TEMP/manifestless-interactive/zed_modules/zed-pkg/docker-node-lib"
          test -L "$RUNNER_TEMP/manifestless-interactive/node_modules/@zed-pkg/docker-node-lib"
          docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-interactive:/work:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home:ro" \
            --workdir /work \
            node:22-bookworm-slim node src/main.js

      - name: Both automation flags and frozen lock-only reinstall work
        run: |
          docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-automated:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install zed-pkg/docker-node-lib@^1 \
              --allow-no-manifest \
              --install-mode copy \
              --registry file:///registry --home /zed-home
          test ! -e "$RUNNER_TEMP/manifestless-automated/.zpkg.toml"
          test -f "$RUNNER_TEMP/manifestless-automated/.zpkg.lock"
          test -d "$RUNNER_TEMP/manifestless-automated/zed_modules/zed-pkg/docker-node-lib"
          test -d "$RUNNER_TEMP/manifestless-automated/node_modules/@zed-pkg/docker-node-lib"
          test -z "$(find "$RUNNER_TEMP/manifestless-automated/zed_modules" "$RUNNER_TEMP/manifestless-automated/node_modules" -type l -print -quit)"

          if docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-automated:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install --allow-no-manifest \
              --install-mode copy \
              --registry file:///registry --home /zed-home
          then
            echo "expected lock-only manifestless install without --frozen to fail"
            exit 1
          fi

          rm -rf \
            "$RUNNER_TEMP/manifestless-automated/zed_modules" \
            "$RUNNER_TEMP/manifestless-automated/node_modules" \
            "$RUNNER_TEMP/manifestless-automated/.zed"
          docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-automated:/work" \
            --volume "$RUNNER_TEMP/registry:/registry:ro" \
            --volume "$RUNNER_TEMP/zed-home:/zed-home" \
            --workdir /work \
            zed-pkg/install-test \
            zed install --frozen --skip-manifest \
              --install-mode copy \
              --registry file:///registry --home /zed-home
          test ! -e "$RUNNER_TEMP/manifestless-automated/.zpkg.toml"
          test -f "$RUNNER_TEMP/manifestless-automated/.zpkg.lock"
          test -z "$(find "$RUNNER_TEMP/manifestless-automated/zed_modules" "$RUNNER_TEMP/manifestless-automated/node_modules" -type l -print -quit)"
          docker run --rm \
            --volume "$RUNNER_TEMP/manifestless-automated:/work:ro" \
            --workdir /work \
            node:22-bookworm-slim node src/main.js

      - name: Symlink install works when the store is mounted
        run: |''',
    "add Docker/file-registry manifestless boundaries",
)
