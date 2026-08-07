//! Strict, lossless identity model for the current project-local `mise.lock`
//! format.
//!
//! The existing environment importer intentionally consumes a smaller common
//! subset. This module owns the complete lock identity required for faithful
//! round trips, frozen verification, and future translation into Zed's native
//! `EnvironmentLock` without executing mise or project code.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Validation boundary for a parsed mise lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiseLockValidationMode {
    /// Preserve and validate current manager state without claiming that every
    /// artifact can be restored on another machine.
    Authoring,
    /// Require cryptographically identified, network-portable artifacts and
    /// reject source-build-only entries.
    FrozenPortable,
}

/// Complete current project lock state. The generated comment header is not
/// semantic TOML and is intentionally not represented here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseLockDocument {
    #[serde(default)]
    pub tools: BTreeMap<String, Vec<MiseLockedTool>>,

    #[serde(default, rename = "conda-packages")]
    pub conda_packages: BTreeMap<String, BTreeMap<String, MiseCondaPackage>>,

    #[serde(default, rename = "pkgx-packages")]
    pub pkgx_packages: BTreeMap<String, BTreeMap<String, MisePkgxPackage>>,
}

/// One exact version/backend/options identity for a logical tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseLockedTool {
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platforms: BTreeMap<String, MisePlatformInfo>,
}

/// mise accepts a legacy compact checksum string or the current platform table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MisePlatformInfo {
    Checksum(String),
    Detail(Box<MisePlatformDetails>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MisePlatformDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_api: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conda_deps: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgx_deps: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgx_provides: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgx_runtime_env: Option<BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MiseProvenance>,

    #[serde(default, skip_serializing_if = "is_false")]
    pub provenance_verified: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_attestations: Option<MiseGithubAttestationsStatus>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_artifacts: Vec<MiseArtifactInfo>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Ordered secondary release artifact extracted into the primary install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseArtifactInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,

    pub url: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_api: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MiseProvenance>,

    #[serde(default, skip_serializing_if = "is_false")]
    pub provenance_verified: bool,
}

