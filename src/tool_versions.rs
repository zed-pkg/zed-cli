//! Lossless, line-aware parsing for project-local `.tool-versions` files.
//!
//! This module intentionally does not decide precedence between `.tool-versions`,
//! mise TOML files, idiomatic version files, or user-global configuration. It
//! preserves source order and opaque tokens so the adapter layer can implement
//! and certify those policies without the parser silently changing semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A parsed `.tool-versions` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersionsDocument {
    source: String,
    lines: Vec<ToolVersionsLine>,
}

impl ToolVersionsDocument {
    /// Parse a complete `.tool-versions` source document.
    pub fn parse(source: impl Into<String>) -> Result<Self, ToolVersionsParseError> {
        let source = source.into();
        let mut lines = Vec::new();

        for (index, segment) in source.split_inclusive('\n').enumerate() {
            let without_lf = segment.strip_suffix('\n').unwrap_or(segment);
            let without_eol = without_lf.strip_suffix('\r').unwrap_or(without_lf);
            let logical = if index == 0 {
                without_eol.strip_prefix('\u{feff}').unwrap_or(without_eol)
            } else {
                without_eol
            };
            lines.push(parse_line(logical, index + 1)?);
        }

        Ok(Self { source, lines })
    }

    /// Read and parse one file while retaining its path in diagnostics.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, ToolVersionsReadError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| ToolVersionsReadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(source).map_err(|source| ToolVersionsReadError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Exact bytes decoded as UTF-8. A no-op import/export can write this value
    /// back verbatim, including comments, whitespace, BOM, and line endings.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Parsed logical lines in original source order.
    pub fn lines(&self) -> &[ToolVersionsLine] {
        &self.lines
    }

    /// Tool declarations in original source order. Duplicate tool declarations
    /// are intentionally preserved.
    pub fn entries(&self) -> impl Iterator<Item = &ToolVersionsEntry> {
        self.lines.iter().filter_map(ToolVersionsLine::entry)
    }

    /// Return all declarations for one logical tool in source order.
    pub fn entries_for<'a>(
        &'a self,
        tool: &'a str,
    ) -> impl Iterator<Item = &'a ToolVersionsEntry> + 'a {
        self.entries().filter(move |entry| entry.tool == tool)
    }

    /// Explicit last-declaration-wins view. The name is deliberately explicit:
    /// callers must not confuse this helper with mise's cross-file precedence.
    pub fn effective_entries_last_wins(&self) -> BTreeMap<&str, &ToolVersionsEntry> {
        let mut entries = BTreeMap::new();
        for entry in self.entries() {
            entries.insert(entry.tool.as_str(), entry);
        }
        entries
    }

    /// Duplicate declarations and their 1-based source line numbers.
    pub fn duplicate_tools(&self) -> BTreeMap<&str, Vec<usize>> {
        let mut lines_by_tool: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for entry in self.entries() {
            lines_by_tool
                .entry(entry.tool.as_str())
                .or_default()
                .push(entry.line_number);
        }
        lines_by_tool.retain(|_, lines| lines.len() > 1);
        lines_by_tool
    }

    /// Whether the document contains at least one tool declaration.
    pub fn has_entries(&self) -> bool {
        self.entries().next().is_some()
    }

    /// Deterministic whitespace-normalized rendering. Use [`Self::source`] for
    /// lossless export; canonical rendering is for snapshots and diagnostics.
    pub fn canonical_string(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        for line in &self.lines {
            match line {
                ToolVersionsLine::Blank { .. } => {}
                ToolVersionsLine::Comment { text, .. } => {
                    output.push('#');
                    if !text.is_empty() {
                        output.push(' ');
                        output.push_str(text);
                    }
                }
                ToolVersionsLine::Entry(entry) => {
                    output.push_str(&entry.tool);
                    for version in entry.deduplicated_versions() {
                        output.push(' ');
                        output.push_str(version);
                    }
                    if let Some(comment) = &entry.comment {
                        output.push_str(" #");
                        if !comment.is_empty() {
                            output.push(' ');
                            output.push_str(comment);
                        }
                    }
                }
            }
            output.push('\n');
        }
        output
    }
}

/// One logical source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVersionsLine {
    Blank {
        line_number: usize,
        raw: String,
    },
    Comment {
        line_number: usize,
        raw: String,
        text: String,
    },
    Entry(ToolVersionsEntry),
}

impl ToolVersionsLine {
    pub fn line_number(&self) -> usize {
        match self {
            Self::Blank { line_number, .. } | Self::Comment { line_number, .. } => *line_number,
            Self::Entry(entry) => entry.line_number,
        }
    }

    pub fn raw(&self) -> &str {
        match self {
            Self::Blank { raw, .. } | Self::Comment { raw, .. } => raw,
            Self::Entry(entry) => &entry.raw,
        }
    }

