use anyhow::{Context, Result};
use toml::Value;
use zed_interfaces::manifest_env::{EnvDeclaration, parse_manifest_env};

/// Validate and return the package environment inventory declared by `[[env]]`.
pub fn validate_manifest_env(text: &str) -> Result<Vec<EnvDeclaration>> {
    parse_manifest_env(text).map_err(anyhow::Error::msg)
}

/// Reattach a validated `[[env]]` inventory to a manifest rendered from the
/// core `zed_interfaces::Manifest` type.
///
/// This is an additive compatibility bridge while the generic manifest model
/// migrates to owning the field directly. It prevents `zed add`, `zed remove`,
/// and other manifest rewrites from silently dropping package environment
/// metadata. No values are resolved or injected here.
pub fn preserve_manifest_env(existing: Option<&str>, rendered: &str) -> Result<String> {
    let Some(existing) = existing else {
        return Ok(rendered.to_owned());
    };
    let declarations = validate_manifest_env(existing)?;
    if declarations.is_empty() {
        return Ok(rendered.to_owned());
    }

    let existing_document: Value =
        toml::from_str(existing).context("parsing existing .zpkg.toml while preserving [[env]]")?;
    let env = existing_document
        .get("env")
        .cloned()
        .expect("validated non-empty env inventory has an env value");

    let mut rendered_document: Value =
        toml::from_str(rendered).context("parsing rendered .zpkg.toml while preserving [[env]]")?;
    rendered_document
        .as_table_mut()
        .context("rendered .zpkg.toml root is not a table")?
        .insert("env".to_string(), env);

    toml::to_string_pretty(&rendered_document)
        .context("serializing .zpkg.toml after preserving [[env]]")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[package]
org = "acme"
name = "tool"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"
"#;

    const WITH_ENV: &str = r#"
[package]
org = "acme"
name = "tool"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"

[[env]]
name = "mode"
key = "ACME_MODE"
kind = "string"
required = false
secret = false
exposure = "env-only"
description = "Optional runtime mode."
overrides = ["runtime.mode"]
environments = ["dev", "stage", "prod"]
defaultValue = "normal"
"#;

    #[test]
    fn validates_full_manifest_env_inventory() {
        let declarations = validate_manifest_env(WITH_ENV).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].key, "ACME_MODE");
    }

    #[test]
    fn manifest_rewrite_preserves_env_array() {
        let rewritten = preserve_manifest_env(Some(WITH_ENV), BASE).unwrap();
        let declarations = validate_manifest_env(&rewritten).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].default_value.as_deref(), Some("normal"));
    }

    #[test]
    fn manifest_without_env_is_unchanged() {
        assert_eq!(preserve_manifest_env(Some(BASE), BASE).unwrap(), BASE);
    }
}
