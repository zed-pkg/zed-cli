#!/usr/bin/env python3
"""Temporary, idempotent hardening for the complete current mise lock contract."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


source = Path("src/mise_lock.rs")
text = source.read_text(encoding="utf-8")

text = replace_once(
    text,
    """    /// Presentation-independent clone. Tool identities and set-like package
    /// lists are sorted; ordered additional artifacts remain ordered.
""",
    """    /// Presentation-independent clone. Tool identity and additional-artifact
    /// order remain semantic; only set-like package lists are sorted.
""",
    "normalization documentation",
)

text = replace_once(
    text,
    """        for identities in normalized.tools.values_mut() {
            for identity in identities.iter_mut() {
                identity.normalize();
            }
            identities.sort_by_cached_key(|identity| {
                serde_json::to_string(identity).unwrap_or_else(|_| identity.version.clone())
            });
        }
""",
    """        for identities in normalized.tools.values_mut() {
            for identity in identities.iter_mut() {
                identity.normalize();
            }
            // mise writes each Vec<LockfileTool> in stored order and
            // multi-version PATH/default selection is order-sensitive.
        }
""",
    "semantic tool order",
)

text = replace_once(
    text,
    """    #[test]
    fn tool_identity_order_is_not_semantic() {
""",
    """    #[test]
    fn tool_identity_order_is_semantic() {
""",
    "tool order regression name",
)

start = text.index("    fn tool_identity_order_is_semantic()")
end = text.index(
    "    #[test]\n    fn ordered_additional_artifacts_change_semantic_identity()", start
)
block = text[start:end]
old_assert = """        assert_eq!(
            first.semantic_digest_sha256().unwrap(),
            second.semantic_digest_sha256().unwrap()
        );
"""
new_assert = """        assert_ne!(
            first.semantic_digest_sha256().unwrap(),
            second.semantic_digest_sha256().unwrap()
        );
"""
if new_assert not in block:
    if block.count(old_assert) != 1:
        raise SystemExit("tool order regression assertion: expected one anchor")
    block = block.replace(old_assert, new_assert, 1)
    text = text[:start] + block + text[end:]

scheme_guard = """    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "`{field}` must use http or https, got `{}`",
        parsed.scheme()
    );
"""
host_guard = scheme_guard + """    ensure!(
        parsed.host_str().is_some(),
        "`{field}` must contain a network host"
    );
"""
text = replace_once(text, scheme_guard, host_guard, "URL host validation")

text = replace_once(
    text,
    '        "x-amz-signature",\n',
    '        "x-amz-signature",\n        "x-amz-security-token",\n',
    "AWS session-token query rejection",
)

text = replace_once(
    text,
    """            "https://example.test/tool.tar.gz?token=secret",
            "https://example.test/tool.tar.gz#fragment",
""",
    """            "https://example.test/tool.tar.gz?token=secret",
            "https://example.test/tool.tar.gz?X-Amz-Security-Token=secret",
            "https://example.test/tool.tar.gz#fragment",
            "https:/missing-host/tool.tar.gz",
""",
    "secret URL regressions",
)

source.write_text(text, encoding="utf-8")

docs = Path("docs/mise-lock-contract.md")
text = docs.read_text(encoding="utf-8")
text = text.replace(
    "- tool identity arrays are sorted by complete identity;\n",
    "- tool identity arrays retain declared order because multi-version activation is order-sensitive;\n",
    1,
)
text = text.replace(
    "Ordered `additional_artifacts` remain ordered because mise extracts them in\n"
    "sequence. Reordering them changes the semantic digest.\n",
    "Ordered tool identities and `additional_artifacts` remain ordered because mise\n"
    "uses their sequence during activation and extraction. Reordering either changes the\n"
    "semantic digest.\n",
    1,
)
text = text.replace(
    "- URL fragments;\n",
    "- missing URL hosts and URL fragments;\n",
    1,
)
docs.write_text(text, encoding="utf-8")
