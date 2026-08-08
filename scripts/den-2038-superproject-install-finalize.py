#!/usr/bin/env python3
"""Keep cooperative submodule installation on the selected superproject."""

from pathlib import Path

source = Path("src/main.rs")
text = source.read_text(encoding="utf-8")
old = '''                submodules::sync(&project)?;
                managed_install::install(
                    &cwd,
'''
new = '''                submodules::sync(&project)?;
                managed_install::install(
                    &project,
'''
if old in text:
    if text.count(old) != 1:
        raise SystemExit("expected exactly one cooperative-install cwd call")
    text = text.replace(old, new, 1)
elif new not in text:
    raise SystemExit("cooperative-install call is neither the old nor expected form")
source.write_text(text, encoding="utf-8")

Path("scripts/den-2038-superproject-install-finalize.py").unlink()
Path(".github/workflows/den-2038-superproject-install-finalize.yml").unlink()
