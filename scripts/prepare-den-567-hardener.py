#!/usr/bin/env python3
"""Make the final hardener replace generated CI sections by semantic markers."""

from pathlib import Path

path = Path("scripts/harden-den-567-product.py")
source = path.read_text(encoding="utf-8")
old = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: {label}: expected one target, found {count}")
    write(path, content.replace(old, new, 1))
'''
new = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count == 1:
        write(path, content.replace(old, new, 1))
        return

    if path == ".github/workflows/ci.yml" and label == "exercise real TTY and pre-resolution consent boundaries":
        start_marker = "      - name: Manifestless install fails closed without a terminal or bypass\\n"
        end_marker = "      - name: Both non-interactive bypass spellings install without a manifest\\n"
        start = content.find(start_marker)
        end = content.find(end_marker, start + 1)
        if start >= 0 and end > start:
            write(path, content[:start] + new.rstrip() + "\\n\\n" + content[end:])
            return

    if path == ".github/workflows/ci.yml" and label == "require explicit frozen mode for lock-only reinstall":
        step_marker = "      - name: Frozen lock-only manifestless reinstall remains reproducible\\n"
        start_marker = "          rm -rf \"$root/zed_modules\" \"$root/node_modules\" \"$root/.zed\"\\n"
        end_marker = "\\n\\n      - name: One clear nested native project becomes the install root\\n"
        step = content.find(step_marker)
        start = content.find(start_marker, step + 1)
        end = content.find(end_marker, start + 1)
        if step >= 0 and start >= 0 and end > start:
            write(path, content[:start] + new.rstrip() + content[end:])
            return

    raise RuntimeError(f"{path}: {label}: expected one target, found {count}")
'''
count = source.count(old)
if count != 1:
    raise SystemExit(f"expected one hardener replace_once function, found {count}")
path.write_text(source.replace(old, new, 1), encoding="utf-8")
