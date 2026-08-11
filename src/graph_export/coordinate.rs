use anyhow::{Result, ensure};

use super::format::GraphFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PackageCoordinate {
    pub(super) org: String,
    pub(super) name: String,
    pub(super) version: String,
}

impl PackageCoordinate {
    pub(super) fn parse(value: &str) -> Result<Self> {
        let Some((package, version)) = value.rsplit_once('@') else {
            anyhow::bail!("package coordinate `{value}` must include an exact @version");
        };
        let mut package_parts = package.split('/');
        let org = package_parts.next().unwrap_or_default();
        let name = package_parts.next().unwrap_or_default();
        ensure!(
            package_parts.next().is_none(),
            "package coordinate `{value}` must be exactly org/name@version"
        );
        ensure!(
            zed_interfaces::manifest::is_slug(org),
            "package organization `{org}` is not a valid lowercase slug"
        );
        ensure!(
            zed_interfaces::manifest::is_slug(name),
            "package name `{name}` is not a valid lowercase slug"
        );
        validate_version_segment(version)?;
        Ok(Self {
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    pub(super) fn display(&self) -> String {
        format!("{}/{}@{}", self.org, self.name, self.version)
    }

    pub(super) fn suggested_filename(&self, format: GraphFormat) -> String {
        let safe_version: String = self
            .version
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '+' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        format!(
            "{}_{}_{}.dependency-graph.{}",
            self.org,
            self.name,
            safe_version,
            format.extension()
        )
    }
}

fn validate_version_segment(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "package version may not be empty");
    ensure!(
        value != "." && value != "..",
        "package version may not be a dot segment"
    );
    ensure!(
        !value.chars().any(|character| {
            character == '/' || character == '\\' || character.is_control()
        }),
        "package version contains a path separator or control character"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_coordinate_parser_accepts_prereleases_and_rejects_requirements() {
        let coordinate = PackageCoordinate::parse("acme/http-kit@2.0.0-beta.1+build.7").unwrap();
        assert_eq!(coordinate.org, "acme");
        assert_eq!(coordinate.name, "http-kit");
        assert_eq!(coordinate.version, "2.0.0-beta.1+build.7");

        assert!(PackageCoordinate::parse("acme/http-kit").is_err());
        assert!(PackageCoordinate::parse("acme/http-kit@../secret").is_err());
        assert!(PackageCoordinate::parse("acme/nested/name@1.0.0").is_err());
        assert!(PackageCoordinate::parse("Acme/http-kit@1.0.0").is_err());
    }

    #[test]
    fn suggested_filenames_are_deterministic_and_binary_safe() {
        let coordinate = PackageCoordinate::parse("acme/http-kit@2.0.0-beta.1+build.7").unwrap();
        assert_eq!(
            coordinate.suggested_filename(GraphFormat::Protobuf),
            "acme_http-kit_2.0.0-beta.1+build.7.dependency-graph.pb"
        );
    }
}
