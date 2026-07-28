#!/usr/bin/env python3
"""Prepare the consolidated DEN-567 product materializers.

Already-landed reviewed fragments are accepted only when they exactly equal the
intended replacement. The large CI consent section is matched by stable step
markers instead of a brittle whole-block snapshot.
"""

from pathlib import Path


materializer_path = Path("scripts/materialize-den-567.py")
materializer = materializer_path.read_text(encoding="utf-8")
old_replace = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match in {path}, found {count}")
    write(path, content.replace(old, new, 1))
'''
new_replace = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count == 1:
        write(path, content.replace(old, new, 1))
        return
    if count == 0 and new in content:
        return
    raise RuntimeError(f"{label}: expected one original or an exact applied value in {path}, found {count}")
'''
if new_replace not in materializer:
    count = materializer.count(old_replace)
    if count != 1:
        raise SystemExit(f"expected one materializer replace_once implementation, found {count}")
    materializer = materializer.replace(old_replace, new_replace, 1)
    materializer_path.write_text(materializer, encoding="utf-8")


hardener_path = Path("scripts/harden-den-567-product.py")
hardener = hardener_path.read_text(encoding="utf-8")
hardener_replace = '''def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: {label}: expected one target, found {count}")
    write(path, content.replace(old, new, 1))
'''
marker_helper = '''def replace_ci_consent_block(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    if new in content:
        return
    start_marker = "      - name: Manifestless install fails closed without a terminal or bypass\\n"
    end_marker = "      - name: Both non-interactive bypass spellings install without a manifest\\n"
    start = content.find(start_marker)
    end = content.find(end_marker, start + 1) if start >= 0 else -1
    if start < 0 or end < 0:
        raise RuntimeError(
            f"{path}: {label}: could not find the generated consent test step boundaries"
        )
    write(path, content[:start] + new + content[end:])
'''
if marker_helper not in hardener:
    count = hardener.count(hardener_replace)
    if count != 1:
        raise SystemExit(f"expected one hardener replace_once implementation, found {count}")
    hardener = hardener.replace(
        hardener_replace,
        hardener_replace + "\n\n" + marker_helper,
        1,
    )

label = '"exercise real TTY and pre-resolution consent boundaries",'
label_index = hardener.find(label)
if label_index < 0:
    raise SystemExit("could not find the CI consent replacement label")
call_index = hardener.rfind("replace_once(", 0, label_index)
if call_index < 0:
    call_index = hardener.rfind("replace_ci_consent_block(", 0, label_index)
    if call_index < 0:
        raise SystemExit("could not find the CI consent replacement call")
elif not hardener.startswith("replace_ci_consent_block(", call_index):
    hardener = (
        hardener[:call_index]
        + "replace_ci_consent_block("
        + hardener[call_index + len("replace_once(") :]
    )

hardener_path.write_text(hardener, encoding="utf-8")
