use zed_cli::tool_versions::{
    ToolVersionsDocument, ToolVersionsParseErrorKind, VersionTokenKind,
};

#[test]
fn public_parser_preserves_order_comments_and_source_bytes() {
    let source = "# project runtimes\nnodejs 22.4.0 20.15.1 # first is default\npython 3.12.4\n";
    let document = ToolVersionsDocument::parse(source).unwrap();
    assert_eq!(document.source(), source);

    let entries: Vec<_> = document.entries().collect();
    assert_eq!(entries[0].tool, "nodejs");
    assert_eq!(entries[0].versions, ["22.4.0", "20.15.1"]);
    assert_eq!(entries[0].comment.as_deref(), Some("first is default"));
}

#[test]
fn public_parser_never_resolves_or_executes_opaque_tokens() {
    let document = ToolVersionsDocument::parse(
        "custom ref:main path:../local env:CUSTOM_VERSION latest 1.2.3\n",
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
            VersionTokenKind::VcsReference,
            VersionTokenKind::LocalPath,
            VersionTokenKind::Environment,
            VersionTokenKind::MovingChannel,
            VersionTokenKind::Opaque,
        ]
    );
}

#[test]
fn malformed_lines_report_stable_locations() {
    let error = ToolVersionsDocument::parse("node 22\npython\n").unwrap_err();
    assert_eq!(error.line, 2);
    assert!(matches!(
        error.kind,
        ToolVersionsParseErrorKind::MissingVersion { ref tool } if tool == "python"
    ));
}