    pub fn entry(&self) -> Option<&ToolVersionsEntry> {
        match self {
            Self::Entry(entry) => Some(entry),
            Self::Blank { .. } | Self::Comment { .. } => None,
        }
    }
}

/// One tool declaration. Version tokens remain opaque and ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersionsEntry {
    pub tool: String,
    pub versions: Vec<String>,
    pub comment: Option<String>,
    pub line_number: usize,
    pub tool_column: usize,
    pub raw: String,
}

impl ToolVersionsEntry {
    /// Exact-token deduplication that retains the first occurrence and declared
    /// order. Version order may affect PATH/default selection and is semantic.
    pub fn deduplicated_versions(&self) -> Vec<&str> {
        let mut seen = BTreeSet::new();
        self.versions
            .iter()
            .map(String::as_str)
            .filter(|version| seen.insert(*version))
            .collect()
    }

    pub fn classified_versions(&self) -> impl Iterator<Item = (&str, VersionTokenKind)> {
        self.versions
            .iter()
            .map(|version| (version.as_str(), VersionTokenKind::classify(version)))
    }
}

/// Non-resolving token classification for diagnostics and migration reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionTokenKind {
    System,
    VcsReference,
    LocalPath,
    Environment,
    Prefix,
    MovingChannel,
    Opaque,
}

impl VersionTokenKind {
    pub fn classify(token: &str) -> Self {
        let lower = token.to_ascii_lowercase();
        if lower == "system" {
            Self::System
        } else if lower.starts_with("ref:") {
            Self::VcsReference
        } else if lower.starts_with("path:") {
            Self::LocalPath
        } else if lower.starts_with("env:") {
            Self::Environment
        } else if lower.starts_with("prefix:") {
            Self::Prefix
        } else if matches!(
            lower.as_str(),
            "latest" | "stable" | "current" | "nightly" | "canary" | "beta" | "alpha"
        ) || lower.starts_with("lts/")
        {
            Self::MovingChannel
        } else {
            Self::Opaque
        }
    }
}

/// Stable, line-aware syntax errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersionsParseError {
    pub line: usize,
    pub column: usize,
    pub kind: ToolVersionsParseErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVersionsParseErrorKind {
    MissingTool,
    MissingVersion { tool: String },
    ControlCharacter { field: &'static str },
}

impl fmt::Display for ToolVersionsParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: ", self.line, self.column)?;
        match &self.kind {
            ToolVersionsParseErrorKind::MissingTool => {
                formatter.write_str("tool declaration is missing a tool name")
            }
            ToolVersionsParseErrorKind::MissingVersion { tool } => {
                write!(formatter, "tool `{tool}` must declare at least one version")
            }
            ToolVersionsParseErrorKind::ControlCharacter { field } => {
                write!(formatter, "{field} contains a control character")
            }
        }
    }
}

impl Error for ToolVersionsParseError {}

/// File-aware read/parse failure.
#[derive(Debug)]
pub enum ToolVersionsReadError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: ToolVersionsParseError,
    },
}

impl fmt::Display for ToolVersionsReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "could not parse {}: {source}", path.display())
            }
        }
    }
}

impl Error for ToolVersionsReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

fn parse_line(raw: &str, line_number: usize) -> Result<ToolVersionsLine, ToolVersionsParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ToolVersionsLine::Blank {
            line_number,
            raw: raw.to_string(),
        });
    }

    if let Some(comment) = trimmed.strip_prefix('#') {
        return Ok(ToolVersionsLine::Comment {
            line_number,
            raw: raw.to_string(),
            text: comment.trim_start().to_string(),
        });
    }

    let (code, comment) = split_inline_comment(raw);
    let code = code.trim();
    let mut tokens = code.split_whitespace();
    let Some(tool) = tokens.next() else {
        return Err(ToolVersionsParseError {
            line: line_number,
            column: 1,
            kind: ToolVersionsParseErrorKind::MissingTool,
        });
    };

    let tool_column = raw.find(tool).map_or(1, |index| index + 1);
    validate_token(tool, "tool name", line_number, tool_column)?;

    let versions: Vec<String> = tokens.map(str::to_string).collect();
    if versions.is_empty() {
        return Err(ToolVersionsParseError {
            line: line_number,
            column: tool_column + tool.len(),
            kind: ToolVersionsParseErrorKind::MissingVersion {
                tool: tool.to_string(),
            },
        });
    }

    for version in &versions {
        let column = raw.find(version).map_or(tool_column + tool.len() + 1, |index| index + 1);
        validate_token(version, "version token", line_number, column)?;
    }

    Ok(ToolVersionsLine::Entry(ToolVersionsEntry {
        tool: tool.to_string(),
        versions,
        comment: comment.map(str::trim_start).map(str::to_string),
        line_number,
        tool_column,
        raw: raw.to_string(),
    }))
}

