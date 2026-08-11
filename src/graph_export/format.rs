use anyhow::{Result, bail};
use zed_interfaces::{
    DependencyGraphExportFormat as ExtendedGraphFormat,
    DependencyGraphFormat as CanonicalGraphFormat,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteKind {
    Canonical,
    Extended,
}

/// CLI routing wrapper around the two contract-owned format families.
///
/// The CLI owns only the choice of endpoint. Every representation descriptor
/// and accepted alias remains defined by `zed-interfaces`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GraphFormat {
    Canonical(CanonicalGraphFormat),
    Extended(ExtendedGraphFormat),
}

impl GraphFormat {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Canonical(format) => format.name(),
            Self::Extended(format) => format.name(),
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if let Some(format) = CanonicalGraphFormat::parse_name(trimmed) {
            return Ok(Self::Canonical(format));
        }
        if let Some(format) = ExtendedGraphFormat::parse_name(trimmed) {
            return Ok(Self::Extended(format));
        }

        let mut expected = CanonicalGraphFormat::ALL
            .into_iter()
            .map(CanonicalGraphFormat::name)
            .chain(
                ExtendedGraphFormat::ALL
                    .into_iter()
                    .map(ExtendedGraphFormat::name),
            )
            .collect::<Vec<_>>();
        let last = expected
            .pop()
            .expect("the interface contract always defines graph formats");
        let expected = format!("{}, or {last}", expected.join(", "));
        bail!("unsupported dependency graph format `{value}`; expected {expected}")
    }

    pub(super) const fn route_kind(self) -> RouteKind {
        match self {
            Self::Canonical(_) => RouteKind::Canonical,
            Self::Extended(_) => RouteKind::Extended,
        }
    }

    pub(super) const fn extension(self) -> &'static str {
        match self {
            Self::Canonical(format) => format.extension(),
            Self::Extended(format) => format.extension(),
        }
    }

    pub(super) const fn media_type(self) -> &'static str {
        match self {
            Self::Canonical(format) => format.media_type(),
            Self::Extended(format) => format.media_type(),
        }
    }

    pub(super) const fn authoritative(self) -> bool {
        match self {
            Self::Canonical(format) => format.is_authoritative(),
            Self::Extended(format) => format.is_authoritative(),
        }
    }

    pub(super) const fn binary(self) -> bool {
        match self {
            Self::Canonical(_) => false,
            Self::Extended(format) => format.is_binary(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_descriptors_and_aliases_are_contract_owned() {
        for shared in CanonicalGraphFormat::ALL {
            let cli = GraphFormat::Canonical(shared);
            assert_eq!(GraphFormat::parse(shared.name()).unwrap(), cli);
            for alias in shared.aliases() {
                assert_eq!(GraphFormat::parse(alias).unwrap(), cli);
            }
            assert_eq!(cli.name(), shared.name());
            assert_eq!(cli.extension(), shared.extension());
            assert_eq!(cli.media_type(), shared.media_type());
            assert_eq!(cli.authoritative(), shared.is_authoritative());
            assert!(!cli.binary());
            assert_eq!(cli.route_kind(), RouteKind::Canonical);
        }
    }

    #[test]
    fn extended_descriptors_and_aliases_are_contract_owned() {
        for shared in ExtendedGraphFormat::ALL {
            let cli = GraphFormat::Extended(shared);
            assert_eq!(GraphFormat::parse(shared.name()).unwrap(), cli);
            for alias in shared.aliases() {
                assert_eq!(GraphFormat::parse(alias).unwrap(), cli);
            }
            assert_eq!(cli.name(), shared.name());
            assert_eq!(cli.extension(), shared.extension());
            assert_eq!(cli.media_type(), shared.media_type());
            assert_eq!(cli.authoritative(), shared.is_authoritative());
            assert_eq!(cli.binary(), shared.is_binary());
            assert_eq!(cli.route_kind(), RouteKind::Extended);
        }
    }

    #[test]
    fn cli_parsing_remains_case_insensitive_and_rejects_unknown_formats() {
        assert_eq!(
            GraphFormat::parse(" YML ").unwrap(),
            GraphFormat::Canonical(CanonicalGraphFormat::Yaml)
        );
        assert_eq!(
            GraphFormat::parse("graphviz").unwrap(),
            GraphFormat::Canonical(CanonicalGraphFormat::Dot)
        );
        assert_eq!(
            GraphFormat::parse("messagepack").unwrap(),
            GraphFormat::Extended(ExtendedGraphFormat::MessagePack)
        );
        assert_eq!(
            GraphFormat::parse("PB").unwrap(),
            GraphFormat::Extended(ExtendedGraphFormat::Protobuf)
        );
        assert_eq!(
            GraphFormat::parse("pickle").unwrap_err().to_string(),
            "unsupported dependency graph format `pickle`; expected json, yaml, toml, dot, mermaid, json5, xml, csv, msgpack, or protobuf"
        );
    }
}
