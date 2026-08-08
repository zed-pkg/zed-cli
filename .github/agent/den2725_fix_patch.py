#!/usr/bin/env python3
from pathlib import Path

path = Path(".github/agent/den2725_root_dispatch_patch.py")
text = path.read_text(encoding="utf-8")
marker = '\n\nPath("src/external_subcommands.rs").write_text('
helper = '''


def replace_one_of_many(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count < 1:
        raise SystemExit(f"{path}: expected at least one replacement target")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")
'''
if text.count(marker) != 1:
    raise SystemExit("patch helper insertion marker changed")
text = text.replace(marker, helper + marker, 1)
old = "for occurrence in range(2):\n    replace_once("
new = "for occurrence in range(2):\n    replace_one_of_many("
if text.count(old) != 2:
    raise SystemExit(f"expected two repeated completion loops, found {text.count(old)}")
path.write_text(text.replace(old, new), encoding="utf-8")
