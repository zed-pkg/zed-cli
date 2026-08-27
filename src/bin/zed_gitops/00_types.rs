use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Output};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

const API_VERSION: &str = "oresoftware.dev/v1alpha1";
const KIND: &str = "GitOpsApplication";
const SCHEMA_REFERENCE: &str = "../application.schema.json";
const CLUSTER_REPOSITORY: &str = "github.com/oresoftware/k8s-cluster";
const GITLINK_CONTRACT_API_VERSION: &str = "zed.dev/gitops/v1alpha1";
const GITLINK_CONTRACT_KIND: &str = "GitlinkContract";
const DEFAULT_SCHEMA: &str = "catalog/gitops/gitlink-contract.v1alpha1.json";
const DEFAULT_CATALOG: &str = "catalog/gitops/apps";

#[derive(Debug, Parser)]
#[command(
    name = "zed-gitops",
    version,
    about = "Validate exact Git-submodule pins against direct Argo CD sources"
)]
struct Cli {
    #[command(subcommand)]
    command: TopLevelCommand,
}

#[derive(Debug, Subcommand)]
enum TopLevelCommand {
    /// Validate a repository-owned GitOps application composition catalog.
    Validate(ValidateArgs),
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// Superproject root containing .gitmodules and the Git index.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Repository-relative directory containing GitOpsApplication JSON records.
    #[arg(long, default_value = DEFAULT_CATALOG)]
    catalog: PathBuf,

    /// Versioned gitlink contract owned by the target repository.
    #[arg(long, value_name = "PATH")]
    schema: Option<PathBuf>,

    /// Compare gitlinks against this already-fetched local ref.
    #[arg(long, value_name = "REF")]
    changed_from: Option<String>,

    /// Output suitable for humans, automation, or code-scanning annotation.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,

    /// Reject unknown fields in catalog and gitlink-contract objects.
    #[arg(long)]
    strict: bool,

    /// Use only local repository evidence. Online reachability is a future opt-in.
    #[arg(long)]
    offline: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
    Sarif,
}

#[derive(Debug, Clone, Deserialize)]
struct Record {
    #[serde(rename = "$schema")]
    schema: String,
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: Metadata,
    spec: Spec,
}

#[derive(Debug, Clone, Deserialize)]
struct Metadata {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Spec {
    owner: String,
    inventory: Inventory,
    source: Source,
    argo: Argo,
    migration: Migration,
}

#[derive(Debug, Clone, Deserialize)]
struct Inventory {
    mode: String,
    path: String,
    repository: String,
    revision: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Source {
    mode: String,
    repository: String,
    #[serde(rename = "targetRevision")]
    target_revision: String,
    path: String,
    renderer: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Argo {
    project: String,
    namespace: String,
    #[serde(rename = "destinationServer")]
    destination_server: String,
    automated: bool,
    prune: bool,
    #[serde(rename = "selfHeal")]
    self_heal: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Migration {
    phase: String,
    #[serde(rename = "staticApplication")]
    static_application: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitlinkContract {
    #[serde(rename = "$schema")]
    #[allow(dead_code)]
    schema: Option<String>,
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    spec: GitlinkContractSpec,
}

#[derive(Debug, Clone, Deserialize)]
struct GitlinkContractSpec {
    #[serde(default, rename = "approvedAppPathPrefixes")]
    approved_app_path_prefixes: Vec<String>,
    #[serde(default, rename = "forbiddenPathSuffixes")]
    forbidden_path_suffixes: Vec<String>,
    #[serde(default, rename = "allowedGitlinks")]
    allowed_gitlinks: Vec<AllowedGitlink>,
}

#[derive(Debug, Clone, Deserialize)]
struct AllowedGitlink {
    path: String,
    #[serde(default)]
    repository: Option<String>,
}

#[derive(Debug, Clone)]
struct ConfiguredSubmodule {
    path: String,
    url: String,
}

#[derive(Debug, Default)]
struct SubmoduleBuilder {
    path: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Diagnostic {
    rule_id: String,
    message: String,
    path: String,
    application: String,
    severity: String,
}

impl Diagnostic {
    fn error(
        rule_id: impl Into<String>,
        message: impl Into<String>,
        path: impl Into<String>,
        application: impl Into<String>,
    ) -> Self {
        Self {
            rule_id: rule_id.into(),
            message: message.into(),
            path: path.into(),
            application: application.into(),
            severity: "error".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    valid: bool,
    records: usize,
    gitlinks: usize,
    errors: usize,
    warnings: usize,
    offline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_from: Option<String>,
    changed_gitlinks: Vec<String>,
    diagnostics: Vec<Diagnostic>,
}
