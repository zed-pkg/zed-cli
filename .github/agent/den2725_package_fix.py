#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    ".zpkg.toml",
    '''[build]
command = "cargo build --release --locked"
outputs = ["target/release/zed"]

[bin]
zed = "target/release/zed"
''',
    '''[build]
command = "cargo build --release --locked --bins"
outputs = ["target/release/zed", "target/release/zed-gitops"]

[bin]
zed = "target/release/zed"
"zed-gitops" = "target/release/zed-gitops"
''',
)

replace_once(
    ".zpkg.toml",
    '''smoke_test = "test -x \\"$ZED_PKG_TEST_TARGET/target/release/zed\\" && \\"$ZED_PKG_TEST_TARGET/target/release/zed\\" --version"''',
    '''smoke_test = "test -x \\"$ZED_PKG_TEST_TARGET/target/release/zed\\" && test -x \\"$ZED_PKG_TEST_TARGET/target/release/zed-gitops\\" && \\"$ZED_PKG_TEST_TARGET/target/release/zed\\" --version && \\"$ZED_PKG_TEST_TARGET/target/release/zed\\" gitops validate --help"''',
)

replace_once(
    "scripts/validate-zed-package-graph.sh",
    '''grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
grep -Fq '".vendor/.zed/**"' .zpkg.toml || { echo 'publish exclusions must omit materialized Zed dependencies' >&2; exit 1; }
''',
    '''grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
grep -Fq 'outputs = ["target/release/zed", "target/release/zed-gitops"]' .zpkg.toml || { echo 'Zed package must publish both root and GitOps executables' >&2; exit 1; }
grep -Fq '"zed-gitops" = "target/release/zed-gitops"' .zpkg.toml || { echo 'Zed package must install the sibling zed-gitops executable' >&2; exit 1; }
grep -Fq '".vendor/.zed/**"' .zpkg.toml || { echo 'publish exclusions must omit materialized Zed dependencies' >&2; exit 1; }
''',
)
