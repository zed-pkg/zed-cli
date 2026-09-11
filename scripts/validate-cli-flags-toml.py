#!/usr/bin/env python3
"""Independent TOML/fail-closed preflight for package-owned flags2env contracts."""

from __future__ import annotations

import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PRIMARY = ROOT / ".cli-flags.toml"
CONTRACTS = [PRIMARY, *sorted(ROOT.glob(".*-cli-flags.toml"))]

errors: list[str] = []
seen: set[pathlib.Path] = set()
for path in CONTRACTS:
    path = path.resolve()
    if path in seen:
        continue
    seen.add(path)
    if not path.is_file():
        errors.append(f"missing flags contract: {path.name}")
        continue
    try:
        contract = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        errors.append(f"invalid TOML in {path.name}: {exc}")
        continue

    parse = contract.get("parse")
    if not isinstance(parse, dict):
        errors.append(f"{path.name}: missing [parse] table")
        continue
    if parse.get("allow_unknown") is not False:
        errors.append(f"{path.name}: parse.allow_unknown must be false")

    env_names = [parse.get(key) for key in ("command_env", "unknown_options_env", "errors_env")]
    if any(not isinstance(value, str) or not value.strip() for value in env_names):
        errors.append(f"{path.name}: parse env outputs must be nonblank strings")
    elif len(set(env_names)) != len(env_names):
        errors.append(f"{path.name}: parse env outputs must be distinct")

if PRIMARY.resolve() in seen and PRIMARY.is_file():
    primary = tomllib.loads(PRIMARY.read_text(encoding="utf-8"))
    flags = primary.get("flags", {})
    for required in ("no_mirrors", "trust_mirror_metadata"):
        if required not in flags:
            errors.append(f".cli-flags.toml: missing required global flag {required}")

if errors:
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    raise SystemExit(1)

print("validated root flags2env TOML contracts: " + ", ".join(path.name for path in sorted(seen)))
