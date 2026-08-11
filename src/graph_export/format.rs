use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteKind {
    Canonical,
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GraphFormat {
    Json,
    Yaml,
    Toml,
    Dot,
    Mermaid,
    Json5,
    Xml,
    Csv,
    MessagePack,
    Protobuf,
}

impl GraphFormat {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Dot => "dot",
            Self::Mermaid => "mermaid",
            Self::Json5 => "json5",
            Self::Xml => "xml",
            Self::Csv => "csv",
            Self::MessagePack => "msgpack",
            Self::Protobuf => "protobuf",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "dot" | "graphviz" => Self::Dot,
            "mermaid" | "mmd" => Self::Mermaid,
            "json5" => Self::Json5,
            "xml" => Self::Xml,
            "csv" => Self::Csv,
            "msgpack" | "messagepack" | "mpk" => Self::MessagePack,
            "protobuf" | "proto" | "pb" => Self::Protobuf,
            _ => bail!(
                "unsupported dependency graph format `{value}`; expected json, yaml, toml, dot, mermaid, json5, xml, csv, msgpack, or protobuf"
            ),
        })
    }

    pub(super) const fn route_kind(self) -> RouteKind {
        match self {
            Self::Json | Self::Yaml | Self::Toml | Self::Dot | Self::Mermaid => {
                RouteKind::Canonical
            }
            Self::Json5 | Self::Xml | Self::Csv | Self::MessagePack | Self::Protobuf => {
                RouteKind::Extended
            }
        }
    }

    pub(super) const fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Dot => "dot",
            Self::Mermaid => "mmd",
            Self::Json5 => "json5",
            Self::Xml => "xml",
            Self::Csv => "csv",
            Self::MessagePack => "msgpack",
            Self::Protobuf => "pb",
        }
    }

    pub(super) const fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/vnd.zpkg.dependency-graph.v1+json",
            Self::Yaml => "application/vnd.zpkg.dependency-graph.v1+yaml",
            Self::Toml => "application/vnd.zpkg.dependency-graph.v1+toml",
            Self::Dot => "text/vnd.graphviz; charset=utf-8",
            Self::Mermaid => "text/vnd.mermaid; charset=utf-8",
            Self::Json5 => "application/vnd.zpkg.dependency-graph.v1+json5",
            Self::Xml => "application/vnd.zpkg.dependency-graph.v1+xml",
            Self::Csv => "text/csv; charset=utf-8",
            Self::MessagePack => "application/vnd.zpkg.dependency-graph.v1+msgpack",
            Self::Protobuf => "application/vnd.zpkg.dependency-graph.v1+protobuf",
        }
    }

    pub(super) const fn authoritative(self) -> bool {
        matches!(
            self,
            Self::Json
                | Self::Yaml
                | Self::Toml
                | Self::Json5
                | Self::Xml
                | Self::MessagePack
                | Self::Protobuf
        )
    }

    pub(super) const fn binary(self) -> bool {
        matches!(self, Self::MessagePack | Self::Protobuf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_map_to_stable_names_and_semantics() {
        assert_eq!(GraphFormat::parse("YML").unwrap(), GraphFormat::Yaml);
        assert_eq!(GraphFormat::parse("graphviz").unwrap(), GraphFormat::Dot);
        assert_eq!(
            GraphFormat::parse("messagepack").unwrap(),
            GraphFormat::MessagePack
        );
        assert_eq!(GraphFormat::parse("PB").unwrap(), GraphFormat::Protobuf);
        assert!(!GraphFormat::Csv.authoritative());
        assert!(!GraphFormat::Dot.authoritative());
        assert!(GraphFormat::Xml.authoritative());
        assert!(GraphFormat::MessagePack.binary());
        assert!(GraphFormat::parse("pickle").is_err());
    }
}
