#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;
use zed_cli::store::Store;
use zed_cli::versioned_graph::{
    LOCAL_MATERIALIZATION_SCHEMA_V1, LocalGraphMaterializationPlan, LocalPackageSource,
    VersionedMaterializationReport,
};
use zed_interfaces::dependency_graph::{
    DependencyGraphCompleteness, DependencyGraphData, DependencyGraphDocument, DependencyKind,
    PackageVersionIdentity, RegistrySnapshot, ResolutionProvenance, ResolvedDependencyEdge,
    ResolvedDependencyNode,
};

fn id(name: &str, version: &str) -> PackageVersionIdentity {
    PackageVersionIdentity {
        registry_id: "fixture-registry".into(),
        org: "fixture".into(),
        name: name.into(),
        version: version.into(),
    }
}

fn digest(seed: u8) -> String {
    format!("sha256:{}", format!("{seed:02x}").repeat(32))
}

fn node(id: PackageVersionIdentity, seed: u8) -> ResolvedDependencyNode {
    ResolvedDependencyNode {
        id,
        artifact_digest: Some(digest(seed)),
        features: Vec::new(),
    }
}

fn edge(from: PackageVersionIdentity, to: PackageVersionIdentity) -> ResolvedDependencyEdge {
    ResolvedDependencyEdge {
        from,
        to,
        kind: DependencyKind::Runtime,
        requirement: Some("exact fixture edge".into()),
        target: None,
        optional: false,
        features: Vec::new(),
    }
}

fn canonical_cycle_graph() -> DependencyGraphDocument {
    let a1 = id("a", "1");
    let b1 = id("b", "1");
    let a2 = id("a", "2");
    let b0 = id("b", "0");
    DependencyGraphDocument {
        schema: zed_interfaces::dependency_graph::DEPENDENCY_GRAPH_SCHEMA_V1.into(),
        graph: DependencyGraphData::Resolved {
            completeness: DependencyGraphCompleteness::Complete,
            roots: vec![a1.clone()],
            nodes: vec![
                node(a1.clone(), 1),
                node(b1.clone(), 2),
                node(a2.clone(), 3),
                node(b0.clone(), 4),
            ],
            edges: vec![
                edge(a1, b1.clone()),
                edge(b1, a2.clone()),
                edge(a2.clone(), b0.clone()),
                edge(b0, a2),
            ],
            provenance: ResolutionProvenance {
                resolver_version: "versioned-cycle-integration/v1".into(),
                target: "test-target".into(),
                enabled_features: Vec::new(),
                registry_snapshots: vec![RegistrySnapshot {
                    registry_id: "fixture-registry".into(),
                    checkpoint_digest: digest(0xee),
                }],
                lock_digest: digest(0xdd),
            },
            parent_graph_digest: None,
            projection: None,
        },
        graph_digest: None,
    }
    .finalize()
    .unwrap()
}

fn write_payload(source: &Path, label: &str) {
    fs::create_dir_all(source).unwrap();
    fs::write(source.join("payload.txt"), format!("{label}\n")).unwrap();
}

fn run_zed(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn parse_report(output: &Output) -> VersionedMaterializationReport {
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .unwrap()
        .to_string();
    serde_json::from_str(&line).unwrap()
}

fn follow_chain(project: &Path) -> (PathBuf, PathBuf) {
    let a1 = project.join("zed_modules/fixture/a");
    let b1 = a1.join("zed_modules/fixture/b");
    let a2 = b1.join("zed_modules/fixture/a");
    let b0 = a2.join("zed_modules/fixture/b");
    let closing_a2 = b0.join("zed_modules/fixture/a");
    (a2, closing_a2)
}

#[test]
fn public_cli_materializes_and_replays_the_four_node_cycle_without_payload_copies() {
    let temp = tempdir().unwrap();
    let graph = canonical_cycle_graph();
    let DependencyGraphData::Resolved { nodes, .. } = &graph.graph else {
        unreachable!()
    };
    let sources = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let source = temp.path().join(format!("source-{index}"));
            write_payload(&source, &node.id.to_string());
            LocalPackageSource {
                id: node.id.clone(),
                source,
            }
        })
        .collect();
    let plan = LocalGraphMaterializationPlan {
        schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
        graph,
        sources,
    };
    let plan_path = temp.path().join("plan.json");
    fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let project = temp.path().join("project");
    let home = temp.path().join("home");

    let first = run_zed(
        &home,
        &[
            "graph",
            "materialize",
            "--plan",
            plan_path.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("[Warn] [zed-cli] circular dependency detected:"));
    assert!(stdout.contains("fixture-registry::fixture/a@2"));
    assert!(stdout.contains("fixture-registry::fixture/b@0"));
    assert!(stdout.contains("reuses the existing exact node through a symlink"));
    let first_report = parse_report(&first);
    assert_eq!(first_report.nodes, 4);
    assert_eq!(first_report.edges, 4);
    assert_eq!(first_report.cycles.len(), 1);
    assert!(!first_report.reused_generation);

    let node_dirs = fs::read_dir(first_report.generation_dir.join("nodes"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(node_dirs.len(), 4);
    for node_dir in node_dirs {
        assert!(
            fs::symlink_metadata(node_dir.path().join("root/payload.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let (a2, closing_a2) = follow_chain(&project);
    assert_eq!(
        fs::read_link(closing_a2).unwrap(),
        a2.canonicalize().unwrap()
    );

    let second = run_zed(
        &home,
        &[
            "graph",
            "materialize",
            "--plan",
            plan_path.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
        ],
    );
    let second_report = parse_report(&second);
    assert!(second_report.reused_generation);
    assert_eq!(first_report.generation_dir, second_report.generation_dir);
    assert_eq!(first_report.graph_digest, second_report.graph_digest);
    assert_eq!(
        first_report.materialization_digest,
        second_report.materialization_digest
    );

    let copy = run_zed(
        &home,
        &[
            "graph",
            "materialize",
            "--plan",
            plan_path.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
            "--mode",
            "copy",
        ],
    );
    assert!(!copy.status.success());
    assert!(
        String::from_utf8_lossy(&copy.stderr)
            .contains("copy mode cannot represent a circular exact-version dependency graph")
    );
}

#[test]
fn verified_graph_path_binds_nodes_to_the_content_addressed_store() {
    let temp = tempdir().unwrap();
    let graph = canonical_cycle_graph();
    let graph_path = temp.path().join("graph.json");
    fs::write(&graph_path, graph.canonical_document_bytes().unwrap()).unwrap();
    let home = temp.path().join("home");
    let store = Store::new(&home);
    let DependencyGraphData::Resolved { nodes, .. } = &graph.graph else {
        unreachable!()
    };
    for node in nodes {
        let sha256 = node
            .artifact_digest
            .as_deref()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        write_payload(&store.pkg_dir(sha256), &node.id.to_string());
    }
    let project = temp.path().join("store-project");
    let output = run_zed(
        &home,
        &[
            "graph",
            "materialize",
            "--graph",
            graph_path.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
        ],
    );
    let report = parse_report(&output);
    assert_eq!(report.nodes, 4);
    let node_metadata = fs::read_dir(report.generation_dir.join("nodes"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            serde_json::from_slice::<Value>(&fs::read(entry.path().join("node.json")).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        node_metadata.iter().all(|record| {
            Path::new(record["source"].as_str().unwrap()).starts_with(store.root())
        })
    );
}
