from pathlib import Path


def replace_once(path: Path, needle: str, replacement: str) -> None:
    text = path.read_text()
    count = text.count(needle)
    if count != 1:
        raise SystemExit(f"{path}: expected one patch anchor, found {count}: {needle!r}")
    path.write_text(text.replace(needle, replacement, 1))


replace_once(
    Path("Cargo.toml"),
    'serde_json = "1"\n',
    'serde_json = "1"\n'
    '# Structured resolver diagnostics. Pinned to the reviewed ores-otel Rust SDK.\n'
    'oresoftware-next-loggers = { git = "https://github.com/ores-otel/ores.otel.log.git", rev = "f6422ef56c80cdc702aee04008b53fee87757671" }\n',
)

replace_once(
    Path("src/install_graph.rs"),
    "mod artifact;\n#[cfg(test)]\nmod hardening_tests;\nmod resolver;\nmod solver;\n#[cfg(test)]\nmod tests;",
    "mod artifact;\nmod cycle_diagnostics;\n#[cfg(test)]\nmod cycle_regression_tests;\n#[cfg(test)]\nmod hardening_tests;\nmod resolver;\nmod solver;\n#[cfg(test)]\nmod tests;",
)

replace_once(
    Path("src/install_graph/solver.rs"),
    "    path.push(dependency.to_string());\n    Constraint {\n",
    "    path.push(dependency.to_string());\n    if cycle_back_edge {\n        super::cycle_diagnostics::emit_cycle_back_edge(&path, requirement);\n    }\n    Constraint {\n",
)

Path(".github/workflows/apply-cycle-hardening.yml").unlink()
Path(".github/scripts/apply-cycle-hardening.py").unlink()
