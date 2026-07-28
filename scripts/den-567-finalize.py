#!/usr/bin/env python3
"""Apply the final DEN-567 Bash contract-test correction exactly once."""

from pathlib import Path

path = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
content = path.read_text(encoding="utf-8")
old = '''            _zed
            printf "%s\\n" "${COMPREPLY[@]}" | grep -Fx install
'''
new = '''            _zed zed "" zed
            printf "%s\\n" "${COMPREPLY[@]}" | grep -Fx install
'''
count = content.count(old)
if count != 1:
    raise RuntimeError(f"expected one Bash invocation target, found {count}")
path.write_text(content.replace(old, new, 1), encoding="utf-8")
print("DEN-567 Bash CI invocation corrected")
