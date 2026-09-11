//! Fail-closed admission for zed-cli's embedded Shared Auth consumer policy.
//!
//! The policy vocabulary is owned by `shared-auth/shared-auth-interfaces`.
//! Zed deliberately embeds the supported `.auth-shared.toml` compatibility
//! alias so installed binaries do not search the caller's working directory
//! for auth policy. Runtime credentials remain outside this file entirely.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

const POLICY: &str = include_str!("../../.auth-shared.toml");
const SCHEMA_VERSION: u32 = 1;
const INTERFACES_REPOSITORY: &str = "https://github.com/shared-auth/shared-auth-interfaces";
const INTERFACES_REVISION: &str = "52b7ac7fbf0c7c169684f613eda923f3aa6c82e9";

pub(super) fn admit_embedded() -> Result<()> {
    let policy: SharedAuthPolicy =
        toml::from_str(POLICY).context("parsing embedded .auth-shared.toml policy")?;
    policy.validate()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedAuthPolicy {
    schema_version: u32,
    compatibility: Compatibility,
    #[serde(default)]
    factors: Option<FactorsPolicy>,
    #[serde(default)]
    pages: Option<PagesPolicy>,
    #[serde(default)]
    styling: Option<StylingPolicy>,
}

impl SharedAuthPolicy {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported Shared Auth policy schema version"
        );
        self.compatibility.validate()?;

        self.factors
            .as_ref()
            .context("zed-cli Shared Auth policy must declare factors.two_factor")?
            .validate()?;
        if let Some(pages) = &self.pages {
            pages.validate()?;
        }
        if let Some(styling) = &self.styling {
            styling.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compatibility {
    repository: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    range: Option<CommitRange>,
}

impl Compatibility {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.repository == INTERFACES_REPOSITORY,
            "Shared Auth policy points at an unexpected interfaces repository"
        );
        match (&self.commit, &self.range) {
            (Some(commit), None) => {
                validate_sha(commit)?;
                ensure!(
                    commit == INTERFACES_REVISION,
                    "Shared Auth policy revision is not the zed-cli admitted interfaces revision"
                );
                Ok(())
            }
            (None, Some(range)) => {
                range.validate()?;
                bail!(
                    "zed-cli's embedded Shared Auth policy requires exact commit provenance; range provenance needs repository ancestry admission"
                )
            }
            (Some(_), Some(_)) | (None, None) => {
                bail!("Shared Auth compatibility must declare exactly one of commit or range")
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitRange {
    base: String,
    head: String,
}

impl CommitRange {
    fn validate(&self) -> Result<()> {
        validate_sha(&self.base)?;
        validate_sha(&self.head)
    }
}

fn validate_sha(value: &str) -> Result<()> {
    ensure!(
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "Shared Auth compatibility revision must be a 40-character lowercase hexadecimal commit SHA"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactorsPolicy {
    #[serde(default)]
    two_factor: Option<TwoFactorPolicy>,
    #[serde(default)]
    three_factor: Option<ThreeFactorPolicy>,
}

impl FactorsPolicy {
    fn validate(&self) -> Result<()> {
        self.two_factor
            .as_ref()
            .context("zed-cli Shared Auth policy must declare factors.two_factor")?
            .validate()?;
        if let Some(three_factor) = &self.three_factor {
            three_factor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TwoFactorPolicy {
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    methods: Option<Vec<FactorMethod>>,
}

impl TwoFactorPolicy {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.required == Some(true),
            "zed-cli Shared Auth policy must explicitly require two-factor authentication"
        );
        let methods = self
            .methods
            .as_deref()
            .context("zed-cli Shared Auth two-factor policy must declare methods")?;
        validate_unique_nonempty(Some(methods), "two-factor methods")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreeFactorPolicy {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    methods: Option<Vec<FactorMethod>>,
}

impl ThreeFactorPolicy {
    fn validate(&self) -> Result<()> {
        let _enabled = self.enabled;
        validate_unique_nonempty(self.methods.as_deref(), "three-factor methods")
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
enum FactorMethod {
    Totp,
    Passkey,
    SecurityKey,
    RecoveryCode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PagesPolicy {
    show: Vec<String>,
}

impl PagesPolicy {
    fn validate(&self) -> Result<()> {
        validate_unique_nonempty(Some(&self.show), "Shared Auth pages")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StylingPolicy {
    brand_name: String,
    primary_color: String,
}

impl StylingPolicy {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.brand_name.trim().is_empty() && self.brand_name.len() <= 80,
            "Shared Auth brand name must be nonempty and bounded"
        );
        ensure!(
            self.primary_color.len() == 7
                && self.primary_color.starts_with('#')
                && self.primary_color[1..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "Shared Auth primary color must be #RRGGBB"
        );
        Ok(())
    }
}

fn validate_unique_nonempty<T>(items: Option<&[T]>, label: &str) -> Result<()>
where
    T: Ord + std::fmt::Debug,
{
    let items = items.with_context(|| format!("{label} must be declared"))?;
    ensure!(!items.is_empty(), "{label} must not be empty");
    let unique = items.iter().collect::<BTreeSet<_>>();
    ensure!(
        unique.len() == items.len(),
        "{label} must not contain duplicates"
    );
    Ok(())
}

fn parse(text: &str) -> Result<()> {
    let policy: SharedAuthPolicy = toml::from_str(text).context("parsing Shared Auth policy")?;
    policy.validate()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn embedded_policy_is_admitted() {
        admit_embedded().unwrap();
    }

    #[test]
    fn repository_uses_only_the_requested_compatibility_alias() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(root.join(".auth-shared.toml").is_file());
        assert!(!root.join(".shared-auth.toml").exists());
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let text = POLICY.replace(
            "schema_version = 1",
            "schema_version = 1\nsecret = \"not-policy\"",
        );
        assert!(toml::from_str::<SharedAuthPolicy>(&text).is_err());
    }

    #[test]
    fn provenance_must_match_the_merged_shared_auth_authority() {
        let wrong_repo = POLICY.replace(INTERFACES_REPOSITORY, "https://github.com/example/other");
        assert!(parse(&wrong_repo).is_err());

        let wrong_revision = POLICY.replace(
            INTERFACES_REVISION,
            "0123456789abcdef0123456789abcdef01234567",
        );
        assert!(parse(&wrong_revision).is_err());

        let uppercase_revision = POLICY.replace(
            INTERFACES_REVISION,
            &INTERFACES_REVISION.to_ascii_uppercase(),
        );
        assert!(parse(&uppercase_revision).is_err());
    }

    #[test]
    fn factor_and_page_sets_are_unique_and_nonempty() {
        let duplicate_factor = POLICY.replace(
            "methods = [\"totp\", \"passkey\", \"security-key\"]",
            "methods = [\"totp\", \"totp\"]",
        );
        assert!(parse(&duplicate_factor).is_err());

        let empty_pages = POLICY.replace(
            "show = [\"sign-in\", \"sign-up\", \"challenge\", \"recovery\", \"consent\", \"error\", \"signed-out\"]",
            "show = []",
        );
        assert!(parse(&empty_pages).is_err());
    }

    #[test]
    fn local_policy_cannot_silently_disable_or_omit_required_two_factor_authentication() {
        let weakened = POLICY.replace("required = true", "required = false");
        assert!(parse(&weakened).is_err());

        let mut omitted: toml::Value = toml::from_str(POLICY).expect("parse embedded policy");
        omitted["factors"]["two_factor"]
            .as_table_mut()
            .expect("factors.two_factor table")
            .remove("required");
        let omitted = toml::to_string(&omitted).expect("serialize policy without required");
        assert!(parse(&omitted).is_err());

        let mut missing_methods: toml::Value =
            toml::from_str(POLICY).expect("parse embedded policy");
        missing_methods["factors"]["two_factor"]
            .as_table_mut()
            .expect("factors.two_factor table")
            .remove("methods");
        let missing_methods =
            toml::to_string(&missing_methods).expect("serialize policy without methods");
        assert!(parse(&missing_methods).is_err());
    }

    #[test]
    fn styling_is_bounded() {
        let bad_color = POLICY.replace("#4F46E5", "purple");
        assert!(parse(&bad_color).is_err());

        let empty_brand =
            POLICY.replace("brand_name = \"Zed Package Manager\"", "brand_name = \"\"");
        assert!(parse(&empty_brand).is_err());
    }
}
