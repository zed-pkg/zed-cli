use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use zed_interfaces::manifest::Manifest;

use crate::config::read_manifest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleasePlan {
    pub release_set: String,
    pub source: ReleaseSource,
    pub zed: Vec<ZedReleaseArtifact>,
    pub native: Vec<NativeReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseSource {
    pub package: String,
    pub version: String,
    pub vcs_tag: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZedReleaseArtifact {
    pub target: Option<String>,
    pub package: String,
    pub version: String,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeReleaseArtifact {
    pub target: String,
    pub registry: String,
    pub package: String,
    pub version: String,
    pub dir: String,
}

pub fn build_plan(manifest: &Manifest) -> ReleasePlan {
    let source_package = manifest.full_name();
    let version = manifest.package.version.clone();
    let vcs_tag = manifest.vcs_tag();

    let zed = if manifest.targets.is_empty() {
        vec![ZedReleaseArtifact {
            target: None,
            package: source_package.clone(),
            version: version.clone(),
            dir: ".".to_string(),
        }]
    } else {
        manifest
            .target_package_names()
            .into_iter()
            .map(|(target, package_name)| {
                let section = manifest
                    .targets
                    .get(&target)
                    .expect("target_package_names only returns declared targets");
                ZedReleaseArtifact {
                    target: Some(target),
                    package: format!("{}/{}", manifest.package.org, package_name),
                    version: version.clone(),
                    dir: section.dir.clone(),
                }
            })
            .collect()
    };

    let native = manifest
        .native_release_routes()
        .into_iter()
        .map(|route| NativeReleaseArtifact {
            target: route.target,
            registry: route.registry.as_str().to_string(),
            package: route.package,
            version: version.clone(),
            dir: route.dir,
        })
        .collect();

    ReleasePlan {
        release_set: format!("{source_package}@{version}#{vcs_tag}"),
        source: ReleaseSource {
            package: source_package,
            version,
            vcs_tag,
            repository: manifest.package.repository.url.clone(),
        },
        zed,
        native,
    }
}

pub fn render_human(plan: &ReleasePlan) -> String {
    let mut output = String::new();
    output.push_str(&format!("release set {}\n", plan.release_set));
    output.push_str(&format!(
        "source: {} @ {} ({})\n",
        plan.source.repository, plan.source.vcs_tag, plan.source.package
    ));
    output.push_str("zed artifacts:\n");
    for artifact in &plan.zed {
        let target = artifact.target.as_deref().unwrap_or("repository");
        output.push_str(&format!(
            "  - {}/{} <- {} [target: {}]\n",
            artifact.package, artifact.version, artifact.dir, target
        ));
    }
    output.push_str("native artifacts:\n");
    if plan.native.is_empty() {
        output.push_str("  - none declared\n");
    } else {
        for artifact in &plan.native {
            output.push_str(&format!(
                "  - {} {}@{} <- {} [target: {}]\n",
                artifact.registry,
                artifact.package,
                artifact.version,
                artifact.dir,
                artifact.target
            ));
        }
    }
    output
}

pub fn plan(project: &Path, json: bool) -> Result<()> {
    let manifest = read_manifest(project)?;
    let plan = build_plan(&manifest);
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", render_human(&plan));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polyglot_plan_is_deterministic_and_includes_native_routes() {
        let manifest = Manifest::parse(
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

[targets.repository]
dir = "."
name = "clients-repository"

[targets.nodejs]
dir = "clients/typescript"
adapter = "node"

[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
"#,
        )
        .unwrap();

        let plan = build_plan(&manifest);
        assert_eq!(plan.release_set, "acme/clients@1.2.3#v1.2.3");
        assert_eq!(
            plan.zed
                .iter()
                .map(|artifact| artifact.target.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["nodejs", "repository", "rust"]
        );
        assert_eq!(
            plan.native
                .iter()
                .map(|artifact| (artifact.registry.as_str(), artifact.package.as_str()))
                .collect::<Vec<_>>(),
            vec![("npm", "@acme/client"), ("crates-io", "acme-client")]
        );

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"release_set\":\"acme/clients@1.2.3#v1.2.3\""));
        let human = render_human(&plan);
        assert!(human.contains("native artifacts:"));
        assert!(human.contains("npm @acme/client@1.2.3"));
    }

    #[test]
    fn single_language_plan_keeps_the_root_package() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "acme"
name = "http-kit"
version = "0.4.0"

[package.repository]
url = "https://github.com/acme/http-kit"
"#,
        )
        .unwrap();

        let plan = build_plan(&manifest);
        assert_eq!(plan.zed.len(), 1);
        assert_eq!(plan.zed[0].target, None);
        assert_eq!(plan.zed[0].package, "acme/http-kit");
        assert!(plan.native.is_empty());
        assert!(render_human(&plan).contains("none declared"));
    }
}
