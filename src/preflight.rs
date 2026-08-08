use std::fs;
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
    "PUB_HOSTED_URL",
    "PUB_TOKEN",
    "DART_PUB_TOKEN",
    "TWINE_USERNAME",
    "TWINE_PASSWORD",
    "TWINE_REPOSITORY_URL",
    "UV_PUBLISH_TOKEN",
    "UV_PUBLISH_USERNAME",
    "UV_PUBLISH_PASSWORD",
    "PYPI_TOKEN",
    "PIP_INDEX_URL",
    "PIP_EXTRA_INDEX_URL",
    "MAVEN_USERNAME",
    "MAVEN_PASSWORD",
    "GEM_HOST_API_KEY",
    "NUGET_API_KEY",
    "COMPOSER_AUTH",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "CI_JOB_TOKEN",
    "BITBUCKET_PACKAGES_TOKEN",
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
        if matches!(
            spec.registry,
            NativeRegistry::PyPi | NativeRegistry::RubyGems | NativeRegistry::NuGet
        ) {
            let output = spec
                .cwd
                .join(".zed/native-preflight")
                .join(spec.registry.as_str());
            if output.exists() {
                fs::remove_dir_all(&output).with_context(|| {
                    format!("remove stale native preflight output {}", output.display())
                })?;
            }
            fs::create_dir_all(&output)
                .with_context(|| format!("create native preflight output {}", output.display()))?;
        }
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

fn python_program() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn gemspec_filename(root: &Path, package: &str) -> String {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("gemspec"))
        .and_then(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{package}.gemspec"))
}

pub fn build_specs(project: &Path, manifest: &Manifest) -> Vec<NativePreflightSpec> {
    manifest
        .native_release_routes()
        .into_iter()
        .map(|route| {
            let target_root = project.join(&route.dir);
            let gemspec = gemspec_filename(&target_root, &route.package);
            let (program, args) = match route.registry {
                NativeRegistry::Npm => (
                    "npm",
                    vec!["pack", "--dry-run", "--json", "--ignore-scripts"],
                ),
                NativeRegistry::CratesIo => ("cargo", vec!["package", "--list", "--allow-dirty"]),
                NativeRegistry::PubDev => ("dart", vec!["pub", "publish", "--dry-run"]),
                NativeRegistry::PyPi => (
                    python_program(),
                    vec![
                        "-m",
                        "build",
                        "--no-isolation",
                        "--outdir",
                        ".zed/native-preflight/pypi",
                    ],
                ),
                NativeRegistry::MavenCentral => {
                    ("mvn", vec!["--batch-mode", "-DskipTests", "package"])
                }
                NativeRegistry::RubyGems => (
                    "gem",
                    vec![
                        "build",
                        "--strict",
                        "--output",
                        ".zed/native-preflight/rubygems/package.gem",
                        // RubyGems convention; manifest validation runs first
                        // and provides a precise error if the target differs.
                        gemspec.as_str(),
                    ],
                ),
                NativeRegistry::NuGet => (
                    "dotnet",
                    vec!["pack", "--output", ".zed/native-preflight/nuget"],
                ),
                NativeRegistry::Packagist => {
                    ("composer", vec!["validate", "--strict", "--no-interaction"])
                }
                NativeRegistry::GoModules => ("go", vec!["list", "./..."]),
            };
            NativePreflightSpec {
                target: route.target,
                registry: route.registry,
                package: route.package,
                cwd: target_root,
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
    use std::process::ExitStatus;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    use super::*;

    fn exit_status(success: bool) -> ExitStatus {
        #[cfg(unix)]
        {
            ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        }
        #[cfg(windows)]
        {
            ExitStatus::from_raw(if success { 0 } else { 1 })
        }
    }

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

[targets.dart]
dir = "clients/dart"
[targets.dart.native]
registry = "pub.dev"
package = "acme_client"

[targets.python]
dir = "clients/python"
[targets.python.native]
registry = "pypi"
package = "Acme.Client"
"#,
        )
        .unwrap()
    }

    #[test]
    fn adapter_commands_are_fixed_and_deterministic() {
        let specs = build_specs(Path::new("/repo"), &manifest());
        assert_eq!(specs.len(), 4);
        assert_eq!(specs[0].target, "dart");
        assert_eq!(specs[0].program, "dart");
        assert_eq!(specs[0].args, ["pub", "publish", "--dry-run"]);
        assert_eq!(specs[0].cwd, Path::new("/repo/clients/dart"));
        assert_eq!(specs[1].target, "nodejs");
        assert_eq!(specs[1].program, "npm");
        assert_eq!(
            specs[1].args,
            ["pack", "--dry-run", "--json", "--ignore-scripts"]
        );
        assert_eq!(specs[1].cwd, Path::new("/repo/clients/typescript"));
        assert_eq!(specs[2].target, "python");
        assert_eq!(specs[2].program, python_program());
        assert_eq!(
            specs[2].args,
            [
                "-m",
                "build",
                "--no-isolation",
                "--outdir",
                ".zed/native-preflight/pypi",
            ]
        );
        assert_eq!(specs[2].cwd, Path::new("/repo/clients/python"));
        assert_eq!(specs[3].target, "rust");
        assert_eq!(specs[3].program, "cargo");
        assert_eq!(specs[3].args, ["package", "--list", "--allow-dirty"]);
        assert_eq!(specs[3].cwd, Path::new("/repo/clients/rust"));
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
                status: exit_status(success),
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
            vec!["dart", "nodejs", "python", "rust"]
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

    #[test]
    fn extended_registry_adapters_are_fixed_and_credential_free() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "clients"
version = "1.2.3"
[package.repository]
url = "https://github.com/acme/clients"

[targets.java]
dir = "java"
[targets.java.native]
registry = "maven-central"
package = "com.acme:client"

[targets.ruby]
dir = "ruby"
[targets.ruby.native]
registry = "rubygems"
package = "acme-client"

[targets.csharp]
dir = "csharp"
[targets.csharp.native]
registry = "nuget"
package = "Acme.Client"

[targets.php]
dir = "php"
[targets.php.native]
registry = "packagist"
package = "acme/client"

[targets.golang]
dir = "go"
[targets.golang.native]
registry = "go-modules"
package = "github.com/acme/client"
tag_format = "go/v{version}"
"#,
        )
        .unwrap();
        let specs = build_specs(Path::new("/repo"), &manifest);
        assert_eq!(
            specs
                .iter()
                .map(|spec| (spec.target.as_str(), spec.program.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("csharp", "dotnet"),
                ("golang", "go"),
                ("java", "mvn"),
                ("php", "composer"),
                ("ruby", "gem"),
            ]
        );
        assert_eq!(
            specs.last().unwrap().args,
            [
                "build",
                "--strict",
                "--output",
                ".zed/native-preflight/rubygems/package.gem",
                "acme-client.gemspec",
            ]
        );
    }

    #[test]
    fn rubygems_preflight_uses_the_manifest_filename_not_the_package_name() {
        let root = tempfile::tempdir().unwrap();
        let ruby = root.path().join("ruby");
        fs::create_dir_all(&ruby).unwrap();
        fs::write(
            ruby.join("client.gemspec"),
            "# validated before preflight\n",
        )
        .unwrap();
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "client"
version = "1.2.3"
[package.repository]
url = "https://github.com/acme/client"

[targets.ruby]
dir = "ruby"
[targets.ruby.native]
registry = "rubygems"
package = "acme-client"
"#,
        )
        .unwrap();

        let specs = build_specs(root.path(), &manifest);
        assert_eq!(specs[0].args.last().unwrap(), "client.gemspec");
    }
}
