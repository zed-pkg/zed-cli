use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use zed_interfaces::manifest::{Manifest, NativeRegistry};

use crate::config::read_manifest;
use crate::release::validate_native_manifests;

const CREDENTIAL_ENV_VARS: &[&str] = &[
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "NPM_CONFIG_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "CARGO_REGISTRIES_CRATES_IO_TOKEN",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePreflightSpec {
    pub target: String,
    pub registry: NativeRegistry,
    pub package: String,
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
}

pub trait CommandRunner {
    fn run(&self, spec: &NativePreflightSpec) -> Result<Output>;
}

#[derive(Debug, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, spec: &NativePreflightSpec) -> Result<Output> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in CREDENTIAL_ENV_VARS {
            command.env_remove(name);
        }
        command.output().with_context(|| {
            format!(
                "run {} preflight for target `{}` in {}",
                spec.registry.as_str(),
                spec.target,
                spec.cwd.display()
            )
        })
    }
}

pub fn build_specs(project: &Path, manifest: &Manifest) -> Vec<NativePreflightSpec> {
    manifest
        .native_release_routes()
        .into_iter()
        .map(|route| {
            let (program, args) = match route.registry {
                NativeRegistry::Npm => (
                    "npm",
                    vec!["pack", "--dry-run", "--json", "--ignore-scripts"],
                ),
                NativeRegistry::CratesIo => {
                    ("cargo", vec!["package", "--list", "--allow-dirty"])
                }
            };
            NativePreflightSpec {
                target: route.target,
                registry: route.registry,
                package: route.package,
                cwd: project.join(route.dir),
                program: program.to_string(),
                args: args.into_iter().map(str::to_string).collect(),
            }
        })
        .collect()
}

pub fn execute_specs(runner: &impl CommandRunner, specs: &[NativePreflightSpec]) -> Result<()> {
    for spec in specs {
        println!(
            "preflight {} {} [target: {}]",
            spec.registry.as_str(),
            spec.package,
            spec.target
        );
        let output = runner.run(spec)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "{} preflight failed for target `{}` ({}):\n{}{}",
                spec.registry.as_str(),
                spec.target,
                spec.package,
                stdout,
                stderr
            );
        }
    }
    Ok(())
}

pub fn preflight(project: &Path) -> Result<()> {
    let manifest = read_manifest(project)?;
    validate_native_manifests(project, &manifest)?;
    let specs = build_specs(project, &manifest);
    if specs.is_empty() {
        println!("native preflight: no native release routes declared");
        return Ok(());
    }
    execute_specs(&ProcessRunner, &specs)?;
    println!("native preflight passed: {} target(s)", specs.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn manifest() -> Manifest {
        Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/clients"

[targets.rust]
dir = "clients/rust"
[targets.rust.native]
registry = "crates-io"
package = "acme-client"

[targets.nodejs]
dir = "clients/typescript"
[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
"#,
        )
        .unwrap()
    }

    #[test]
    fn adapter_commands_are_fixed_and_deterministic() {
        let specs = build_specs(Path::new("/repo"), &manifest());
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].target, "nodejs");
        assert_eq!(specs[0].program, "npm");
        assert_eq!(
            specs[0].args,
            ["pack", "--dry-run", "--json", "--ignore-scripts"]
        );
        assert_eq!(specs[0].cwd, Path::new("/repo/clients/typescript"));
        assert_eq!(specs[1].target, "rust");
        assert_eq!(specs[1].program, "cargo");
        assert_eq!(
            specs[1].args,
            ["package", "--list", "--allow-dirty"]
        );
        assert_eq!(specs[1].cwd, Path::new("/repo/clients/rust"));
    }

    struct FakeRunner {
        calls: RefCell<Vec<NativePreflightSpec>>,
        fail_target: Option<&'static str>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: &NativePreflightSpec) -> Result<Output> {
            self.calls.borrow_mut().push(spec.clone());
            let success = self.fail_target != Some(spec.target.as_str());
            Ok(Output {
                status: if success {
                    std::process::ExitStatus::from_raw(0)
                } else {
                    std::process::ExitStatus::from_raw(1 << 8)
                },
                stdout: if success { b"ok".to_vec() } else { Vec::new() },
                stderr: if success {
                    Vec::new()
                } else {
                    b"synthetic failure".to_vec()
                },
            })
        }
    }

    #[test]
    fn every_declared_adapter_runs_once() {
        let runner = FakeRunner {
            calls: RefCell::new(Vec::new()),
            fail_target: None,
        };
        let specs = build_specs(Path::new("/repo"), &manifest());
        execute_specs(&runner, &specs).unwrap();
        assert_eq!(
            runner
                .calls
                .borrow()
                .iter()
                .map(|call| call.target.as_str())
                .collect::<Vec<_>>(),
            vec!["nodejs", "rust"]
        );
    }

    #[test]
    fn a_failed_adapter_stops_the_release_preflight() {
        let runner = FakeRunner {
            calls: RefCell::new(Vec::new()),
            fail_target: Some("rust"),
        };
        let specs = build_specs(Path::new("/repo"), &manifest());
        let error = execute_specs(&runner, &specs).unwrap_err().to_string();
        assert!(error.contains("crates-io preflight failed"));
        assert!(error.contains("synthetic failure"));
    }
}
