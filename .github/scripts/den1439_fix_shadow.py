from pathlib import Path

path = Path("src/environment.rs")
text = path.read_text()
replacements = [
    ("let first = tempfile::tempdir().unwrap();", "let first_dir = tempfile::tempdir().unwrap();"),
    ("let second = tempfile::tempdir().unwrap();", "let second_dir = tempfile::tempdir().unwrap();"),
    ("for root in [first.path(), second.path()]", "for root in [first_dir.path(), second_dir.path()]"),
    ("if root == first.path()", "if root == first_dir.path()"),
    ("fs::write(first.path().join(\"mise.lock\"), first_lock).unwrap();", "fs::write(first_dir.path().join(\"mise.lock\"), first_lock).unwrap();"),
    ("fs::write(second.path().join(\"mise.lock\"), second_lock).unwrap();", "fs::write(second_dir.path().join(\"mise.lock\"), second_lock).unwrap();"),
    ("let first = import_mise(first.path(), None, None, true).unwrap();", "let first_imported = import_mise(first_dir.path(), None, None, true).unwrap();"),
    ("let second = import_mise(second.path(), None, None, true).unwrap();", "let second_imported = import_mise(second_dir.path(), None, None, true).unwrap();"),
    ("assert_eq!(first.plan.sources[0].digest, second.plan.sources[0].digest);", "assert_eq!(\n            first_imported.plan.sources[0].digest,\n            second_imported.plan.sources[0].digest\n        );"),
    ("assert_eq!(first.digest, second.digest);", "assert_eq!(first_imported.digest, second_imported.digest);"),
    ("second.path().join(\"mise.lock\")", "second_dir.path().join(\"mise.lock\")"),
    ("let changed = import_mise(second.path(), None, None, true).unwrap();", "let changed = import_mise(second_dir.path(), None, None, true).unwrap();"),
    ("assert_ne!(first.plan.sources[0].digest, changed.plan.sources[0].digest);", "assert_ne!(\n            first_imported.plan.sources[0].digest,\n            changed.plan.sources[0].digest\n        );"),
]
for old, new in replacements:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one shadowing test match, found {count}: {old!r}")
    text = text.replace(old, new, 1)
path.write_text(text)
