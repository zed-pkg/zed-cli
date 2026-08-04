#!/usr/bin/env python3
"""Keep the untagged platform enum compact without weakening its schema."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


path = Path("src/mise_lock.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "    Detail(MisePlatformDetails),\n",
    "    Detail(Box<MisePlatformDetails>),\n",
    "boxed detailed platform variant",
)
text = replace_once(
    text,
    """                *self = Self::Detail(MisePlatformDetails {
                    checksum: Some(checksum),
                    ..MisePlatformDetails::default()
                });
""",
    """                *self = Self::Detail(Box::new(MisePlatformDetails {
                    checksum: Some(checksum),
                    ..MisePlatformDetails::default()
                }));
""",
    "boxed compact-checksum normalization",
)

anchor = """    #[test]
    fn complete_current_lock_round_trips_and_is_frozen_portable() {
"""
regression = """    #[test]
    fn platform_info_enum_remains_indirect_and_bounded() {
        assert!(std::mem::size_of::<MisePlatformInfo>() <= 64);
    }

    #[test]
    fn complete_current_lock_round_trips_and_is_frozen_portable() {
"""
text = replace_once(text, anchor, regression, "platform enum size regression")
path.write_text(text, encoding="utf-8")
