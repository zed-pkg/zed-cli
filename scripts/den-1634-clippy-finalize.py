#!/usr/bin/env python3
"""Remove the one Clippy-reported redundant return from the Windows cwd helper."""

from pathlib import Path

path = Path("src/dev.rs")
text = path.read_text(encoding="utf-8")
old = '''        return PathBuf::from(OsString::from_wide(&normalize_windows_child_current_dir(
            &wide,
        )));
'''
new = '''        PathBuf::from(OsString::from_wide(&normalize_windows_child_current_dir(
            &wide,
        )))
'''
if text.count(old) != 1:
    raise SystemExit("expected exactly one rustfmt-normalized redundant Windows cwd return")
path.write_text(text.replace(old, new, 1), encoding="utf-8")

Path("scripts/den-1634-clippy-finalize.py").unlink()
Path(".github/workflows/den-1634-clippy-finalize.yml").unlink()
