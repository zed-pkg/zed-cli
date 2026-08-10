//! Shared command/output layer for canonical `zed env export devbox|flox`
//! and the staged `zed-env-export` compatibility binary.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::nix_environment_export::{ExportManager, ExportResult, export_environment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub plan: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub receipt: Option<PathBuf>,
    pub json: bool,
}

pub fn execute(root: &Path, manager: ExportManager, options: ExportOptions) -> Result<()> {
    let result = export_environment(
        root,
        manager,
        options.plan.as_deref(),
        options.output.as_deref(),
        options.receipt.as_deref(),
    )?;
    print_result(&result, options.json)
}

pub fn execute_current_dir(manager: ExportManager, options: ExportOptions) -> Result<()> {
    let root = std::env::current_dir()?;
    execute(&root, manager, options)
}

pub fn print_result(result: &ExportResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        println!(
            "{} environment {}: {}",
            result.manager.as_str(),
            if result.changed {
                "exported"
            } else {
                "unchanged"
            },
            result.output_path
        );
        println!("receipt: {}", result.receipt_path);
        println!(
            "environment-plan-sha256: {}",
            result.environment_plan_sha256
        );
        println!("output-sha256: {}", result.output_sha256);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_retain_project_relative_paths_without_defaults() {
        let options = ExportOptions {
            plan: Some(PathBuf::from(".zed/environment-plan.json")),
            output: None,
            receipt: None,
            json: true,
        };
        assert_eq!(
            options.plan.as_deref(),
            Some(Path::new(".zed/environment-plan.json"))
        );
        assert!(options.output.is_none());
        assert!(options.receipt.is_none());
        assert!(options.json);
    }
}
