#!/usr/bin/env python3
"""One-shot branch integration for the secure zed-binary command."""

from __future__ import annotations

import os
from pathlib import Path

NEW = os.environ["INTERFACES_REV"]
OLD = os.environ["OLD_INTERFACES_REV"]


def replace_required(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"{label} marker not found")
    return text.replace(old, new, 1)


cargo = Path("Cargo.toml")
text = cargo.read_text().replace(OLD, NEW)
marker = '[[bin]]\nname = "zed-gitops"\npath = "src/bin/zed-gitops.rs"\n'
if 'name = "zed-binary"' not in text:
    text = replace_required(
        text,
        marker,
        marker + '\n[[bin]]\nname = "zed-binary"\npath = "src/bin/zed-binary.rs"\n',
        "Cargo.toml binary insertion",
    )
cargo.write_text(text)

lib = Path("src/lib.rs")
text = lib.read_text()
if "pub mod binary_archive;" not in text:
    text = replace_required(
        text,
        "pub mod auth;\n",
        "pub mod auth;\npub mod binary_archive;\n",
        "src/lib.rs binary module insertion",
    )
lib.write_text(text)

registry = Path("src/registry.rs")
text = registry.read_text()
old_fn = '''    fn artifact_file(&self, sha256: &str) -> PathBuf {
        self.root.join("artifacts").join(format!("{sha256}.tar.gz"))
    }
'''
new_fn = '''    fn artifact_file(
        &self,
        sha256: &str,
        format: zed_interfaces::artifact::ArtifactFormat,
    ) -> PathBuf {
        self.root
            .join("artifacts")
            .join(format!("{sha256}.{}", format.extension()))
    }
'''
if old_fn in text:
    text = text.replace(old_fn, new_fn, 1)
elif "fn artifact_file(\n        &self,\n        sha256: &str,\n        format:" not in text:
    raise SystemExit("src/registry.rs artifact helper marker not found")
text = text.replace(
    "let src = self.artifact_file(&version.sha256);",
    "let src = self.artifact_file(&version.sha256, version.format);",
)
text = text.replace(
    "let dest = self.artifact_file(&meta.sha256);",
    "let dest = self.artifact_file(&meta.sha256, meta.format);",
)
registry.write_text(text)

docker = Path(".github/docker/install-test.Dockerfile")
text = docker.read_text().replace(
    "RUN cargo build --release --locked --bin zed",
    "RUN cargo build --release --locked --bins",
)
copy_line = (
    "COPY --from=zed-builder --chown=1001:1001 "
    "/src/zed-cli/target/release/zed /usr/local/bin/zed\n"
)
binary_copy = (
    "COPY --from=zed-builder --chown=1001:1001 "
    "/src/zed-cli/target/release/zed-binary /usr/local/bin/zed-binary\n"
)
if binary_copy not in text:
    text = replace_required(text, copy_line, copy_line + binary_copy, "Dockerfile copy")
docker.write_text(text)

release = Path(".github/workflows/release.yml")
text = release.read_text().replace(OLD, NEW)
target = "${{ matrix.target }}"
text = text.replace(
    f'cross build --locked --release --target "{target}"',
    f'cross build --locked --release --bins --target "{target}"',
)
text = text.replace(
    f'cargo build --locked --release --target "{target}"',
    f'cargo build --locked --release --bins --target "{target}"',
)
old_package = f'''          bin=zed
          [[ "{target}" == *windows* ]] && bin=zed.exe
          dir="target/{target}/release"
          name="zed-{target}"
          mkdir -p dist
          if [[ "{target}" == *windows* ]]; then
            (cd "$dir" && 7z a "$OLDPWD/dist/$name.zip" "$bin")
            archive="$name.zip"
          else
            tar -C "$dir" -czf "dist/$name.tar.gz" "$bin"
            archive="$name.tar.gz"
          fi
'''
new_package = f'''          bins=(zed zed-binary)
          if [[ "{target}" == *windows* ]]; then
            bins=(zed.exe zed-binary.exe)
          fi
          dir="target/{target}/release"
          name="zed-{target}"
          mkdir -p dist
          if [[ "{target}" == *windows* ]]; then
            (cd "$dir" && 7z a "$OLDPWD/dist/$name.zip" "${{bins[@]}}")
            archive="$name.zip"
          else
            tar -C "$dir" -czf "dist/$name.tar.gz" "${{bins[@]}}"
            archive="$name.tar.gz"
          fi
'''
if old_package in text:
    text = text.replace(old_package, new_package, 1)
elif "bins=(zed zed-binary)" not in text:
    raise SystemExit("release packaging marker not found")
release.write_text(text)

for path in Path(".github/workflows").glob("*.yml"):
    text = path.read_text()
    if OLD in text:
        path.write_text(text.replace(OLD, NEW))
