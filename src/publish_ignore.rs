//! Publish-ignore parsing and artifact-local rule assembly.
//!
//! Every artifact consumes rules in one order: manifest exclusions, runtime
//! safety exclusions, then the `.zedignore` at that artifact's source root.
//! Keeping this here prevents pack staging and safety preflights from drifting.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use zed_interfaces::excludes::effective_excludes as resolve_effective_excludes;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::IGNORE_FILE;

/// Ignore-control metadata is never payload. These final rules are appended
/// after authored negations so neither a manifest nor `.zedignore` can publish
/// an ignore file accidentally.
const CONTROL_EXCLUDES: &[&str] = &[IGNORE_FILE, "**/.zedignore"];

pub(crate) fn parse_rules(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn read_rules(source_root: &Path) -> Result<Vec<String>> {
    let path = source_root.join(IGNORE_FILE);
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(parse_rules(&contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => {
            Err(error).with_context(|| format!("reading package ignore file {}", path.display()))
        }
    }
}

pub(crate) fn effective_artifact_excludes(
    manifest: &Manifest,
    ignore_rules: &[String],
) -> Vec<String> {
    let mut ordered = manifest.publish.exclude.clone();

    let modules_dir = manifest.modules_dir().trim_matches('/').to_string();
    if !modules_dir.is_empty() {
        ordered.push(format!("{modules_dir}/**"));
    }

    ordered.push(format!("{}/**", crate::transaction::STAGING_DIR));
    ordered.extend(ignore_rules.iter().cloned());

    let mut excludes = resolve_effective_excludes(&ordered, manifest.publish.include_readme);
    for pattern in CONTROL_EXCLUDES {
        if !excludes
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(pattern))
        {
            excludes.push((*pattern).to_string());
        }
    }
    excludes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"[package]
org = "acme"
name = "publish-ignore"
version = "1.0.0"

[package.repository]
url = "https://example.invalid/acme/publish-ignore"

{extra}
"#
        ))
        .unwrap()
    }

    #[test]
    fn parser_ignores_comments_and_blank_lines() {
        assert_eq!(
            parse_rules("\n # comment\n.cache/**\n\n !target/ \n"),
            vec![".cache/**", "!target/"]
        );
    }

    #[test]
    fn artifact_ignore_negation_runs_after_manifest_rules() {
        let manifest = manifest("[publish]\nexclude = [\"target/**\"]");
        let excludes = effective_artifact_excludes(&manifest, &["!target".to_string()]);
        assert!(!excludes.iter().any(|rule| rule == "target/**"));
        assert!(!excludes.iter().any(|rule| rule == "**/target/**"));
    }

    #[test]
    fn ignore_control_files_cannot_be_reincluded() {
        let manifest = manifest("");
        let excludes = effective_artifact_excludes(
            &manifest,
            &["!.zedignore".to_string(), "!**/.zedignore".to_string()],
        );
        assert!(excludes.iter().any(|rule| rule == ".zedignore"));
        assert!(excludes.iter().any(|rule| rule == "**/.zedignore"));
    }
}