fn split_inline_comment(line: &str) -> (&str, Option<&str>) {
    for (index, character) in line.char_indices() {
        if character != '#' {
            continue;
        }
        let begins_comment = index == 0
            || line[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if begins_comment {
            return (&line[..index], Some(&line[index + character.len_utf8()..]));
        }
    }
    (line, None)
}

fn validate_token(
    token: &str,
    field: &'static str,
    line: usize,
    column: usize,
) -> Result<(), ToolVersionsParseError> {
    if token.chars().any(char::is_control) {
        Err(ToolVersionsParseError {
            line,
            column,
            kind: ToolVersionsParseErrorKind::ControlCharacter { field },
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_entries_and_multiple_versions_losslessly() {
        let source = "# runtimes\nnodejs 22.4.0 20.15.1 # ordered\n\npython 3.12.4\n";
        let document = ToolVersionsDocument::parse(source).unwrap();
        assert_eq!(document.source(), source);
        let entries: Vec<_> = document.entries().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tool, "nodejs");
        assert_eq!(entries[0].versions, ["22.4.0", "20.15.1"]);
        assert_eq!(entries[0].comment.as_deref(), Some("ordered"));
        assert_eq!(entries[1].line_number, 4);
    }

    #[test]
    fn preserves_crlf_bom_and_source_whitespace() {
        let source = "\u{feff}  nodejs   22.4.0\r\n# comment\r\n";
        let document = ToolVersionsDocument::parse(source).unwrap();
        assert_eq!(document.source(), source);
        assert_eq!(document.entries().next().unwrap().tool, "nodejs");
    }

    #[test]
    fn hash_inside_token_is_not_an_inline_comment() {
        let document = ToolVersionsDocument::parse("tool ref:feature#anchor # real comment\n").unwrap();
        let entry = document.entries().next().unwrap();
        assert_eq!(entry.versions, ["ref:feature#anchor"]);
        assert_eq!(entry.comment.as_deref(), Some("real comment"));
    }

    #[test]
    fn duplicate_version_deduplication_preserves_semantic_order() {
        let document = ToolVersionsDocument::parse("node 22 20 22 18 20\n").unwrap();
        let entry = document.entries().next().unwrap();
        assert_eq!(entry.deduplicated_versions(), ["22", "20", "18"]);
        assert_eq!(entry.versions, ["22", "20", "22", "18", "20"]);
    }

    #[test]
    fn duplicate_tools_are_preserved_and_last_wins_is_explicit() {
        let document = ToolVersionsDocument::parse("node 20\npython 3.12\nnode 22\n").unwrap();
        assert_eq!(document.entries().count(), 3);
        assert_eq!(document.duplicate_tools()["node"], [1, 3]);
        assert_eq!(
            document.effective_entries_last_wins()["node"].versions,
            ["22"]
        );
    }

    #[test]
    fn missing_version_has_stable_line_diagnostic() {
        let error = ToolVersionsDocument::parse("node 22\npython\n").unwrap_err();
        assert_eq!(error.line, 2);
        assert!(matches!(
            error.kind,
            ToolVersionsParseErrorKind::MissingVersion { ref tool } if tool == "python"
        ));
        assert!(error.to_string().starts_with("2:7:"));
    }

    #[test]
    fn opaque_and_moving_tokens_are_classified_without_resolution() {
        let document = ToolVersionsDocument::parse(
            "tool system ref:abc path:../tool env:VERSION prefix:3 latest 1.2.3\n",
        )
        .unwrap();
        let kinds: Vec<_> = document
            .entries()
            .next()
            .unwrap()
            .classified_versions()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            kinds,
            [
                VersionTokenKind::System,
                VersionTokenKind::VcsReference,
                VersionTokenKind::LocalPath,
                VersionTokenKind::Environment,
                VersionTokenKind::Prefix,
                VersionTokenKind::MovingChannel,
                VersionTokenKind::Opaque,
            ]
        );
    }

    #[test]
    fn canonical_rendering_is_deterministic_but_source_remains_lossless() {
        let source = " node   22 20 22   # preferred\r\n\r\n#note\r\n";
        let document = ToolVersionsDocument::parse(source).unwrap();
        assert_eq!(document.source(), source);
        assert_eq!(
            document.canonical_string(),
            "node 22 20 # preferred\n\n# note\n"
        );
    }

    #[test]
    fn empty_document_is_valid_but_has_no_entries() {
        let document = ToolVersionsDocument::parse("").unwrap();
        assert!(!document.has_entries());
        assert_eq!(document.canonical_string(), "");
    }
}
