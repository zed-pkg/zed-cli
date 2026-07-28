#!/usr/bin/env python3
"""Repair the first-pass materializer's stale import anchor."""

from pathlib import Path

path = Path("scripts/materialize-manifestless-install.py")
source = path.read_text(encoding="utf-8")
old = '''replace_once(
    "src/main.rs",
    "use zed_cli::flags;\\n",
    "use zed_cli::flags;\\nuse zed_cli::manifestless;\\n",
    "import manifestless dispatcher",
)'''
new = '''replace_once(
    "src/main.rs",
    "use zed_cli::config::Config;\\n",
    "use zed_cli::config::Config;\\nuse zed_cli::manifestless;\\n",
    "import manifestless dispatcher",
)'''
count = source.count(old)
if count != 1:
    raise SystemExit(f"expected one stale main import replacement, found {count}")
path.write_text(source.replace(old, new, 1), encoding="utf-8")