/// Current mise provenance representation: a named verifier or a SLSA table
/// carrying the provenance URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MiseProvenance {
    Named(MiseProvenanceName),
    Slsa(MiseSlsaProvenance),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MiseProvenanceName {
    Minisign,
    Cosign,
    Slsa,
    GithubAttestations,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseSlsaProvenance {
    pub slsa: MiseSlsaDetails,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseSlsaDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MiseGithubAttestationsStatus {
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiseCondaPackage {
    pub url: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MisePkgxPackage {
    pub url: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgx_provides: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgx_runtime_env: Option<BTreeMap<String, String>>,
}

fn normalize_current_wire_platform_keys(value: &mut toml::Value, path: &str) -> Result<()> {
    let root = value
        .as_table_mut()
        .with_context(|| format!("current mise lock `{path}` must be a TOML table"))?;
    let Some(tools_value) = root.get_mut("tools") else {
        return Ok(());
    };
    let tools = tools_value
        .as_table_mut()
        .with_context(|| format!("`tools` in current mise lock `{path}` must be a table"))?;

    for (tool_name, value) in tools {
        match value {
            toml::Value::Array(entries) => {
                for (index, entry) in entries.iter_mut().enumerate() {
                    let table = entry.as_table_mut().with_context(|| {
                        format!(
                            "`tools.{tool_name}[{index}]` in current mise lock `{path}` must be a table"
                        )
                    })?;
                    unflatten_platform_keys(table, tool_name, index, path)?;
                }
            }
            toml::Value::Table(table) => {
                unflatten_platform_keys(table, tool_name, 0, path)?;
            }
            _ => bail!(
                "`tools.{tool_name}` in current mise lock `{path}` must be a table or table array"
            ),
        }
    }
    Ok(())
}

fn unflatten_platform_keys(
    table: &mut toml::map::Map<String, toml::Value>,
    tool_name: &str,
    index: usize,
    path: &str,
) -> Result<()> {
    let keys = table
        .keys()
        .filter(|key| key.starts_with("platforms."))
        .cloned()
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Ok(());
    }
    ensure!(
        !table.contains_key("platforms"),
        "`tools.{tool_name}[{index}]` in current mise lock `{path}` mixes nested `platforms` with current quoted `platforms.<target>` keys"
    );

    let mut platforms = toml::map::Map::new();
    for key in keys {
        let platform = key.strip_prefix("platforms.").unwrap_or_default();
        ensure!(
            !platform.is_empty(),
            "`tools.{tool_name}[{index}]` in current mise lock `{path}` has an empty platform key"
        );
        let metadata = table.remove(&key).with_context(|| {
            format!("failed to extract `{key}` from `tools.{tool_name}[{index}]`")
        })?;
        ensure!(
            platforms.insert(platform.to_string(), metadata).is_none(),
            "duplicate current platform identity `{platform}` in `tools.{tool_name}[{index}]` of `{path}`"
        );
    }
    table.insert("platforms".to_string(), toml::Value::Table(platforms));
    Ok(())
}

fn flatten_current_wire_platform_keys(value: &mut toml::Value, path: &str) -> Result<()> {
    let root = value
        .as_table_mut()
        .with_context(|| format!("normalized current mise lock `{path}` must be a TOML table"))?;
    let Some(tools_value) = root.get_mut("tools") else {
        return Ok(());
    };
    let tools = tools_value.as_table_mut().with_context(|| {
        format!("`tools` in normalized current mise lock `{path}` must be a table")
    })?;

    for (tool_name, value) in tools {
        match value {
            toml::Value::Array(entries) => {
                for (index, entry) in entries.iter_mut().enumerate() {
                    let table = entry.as_table_mut().with_context(|| {
                        format!(
                            "`tools.{tool_name}[{index}]` in normalized current mise lock `{path}` must be a table"
                        )
                    })?;
                    flatten_platform_keys(table, tool_name, index, path)?;
                }
            }
            toml::Value::Table(table) => {
                flatten_platform_keys(table, tool_name, 0, path)?;
            }
            _ => bail!(
                "`tools.{tool_name}` in normalized current mise lock `{path}` must be a table or table array"
            ),
        }
    }
    Ok(())
}

fn flatten_platform_keys(
    table: &mut toml::map::Map<String, toml::Value>,
    tool_name: &str,
    index: usize,
    path: &str,
) -> Result<()> {
    let Some(platforms_value) = table.remove("platforms") else {
        return Ok(());
    };
    let platforms = platforms_value.as_table().with_context(|| {
        format!(
            "`tools.{tool_name}[{index}].platforms` in normalized current mise lock `{path}` must be a table"
        )
    })?;
    let entries = platforms
        .iter()
        .map(|(platform, metadata)| (platform.clone(), metadata.clone()))
        .collect::<Vec<_>>();
    for (platform, metadata) in entries {
        let key = format!("platforms.{platform}");
        ensure!(
            table.insert(key.clone(), metadata).is_none(),
            "duplicate flattened current platform key `{key}` in `tools.{tool_name}[{index}]` of `{path}`"
        );
    }
    Ok(())
}

impl MiseLockDocument {
    /// Parse and validate a complete current mise lock without invoking mise.
    pub fn parse(input: &str, path: &str, mode: MiseLockValidationMode) -> Result<Self> {
        let mut value: toml::Value = toml::from_str(input)
            .with_context(|| format!("failed to parse current mise lock `{path}`"))?;
        normalize_current_wire_platform_keys(&mut value, path)?;
        let document: Self = value
            .try_into()
            .with_context(|| format!("failed to decode current mise lock `{path}`"))?;
        document.validate(path, mode)?;
        Ok(document)
    }

    /// Validate identities, provenance relationships, shared package
    /// references, URLs, and the selected portability boundary.
    pub fn validate(&self, path: &str, mode: MiseLockValidationMode) -> Result<()> {
        for (tool_name, identities) in &self.tools {
            validate_text(&format!("tools.{tool_name}"), tool_name)?;
            ensure!(
                !identities.is_empty(),
                "`tools.{tool_name}` in `{path}` must contain at least one identity"
            );

            let mut identity_keys = BTreeSet::new();
            for (index, identity) in identities.iter().enumerate() {
                let field = format!("tools.{tool_name}[{index}]");
                identity.validate(self, path, &field, mode)?;
                let key = identity.identity_key()?;
                ensure!(
                    identity_keys.insert(key),
                    "duplicate option-dependent identity `{field}` in `{path}`"
                );
            }
        }

        for (platform, packages) in &self.conda_packages {
            validate_text(&format!("conda-packages.{platform}"), platform)?;
            for (basename, package) in packages {
                let field = format!("conda-packages.{platform}.{basename}");
                validate_text(&field, basename)?;
                validate_network_url(&format!("{field}.url"), &package.url)?;
                validate_optional_checksum(
                    &format!("{field}.checksum"),
                    package.checksum.as_deref(),
                    mode,
                )?;
            }
        }

        for (platform, packages) in &self.pkgx_packages {
            validate_text(&format!("pkgx-packages.{platform}"), platform)?;
            for (id, package) in packages {
                let field = format!("pkgx-packages.{platform}.{id}");
                validate_text(&field, id)?;
                validate_network_url(&format!("{field}.url"), &package.url)?;
                validate_optional_checksum(
                    &format!("{field}.checksum"),
                    package.checksum.as_deref(),
                    mode,
                )?;
                validate_optional_strings(
                    &format!("{field}.pkgx_provides"),
                    package.pkgx_provides.as_deref(),
                )?;
                validate_runtime_env(
                    &format!("{field}.pkgx_runtime_env"),
                    package.pkgx_runtime_env.as_ref(),
                )?;
            }
        }
        Ok(())
    }

    /// Presentation-independent clone. Tool identity and additional-artifact
    /// order remain semantic; only set-like package lists are sorted.
    pub fn normalized(&self) -> Self {
        let mut normalized = self.clone();

        for identities in normalized.tools.values_mut() {
            for identity in identities.iter_mut() {
                identity.normalize();
            }
            // mise writes each Vec<LockfileTool> in stored order and
            // multi-version PATH/default selection is order-sensitive.
        }

        for packages in normalized.conda_packages.values_mut() {
            for package in packages.values_mut() {
                normalize_optional_checksum(&mut package.checksum);
            }
        }

        for packages in normalized.pkgx_packages.values_mut() {
            for package in packages.values_mut() {
                normalize_optional_checksum(&mut package.checksum);
                normalize_optional_strings(&mut package.pkgx_provides);
            }
        }

        normalized
    }

    /// Compact canonical JSON used as the semantic lock identity.
    pub fn canonical_json_bytes(&self) -> Result<Vec<u8>> {
        self.validate("<normalized mise lock>", MiseLockValidationMode::Authoring)?;
        serde_json::to_vec(&self.normalized())
            .context("failed to serialize normalized current mise lock")
    }

    /// Domain-separated SHA-256 over canonical JSON.
    pub fn semantic_digest_sha256(&self) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(b"zed-pkg:mise-lock-identity:v1\0");
        hasher.update(self.canonical_json_bytes()?);
        Ok(hex::encode(hasher.finalize()))
    }

    /// Deterministic TOML suitable for explicit export and round-trip tests.
    pub fn to_toml_string(&self) -> Result<String> {
        self.validate("<normalized mise lock>", MiseLockValidationMode::Authoring)?;
        let nested = toml::to_string(&self.normalized())
            .context("failed to stage normalized current mise lock as TOML")?;
        let mut value: toml::Value = toml::from_str(&nested)
            .context("failed to stage normalized current mise lock as a TOML value")?;
        flatten_current_wire_platform_keys(&mut value, "<normalized mise lock>")?;
        toml::to_string_pretty(&value)
            .context("failed to serialize normalized current mise lock as TOML")
    }
}

impl MiseLockedTool {
    fn validate(
        &self,
        document: &MiseLockDocument,
        path: &str,
        field: &str,
        mode: MiseLockValidationMode,
    ) -> Result<()> {
        validate_text(&format!("{field}.version"), &self.version)?;
        ensure!(
            !looks_moving(&self.version),
            "`{field}.version` in `{path}` must be an exact non-moving identity, got `{}`",
            self.version
        );

        if let Some(backend) = &self.backend {
            validate_text(&format!("{field}.backend"), backend)?;
        }
        for (key, value) in &self.options {
            validate_text(&format!("{field}.options.{key}"), key)?;
            validate_control_free(&format!("{field}.options.{key}"), value)?;
        }

        if mode == MiseLockValidationMode::FrozenPortable {
            ensure!(
                !self.platforms.is_empty(),
                "`{field}.platforms` in `{path}` must contain a portable artifact"
            );
        }

        for (platform, info) in &self.platforms {
            let platform_field = format!("{field}.platforms.{platform}");
            validate_text(&platform_field, platform)?;
            info.validate(document, path, &platform_field, platform, mode)?;
        }
        Ok(())
    }

    fn identity_key(&self) -> Result<String> {
        serde_json::to_string(&(
            self.version.as_str(),
            self.backend.as_deref(),
            &self.options,
        ))
        .context("failed to serialize mise tool identity key")
    }

    fn normalize(&mut self) {
        for info in self.platforms.values_mut() {
            info.normalize();
        }
    }
}

impl MisePlatformInfo {
    fn validate(
        &self,
        document: &MiseLockDocument,
        path: &str,
        field: &str,
        platform: &str,
        mode: MiseLockValidationMode,
    ) -> Result<()> {
        match self {
            Self::Checksum(checksum) => validate_checksum(field, checksum),
            Self::Detail(details) => details.validate(document, path, field, platform, mode),
        }
    }

    fn normalize(&mut self) {
        match self {
            Self::Checksum(checksum) => {
                let checksum = normalize_checksum(checksum);
                *self = Self::Detail(Box::new(MisePlatformDetails {
                    checksum: Some(checksum),
                    ..MisePlatformDetails::default()
                }));
            }
            Self::Detail(details) => details.normalize(),
        }
    }
}

impl MisePlatformDetails {
    fn validate(
        &self,
        document: &MiseLockDocument,
        path: &str,
        field: &str,
        platform: &str,
        mode: MiseLockValidationMode,
    ) -> Result<()> {
        if let Some(install) = &self.install {
            ensure!(
                install == "source",
                "`{field}.install` in `{path}` only supports the current mise value `source`, got `{install}`"
            );
            ensure!(
                mode != MiseLockValidationMode::FrozenPortable,
                "`{field}` in `{path}` is a source-build-only identity and is not frozen-portable"
            );
        }

        validate_optional_checksum(&format!("{field}.checksum"), self.checksum.as_deref(), mode)?;
        validate_optional_size(&format!("{field}.size"), self.size)?;
        validate_optional_url(&format!("{field}.url"), self.url.as_deref())?;
        validate_optional_url(&format!("{field}.url_api"), self.url_api.as_deref())?;
        validate_provenance(
            field,
            self.provenance.as_ref(),
            self.provenance_verified,
            self.github_attestations,
        )?;

        if mode == MiseLockValidationMode::FrozenPortable {
            ensure!(
                self.checksum.is_some(),
                "`{field}.checksum` in `{path}` is required for frozen-portable replay"
            );
        }

        validate_optional_strings(&format!("{field}.conda_deps"), self.conda_deps.as_deref())?;
        validate_optional_strings(&format!("{field}.pkgx_deps"), self.pkgx_deps.as_deref())?;
        validate_optional_strings(
            &format!("{field}.pkgx_provides"),
            self.pkgx_provides.as_deref(),
        )?;
        validate_runtime_env(
            &format!("{field}.pkgx_runtime_env"),
            self.pkgx_runtime_env.as_ref(),
        )?;

        if let Some(dependencies) = &self.conda_deps {
            let packages = document.conda_packages.get(platform).with_context(|| {
                format!(
                    "`{field}.conda_deps` in `{path}` references platform `{platform}` with no shared conda-packages section"
                )
            })?;
            for dependency in dependencies {
                ensure!(
                    packages.contains_key(dependency),
                    "`{field}.conda_deps` in `{path}` references missing conda package `{dependency}` on `{platform}`"
                );
            }
        }

        if let Some(dependencies) = &self.pkgx_deps {
            let packages = document.pkgx_packages.get(platform).with_context(|| {
                format!(
                    "`{field}.pkgx_deps` in `{path}` references platform `{platform}` with no shared pkgx-packages section"
                )
            })?;
            for dependency in dependencies {
                ensure!(
                    packages.contains_key(dependency),
                    "`{field}.pkgx_deps` in `{path}` references missing pkgx package `{dependency}` on `{platform}`"
                );
            }
        }

        let mut artifact_urls = BTreeSet::new();
        for (index, artifact) in self.additional_artifacts.iter().enumerate() {
            let artifact_field = format!("{field}.additional_artifacts[{index}]");
            artifact.validate(&artifact_field, mode)?;
            ensure!(
                artifact_urls.insert(artifact.url.as_str()),
                "duplicate additional artifact URL `{}` in `{field}`",
                artifact.url
            );
        }
        Ok(())
    }

    fn normalize(&mut self) {
        normalize_optional_checksum(&mut self.checksum);
        normalize_optional_strings(&mut self.conda_deps);
        normalize_optional_strings(&mut self.pkgx_deps);
        normalize_optional_strings(&mut self.pkgx_provides);
        for artifact in &mut self.additional_artifacts {
            artifact.normalize();
        }
    }
}

impl MiseArtifactInfo {
    fn validate(&self, field: &str, mode: MiseLockValidationMode) -> Result<()> {
        validate_network_url(&format!("{field}.url"), &self.url)?;
        validate_optional_url(&format!("{field}.url_api"), self.url_api.as_deref())?;
        validate_optional_checksum(&format!("{field}.checksum"), self.checksum.as_deref(), mode)?;
        validate_optional_size(&format!("{field}.size"), self.size)?;
        validate_provenance(
            field,
            self.provenance.as_ref(),
            self.provenance_verified,
            None,
        )?;
        if mode == MiseLockValidationMode::FrozenPortable {
            ensure!(
                self.checksum.is_some(),
                "`{field}.checksum` is required for frozen-portable replay"
            );
        }
        Ok(())
    }

    fn normalize(&mut self) {
        normalize_optional_checksum(&mut self.checksum);
    }
}

fn validate_provenance(
    field: &str,
    provenance: Option<&MiseProvenance>,
    provenance_verified: bool,
    github_attestations: Option<MiseGithubAttestationsStatus>,
) -> Result<()> {
    ensure!(
        !provenance_verified || provenance.is_some(),
        "`{field}.provenance_verified = true` requires `{field}.provenance`"
    );
    ensure!(
        provenance.is_none() || github_attestations.is_none(),
        "`{field}` cannot record verified provenance and an unavailable GitHub attestation simultaneously"
    );
    if let Some(MiseProvenance::Slsa(slsa)) = provenance
        && let Some(url) = &slsa.slsa.url
    {
        validate_network_url(&format!("{field}.provenance.slsa.url"), url)?;
    }
    Ok(())
}

fn validate_optional_checksum(
    field: &str,
    checksum: Option<&str>,
    mode: MiseLockValidationMode,
) -> Result<()> {
    if let Some(checksum) = checksum {
        validate_checksum(field, checksum)?;
    } else if mode == MiseLockValidationMode::FrozenPortable {
        bail!("`{field}` is required for frozen-portable replay");
    }
    Ok(())
}

fn validate_checksum(field: &str, checksum: &str) -> Result<()> {
    ensure!(
        checksum == checksum.trim() && !checksum.chars().any(char::is_control),
        "`{field}` must be trimmed and contain no control characters"
    );
    let (algorithm, digest) = checksum
        .split_once(':')
        .with_context(|| format!("`{field}` must use algorithm:digest syntax"))?;
    let expected = match algorithm.to_ascii_lowercase().as_str() {
        "sha256" | "blake3" => 64,
        "sha512" => 128,
        other => bail!("`{field}` uses unsupported checksum algorithm `{other}`"),
    };
    ensure!(
        digest.len() == expected && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "`{field}` must contain {expected} hexadecimal digest characters"
    );
    Ok(())
}

fn validate_optional_size(field: &str, size: Option<u64>) -> Result<()> {
    if let Some(size) = size {
        ensure!(size > 0, "`{field}` must be greater than zero");
    }
    Ok(())
}

fn validate_optional_url(field: &str, url: Option<&str>) -> Result<()> {
    if let Some(url) = url {
        validate_network_url(field, url)?;
    }
    Ok(())
}

fn validate_network_url(field: &str, value: &str) -> Result<()> {
    validate_text(field, value)?;
    ensure!(
        value.starts_with("https://") || value.starts_with("http://"),
        "`{field}` must use an exact http:// or https:// network URL"
    );
    let parsed = Url::parse(value).with_context(|| format!("`{field}` is not a valid URL"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "`{field}` must use http or https, got `{}`",
        parsed.scheme()
    );
    ensure!(
        parsed.host_str().is_some(),
        "`{field}` must contain a network host"
    );
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "`{field}` must not embed URL credentials"
    );
    ensure!(
        parsed.fragment().is_none(),
        "`{field}` must not contain a URL fragment"
    );

    const SECRET_QUERY_KEYS: &[&str] = &[
        "access_token",
        "api_key",
        "apikey",
        "auth",
        "authorization",
        "key",
        "secret",
        "sig",
        "signature",
        "token",
        "x-amz-credential",
        "x-amz-signature",
        "x-amz-security-token",
        "x-goog-credential",
        "x-goog-signature",
    ];
    for (key, _) in parsed.query_pairs() {
        let key = key.to_ascii_lowercase();
        ensure!(
            !SECRET_QUERY_KEYS.contains(&key.as_str()),
            "`{field}` contains secret-bearing query parameter `{key}`"
        );
    }
    Ok(())
}

fn validate_optional_strings(field: &str, values: Option<&[String]>) -> Result<()> {
    if let Some(values) = values {
        let mut seen = BTreeSet::new();
        for (index, value) in values.iter().enumerate() {
            validate_text(&format!("{field}[{index}]"), value)?;
            ensure!(
                seen.insert(value),
                "`{field}` contains duplicate value `{value}`"
            );
        }
    }
    Ok(())
}

fn validate_runtime_env(field: &str, values: Option<&BTreeMap<String, String>>) -> Result<()> {
    if let Some(values) = values {
        for (key, value) in values {
            ensure!(
                is_environment_key(key),
                "`{field}` contains non-portable environment key `{key}`"
            );
            validate_control_free(&format!("{field}.{key}"), value)?;
        }
    }
    Ok(())
}

fn is_environment_key(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn validate_text(field: &str, value: &str) -> Result<()> {
    ensure!(
        value == value.trim() && !value.is_empty(),
        "`{field}` must be non-empty and trimmed"
    );
    validate_control_free(field, value)
}

fn validate_control_free(field: &str, value: &str) -> Result<()> {
    ensure!(
        !value.chars().any(char::is_control),
        "`{field}` must contain no control characters"
    );
    Ok(())
}

fn looks_moving(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    matches!(
        value.as_str(),
        "latest"
            | "stable"
            | "current"
            | "system"
            | "present"
            | "head"
            | "main"
            | "master"
            | "nightly"
            | "canary"
            | "beta"
            | "alpha"
            | "lts"
    ) || value.starts_with("ref:main")
        || value.starts_with("ref:master")
        || value.starts_with("lts/")
        || value.starts_with("prefix:")
        || value.starts_with("path:")
        || value.starts_with("env:")
        || value.contains('*')
        || value.ends_with(".x")
}

fn normalize_checksum(value: &str) -> String {
    match value.split_once(':') {
        Some((algorithm, digest)) => {
            format!(
                "{}:{}",
                algorithm.to_ascii_lowercase(),
                digest.to_ascii_lowercase()
            )
        }
        None => value.to_string(),
    }
}

fn normalize_optional_checksum(value: &mut Option<String>) {
    if let Some(value) = value {
        *value = normalize_checksum(value);
    }
}

fn normalize_optional_strings(values: &mut Option<Vec<String>>) {
    if let Some(values) = values {
        values.sort();
        values.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn full_lock() -> String {
        format!(
            r#"
[conda-packages.linux-x64."ncurses-6.4-h7ea286d_0"]
url = "https://repo.example.test/conda/ncurses.tar.bz2"
checksum = "sha256:{A}"

[pkgx-packages.linux-x64."zlib.net@1.3.1"]
url = "https://repo.example.test/pkgx/zlib.tar.gz"
checksum = "sha256:{B}"
pkgx_provides = ["zlib-flate"]
pkgx_runtime_env = {{ ZLIB_HOME = "lib/zlib" }}

[[tools.node]]
version = "22.4.0"
backend = "core:node"
options = {{ flavor = "full" }}

[tools.node.platforms.linux-x64]
checksum = "sha256:{A}"
size = 42
url = "https://downloads.example.test/node.tar.gz?channel=release"
url_api = "https://api.example.test/releases/node"
conda_deps = ["ncurses-6.4-h7ea286d_0"]
pkgx_deps = ["zlib.net@1.3.1"]
pkgx_provides = ["node", "npm"]
pkgx_runtime_env = {{ NODE_HOME = "lib/node" }}
provenance = {{ slsa = {{ url = "https://attest.example.test/node.intoto.jsonl" }} }}
provenance_verified = true
additional_artifacts = [
  {{ url = "https://downloads.example.test/npm.tar.gz", checksum = "sha256:{B}", size = 7, provenance = "minisign", provenance_verified = true }},
  {{ url = "https://downloads.example.test/corepack.tar.gz", checksum = "blake3:{C}" }}
]

[tools.node.platforms.macos-arm64]
checksum = "sha512:{A}{B}"
url = "https://downloads.example.test/node-darwin.tar.gz"
github_attestations = "unavailable"
"#
        )
    }

    #[test]
    fn platform_info_enum_remains_indirect_and_bounded() {
        assert!(std::mem::size_of::<MisePlatformInfo>() <= 64);
    }

    #[test]
    fn complete_current_lock_round_trips_and_is_frozen_portable() {
        let lock = MiseLockDocument::parse(
            &full_lock(),
            "mise.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        let output = lock.to_toml_string().unwrap();
        let reparsed = MiseLockDocument::parse(
            &output,
            "normalized-mise.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        assert_eq!(lock.normalized(), reparsed.normalized());
        assert_eq!(
            lock.semantic_digest_sha256().unwrap(),
            reparsed.semantic_digest_sha256().unwrap()
        );
    }

    #[test]
    fn compact_checksum_and_table_forms_have_the_same_identity() {
        let compact = format!(
            "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms]\nlinux-x64 = \"sha256:{A}\"\n"
        );
        let table = format!(
            "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\n"
        );
        let compact = MiseLockDocument::parse(
            &compact,
            "compact.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        let table =
            MiseLockDocument::parse(&table, "table.lock", MiseLockValidationMode::FrozenPortable)
                .unwrap();
        assert_eq!(
            compact.semantic_digest_sha256().unwrap(),
            table.semantic_digest_sha256().unwrap()
        );
    }

    #[test]
    fn unknown_current_lock_fields_fail_closed() {
        let lock = format!(
            "[[tools.node]]\nversion = \"22.4.0\"\nunknown = true\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\n"
        );
        let error = MiseLockDocument::parse(&lock, "mise.lock", MiseLockValidationMode::Authoring)
            .unwrap_err();
        assert!(format!("{error:#}").contains("unknown"));
    }

    #[test]
    fn secret_bearing_urls_fail_closed() {
        for url in [
            "https://user:password@example.test/tool.tar.gz",
            "https://example.test/tool.tar.gz?token=secret",
            "https://example.test/tool.tar.gz?X-Amz-Security-Token=secret",
            "https://example.test/tool.tar.gz#fragment",
            "https:/missing-host/tool.tar.gz",
        ] {
            let lock = format!(
                "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\nurl = \"{url}\"\n"
            );
            assert!(
                MiseLockDocument::parse(
                    &lock,
                    "mise.lock",
                    MiseLockValidationMode::FrozenPortable,
                )
                .is_err(),
                "URL should fail: {url}"
            );
        }
    }

    #[test]
    fn frozen_portable_requires_checksums_and_rejects_source_installs() {
        let missing = "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nurl = \"https://example.test/node.tar.gz\"\n";
        assert!(
            MiseLockDocument::parse(missing, "mise.lock", MiseLockValidationMode::FrozenPortable,)
                .is_err()
        );

        let source = "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\ninstall = \"source\"\n";
        MiseLockDocument::parse(source, "mise.lock", MiseLockValidationMode::Authoring).unwrap();
        assert!(
            MiseLockDocument::parse(source, "mise.lock", MiseLockValidationMode::FrozenPortable,)
                .is_err()
        );
    }

    #[test]
    fn shared_package_references_must_resolve_on_the_exact_platform() {
        let lock = format!(
            "[[tools.python]]\nversion = \"3.12.4\"\n[tools.python.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\nconda_deps = [\"missing-package\"]\n"
        );
        let error =
            MiseLockDocument::parse(&lock, "mise.lock", MiseLockValidationMode::FrozenPortable)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no shared conda-packages section")
        );
    }

    #[test]
    fn duplicate_option_dependent_identity_is_rejected() {
        let lock = format!(
            "[[tools.node]]\nversion = \"22.4.0\"\noptions = {{ flavor = \"full\" }}\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\n[[tools.node]]\nversion = \"22.4.0\"\noptions = {{ flavor = \"full\" }}\n[tools.node.platforms.macos-arm64]\nchecksum = \"sha256:{B}\"\n"
        );
        let error =
            MiseLockDocument::parse(&lock, "mise.lock", MiseLockValidationMode::FrozenPortable)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate option-dependent identity")
        );
    }

    #[test]
    fn tool_identity_order_is_semantic() {
        let first = format!(
            "[[tools.node]]\nversion = \"20.15.1\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\n[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{B}\"\n"
        );
        let second = format!(
            "[[tools.node]]\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{B}\"\n[[tools.node]]\nversion = \"20.15.1\"\n[tools.node.platforms.linux-x64]\nchecksum = \"sha256:{A}\"\n"
        );
        let first =
            MiseLockDocument::parse(&first, "first.lock", MiseLockValidationMode::FrozenPortable)
                .unwrap();
        let second = MiseLockDocument::parse(
            &second,
            "second.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        assert_ne!(
            first.semantic_digest_sha256().unwrap(),
            second.semantic_digest_sha256().unwrap()
        );
    }

    #[test]
    fn ordered_additional_artifacts_change_semantic_identity() {
        let first = MiseLockDocument::parse(
            &full_lock(),
            "first.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        let mut second = first.clone();
        let MisePlatformInfo::Detail(platform) = second.tools.get_mut("node").unwrap()[0]
            .platforms
            .get_mut("linux-x64")
            .unwrap()
        else {
            panic!("expected detailed platform")
        };
        platform.additional_artifacts.reverse();
        assert_ne!(
            first.semantic_digest_sha256().unwrap(),
            second.semantic_digest_sha256().unwrap()
        );
    }

    #[test]
    fn semantic_digest_normalizes_checksum_case_and_set_like_lists() {
        let first = MiseLockDocument::parse(
            &full_lock(),
            "first.lock",
            MiseLockValidationMode::FrozenPortable,
        )
        .unwrap();
        let mut second = first.clone();
        let MisePlatformInfo::Detail(platform) = second.tools.get_mut("node").unwrap()[0]
            .platforms
            .get_mut("linux-x64")
            .unwrap()
        else {
            panic!("expected detailed platform")
        };
        platform.checksum = Some(format!("SHA256:{}", A.to_ascii_uppercase()));
        platform.pkgx_provides.as_mut().unwrap().reverse();
        assert_eq!(
            first.semantic_digest_sha256().unwrap(),
            second.semantic_digest_sha256().unwrap()
        );
    }
}
