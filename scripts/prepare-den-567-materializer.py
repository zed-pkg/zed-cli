#!/usr/bin/env python3
"""Make the consolidated product materializer tolerate already-landed fragments.

Several reviewed pieces (the immutable flags2env pin and completion module) were
committed before the whole product patch. The materializer must treat an exact
replacement value as already applied while still failing closed on any third
shape.
"""

from pathlib import Path

path = Path("scripts/materialize-den-567.py")
source = path.read_text(encoding="utf-8")
old = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match in {path}, found {count}")
    write(path, content.replace(old, new, 1))
'''
new = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count == 1:
        write(path, content.replace(old, new, 1))
        return
    if count == 0 and new in content:
        return
    raise RuntimeError(f"{label}: expected one original or an exact applied value in {path}, found {count}")
'''
count = source.count(old)
if count != 1:
    raise SystemExit(f"expected one replace_once implementation, found {count}")
path.write_text(source.replace(old, new, 1), encoding="utf-8")
