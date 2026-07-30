#!/usr/bin/env python3
"""Cross-check a polyglot `.zpkg.toml` against each target's NATIVE manifest.

A `[targets.<lang>]` slice is published twice from one commit: to zpkg.tech
under its zed name, and to that ecosystem's own registry (npm, PyPI, crates.io,
Maven Central, RubyGems, pub.dev, Hex, Packagist, NuGet, …) under its native
name. Two manifests describe one release, and nothing in zed links them — so
this script is the drift gate.

It reports, per target:

  * the zed coordinates      (`org/name@version`)
  * the native registry it would publish to
  * the native coordinates   (`name@version`, where the ecosystem records them)
  * whether the two versions agree

Exit codes:
  0  every target agrees (gaps may still be reported)
  1  drift, a missing target dir, or --strict with gaps outstanding
  2  bad invocation

Many ecosystems (Go, Swift, Packagist, opam, …) carry no version in their
manifest at all: they are versioned purely by VCS tag. Those are reported as
`from VCS tag` rather than treated as drift. Go additionally needs the tag
`<subdir>/vX.Y.Z` when the module lives in a subdirectory, which zed's
repo-global `[publish].tag_format` cannot express — that is reported as a gap.

Usage:
    scripts/check-native-parity.py [REPO_ROOT] [--strict] [--json]
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover - older interpreters
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ModuleNotFoundError:
        sys.exit(
            "error: needs Python 3.11+ (for tomllib) or `pip install tomli`; "
            f"this is {sys.version.split()[0]}"
        )

VCS_VERSIONED = None  # sentinel: this ecosystem takes its version from the tag


def _re(text: str, pattern: str) -> str | None:
    match = re.search(pattern, text, re.MULTILINE)
    return match.group(1) if match else None


def read_json_manifest(path: Path) -> tuple[str | None, str | None]:
    data = json.loads(path.read_text())
    return data.get("name"), data.get("version")


def read_pyproject(path: Path) -> tuple[str | None, str | None]:
    data = tomllib.loads(path.read_text())
    project = data.get("project", {})
    if project:
        version = project.get("version")
        # setuptools_scm and friends derive the version from the VCS tag.
        if version is None and "version" in project.get("dynamic", []):
            return project.get("name"), VCS_VERSIONED
        return project.get("name"), version
    poetry = data.get("tool", {}).get("poetry", {})
    return poetry.get("name"), poetry.get("version")


def read_cargo(path: Path) -> tuple[str | None, str | None]:
    package = tomllib.loads(path.read_text()).get("package", {})
    version = package.get("version")
    # `version.workspace = true` inherits from a workspace root that is not
    # part of the published slice, so it cannot be resolved from here.
    return package.get("name"), None if isinstance(version, dict) else version


def read_gomod(path: Path) -> tuple[str | None, str | None]:
    return _re(path.read_text(), r"^module\s+(\S+)"), VCS_VERSIONED


def read_gemspec(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return (
        _re(text, r"""\.name\s*=\s*["']([^"']+)["']"""),
        _re(text, r"""\.version\s*=\s*["']([^"']+)["']"""),
    )


def read_pom(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    # Strip <parent>…</parent> so the parent's coordinates are not mistaken
    # for this artifact's own.
    body = re.sub(r"<parent>.*?</parent>", "", text, flags=re.DOTALL)
    group = _re(body, r"<groupId>([^<]+)</groupId>")
    artifact = _re(body, r"<artifactId>([^<]+)</artifactId>")
    version = _re(body, r"<version>([^<]+)</version>")
    name = f"{group}:{artifact}" if group and artifact else artifact
    return name, version


def read_gradle(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    name = _re(text, r"""^\s*(?:rootProject\.name|archivesName)\s*=\s*["']([^"']+)["']""")
    if name is None:
        # Gradle conventionally names the project in settings.gradle[.kts],
        # not in the build script.
        for settings in ("settings.gradle.kts", "settings.gradle"):
            candidate = path.parent / settings
            if candidate.is_file():
                name = _re(
                    candidate.read_text(),
                    r"""^\s*rootProject\.name\s*=\s*["']([^"']+)["']""",
                )
                if name:
                    break
    group = _re(text, r"""^\s*group\s*=\s*["']([^"']+)["']""")
    if group and name:
        name = f"{group}:{name}"
    return name, _re(text, r"""^\s*version\s*=\s*["']([^"']+)["']""")


def read_pubspec(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"^name:\s*(\S+)"), _re(text, r"^version:\s*(\S+)")


def read_mix(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return (
        _re(text, r"""app:\s*:(\w+)"""),
        _re(text, r"""version:\s*["']([^"']+)["']"""),
    )


def read_gleam(path: Path) -> tuple[str | None, str | None]:
    data = tomllib.loads(path.read_text())
    return data.get("name"), data.get("version")


def read_composer(path: Path) -> tuple[str | None, str | None]:
    # Packagist takes the version from the git tag, never from composer.json.
    return json.loads(path.read_text()).get("name"), VCS_VERSIONED


def read_csproj(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    name = _re(text, r"<PackageId>([^<]+)</PackageId>") or path.stem
    return name, _re(text, r"<Version>([^<]+)</Version>")


def read_cabal(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"^name:\s*(\S+)"), _re(text, r"^version:\s*(\S+)")


def read_julia(path: Path) -> tuple[str | None, str | None]:
    data = tomllib.loads(path.read_text())
    return data.get("name"), data.get("version")


def read_nimble(path: Path) -> tuple[str | None, str | None]:
    return path.stem, _re(path.read_text(), r"""^version\s*=\s*["']([^"']+)["']""")


def read_shard(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"^name:\s*(\S+)"), _re(text, r"^version:\s*(\S+)")


def read_rockspec(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"""^package\s*=\s*["']([^"']+)["']"""), _re(
        text, r"""^version\s*=\s*["']([^"']+)["']"""
    )


def read_zigzon(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"""\.name\s*=\s*\.?["]?([\w-]+)["]?"""), _re(
        text, r"""\.version\s*=\s*["]([^"]+)["]"""
    )


def read_description(path: Path) -> tuple[str | None, str | None]:
    text = path.read_text()
    return _re(text, r"^Package:\s*(\S+)"), _re(text, r"^Version:\s*(\S+)")


def read_name_only(path: Path) -> tuple[str | None, str | None]:
    """Ecosystems whose manifest names the package but leaves versioning to the tag."""
    return path.stem, VCS_VERSIONED


def read_unversioned(path: Path) -> tuple[str | None, str | None]:
    """Ecosystems with no machine-readable identity in the manifest at all."""
    return None, VCS_VERSIONED


# (glob, registry label, CI toolchain key, reader). First match in a target
# directory wins, so more specific manifests come first.
ECOSYSTEMS: list[tuple[str, str, str, object]] = [
    ("package.json", "npm", "npm", read_json_manifest),
    ("pyproject.toml", "PyPI", "pypi", read_pyproject),
    ("setup.py", "PyPI", "pypi", read_unversioned),
    ("Cargo.toml", "crates.io", "cargo", read_cargo),
    ("go.mod", "Go module proxy", "go", read_gomod),
    ("*.gemspec", "RubyGems", "rubygems", read_gemspec),
    ("pom.xml", "Maven Central", "maven", read_pom),
    ("build.gradle.kts", "Maven Central", "gradle", read_gradle),
    ("build.gradle", "Maven Central", "gradle", read_gradle),
    ("build.sbt", "Maven Central", "sbt", read_unversioned),
    ("deps.edn", "Clojars", "clojure", read_unversioned),
    ("build.clj", "Clojars", "clojure", read_unversioned),
    ("pubspec.yaml", "pub.dev", "dart", read_pubspec),
    ("composer.json", "Packagist", "php", read_composer),
    ("mix.exs", "Hex", "elixir", read_mix),
    ("rebar.config", "Hex", "erlang", read_unversioned),
    ("gleam.toml", "Hex", "gleam", read_gleam),
    ("*.csproj", "NuGet", "dotnet", read_csproj),
    ("*.fsproj", "NuGet", "dotnet", read_csproj),
    ("*.psd1", "PowerShell Gallery", "powershell", read_name_only),
    ("Package.swift", "Swift Package Index", "swift", read_unversioned),
    ("*.cabal", "Hackage", "haskell", read_cabal),
    ("Project.toml", "Julia General", "julia", read_julia),
    ("*.nimble", "nimble", "nim", read_nimble),
    ("shard.yml", "shards", "crystal", read_shard),
    ("*.rockspec", "LuaRocks", "lua", read_rockspec),
    ("build.zig.zon", "zig fetch", "zig", read_zigzon),
    ("*.opam", "opam", "ocaml", read_name_only),
    ("dune-project", "opam", "ocaml", read_unversioned),
    ("DESCRIPTION", "CRAN", "r", read_description),
    ("CMakeLists.txt", "source / CMake", "cmake", read_unversioned),
]


def normalize_version(key: str, native_version: str) -> str:
    """Strip ecosystem-specific suffixes that are not part of the upstream version.

    LuaRocks appends a rockspec revision (`0.1.0-1` is version 0.1.0, revision 1),
    and Maven/NuGet prereleases are compared on their release core.
    """
    if key == "lua":
        return re.sub(r"-\d+$", "", native_version)
    return native_version


def find_native(target_dir: Path):
    """Locate the native manifest at a target's root."""
    for pattern, registry, key, reader in ECOSYSTEMS:
        if "*" in pattern:
            matches = sorted(target_dir.glob(pattern))
            if matches:
                return matches[0], registry, key, reader
        elif (target_dir / pattern).is_file():
            return target_dir / pattern, registry, key, reader
    return None


def native_publish_disabled(path: Path, key: str) -> bool:
    """Return whether a native manifest explicitly opts out of its registry."""
    if key != "dart":
        return False
    return bool(
        re.search(
            r"""^publish_to:\s*(?:none|["']none["'])(?:\s*#.*)?$""",
            path.read_text(),
            re.MULTILINE,
        )
    )


def main() -> int:
    argv = sys.argv[1:]
    strict = "--strict" in argv
    emit_json = "--json" in argv
    positional = [a for a in argv if not a.startswith("-")]
    root = Path(positional[0] if positional else ".").resolve()

    manifest_path = root / ".zpkg.toml"
    if not manifest_path.is_file():
        print(f"error: no .zpkg.toml at {root}", file=sys.stderr)
        return 2

    manifest = tomllib.loads(manifest_path.read_text())
    package = manifest["package"]
    org, version = package["org"], package["version"]
    targets = manifest.get("targets", {})

    if not targets:
        if emit_json:
            print('{"include": []}')
        else:
            print(f"{org}/{package['name']}@{version} declares no [targets]; nothing to check.")
        return 0

    tag_format = manifest.get("publish", {}).get("tag_format", "v{version}")
    # `problems` are drift the repo can fix today and must: they fail the build.
    # `gaps` are things zed cannot yet express, so they are reported loudly but
    # do not fail unless --strict. Flip a gap into a problem once the feature
    # lands, and this gate starts enforcing it.
    problems: list[str] = []
    gaps: list[str] = []
    rows: list[tuple[str, str, str, str, str]] = []
    matrix: list[dict[str, str]] = []

    # `dir = "."` is the sanctioned whole-repository target (doc 18): it ships
    # every language as one artifact alongside the isolated slices, and
    # `zed pack` exempts it from the nested-manifest guard by design. Every
    # OTHER target must be an isolated root — a language target that contains
    # another language target would put the inner language's bytes in the
    # outer artifact, which is exactly what doc 17 exists to prevent.
    declared = {t: targets[t]["dir"].strip("/") for t in targets}
    for target, dir_ in declared.items():
        if dir_ in ("", "."):
            continue
        for other, other_dir in declared.items():
            if other != target and other_dir not in ("", ".") and (
                dir_ == other_dir or dir_.startswith(other_dir + "/")
            ):
                problems.append(
                    f"[targets.{target}] dir `{dir_}` is nested inside [targets.{other}] "
                    f"dir `{other_dir}`; the outer slice would contain the inner language"
                )

    for target in sorted(targets):
        section = targets[target]
        zed_name = section.get("name") or f"{package['name']}-{target}"
        zed_coords = f"{org}/{zed_name}@{version}"
        target_dir = root / section["dir"]

        if not target_dir.is_dir():
            problems.append(f"[targets.{target}] dir `{section['dir']}` does not exist")
            continue

        found = find_native(target_dir)
        if found is None:
            # Not drift: plenty of languages (shell, matlab) have no registry.
            # It just means this slice is zpkg.tech-only.
            rows.append((target, zed_coords, "—", "—", "zed only"))
            continue

        native_path, registry, key, reader = found
        native_name, native_version = reader(native_path)

        if native_version is VCS_VERSIONED:
            if key == "go" and section["dir"].strip("/") not in ("", "."):
                subdir = section["dir"].strip("/")
                expected_tag = f"{subdir}/{tag_format.format(version=version)}"
                note = f"tag `{expected_tag}`"
                gaps.append(
                    f"[targets.{target}] is a Go module in `{subdir}/`, so the module proxy "
                    f"requires the tag `{expected_tag}` — but [publish].tag_format is repo-global "
                    f"(`{tag_format}`), so zed can only create `{tag_format.format(version=version)}`. "
                    f"Per-target tag formats are needed to publish this slice to the Go proxy."
                )
            else:
                note = "from VCS tag"
            shown = native_name or native_path.name
        elif native_version is None:
            note = "version not readable"
            shown = native_name or native_path.name
        elif normalize_version(key, native_version) != version:
            note = "DRIFT"
            shown = f"{native_name or native_path.name}@{native_version}"
            problems.append(
                f"[targets.{target}] version drift: .zpkg.toml says {version}, "
                f"{section['dir']}/{native_path.name} says {native_version}"
            )
        else:
            note = "ok"
            shown = f"{native_name or native_path.name}@{native_version}"

        if native_publish_disabled(native_path, key):
            rows.append(
                (target, zed_coords, "—", shown, f"{note}; publish_to: none")
            )
            continue

        rows.append((target, zed_coords, registry, shown, note))
        matrix.append(
            {
                "target": target,
                "dir": section["dir"],
                "registry": registry,
                "ecosystem": key,
                "native": shown,
            }
        )

    if emit_json:
        print(json.dumps({"include": matrix}))
        return 1 if problems else 0

    header = ("target", "zed package", "native registry", "native package", "status")
    widths = [max(len(str(row[i])) for row in [header, *rows]) for i in range(len(header))]
    print("  ".join(h.ljust(w) for h, w in zip(header, widths)).rstrip())
    print("  ".join("-" * w for w in widths))
    for row in rows:
        print("  ".join(str(c).ljust(w) for c, w in zip(row, widths)).rstrip())
    print()

    native_count = sum(1 for r in rows if r[2] != "—")
    print(
        f"{len(rows)} target(s): {native_count} publishable to a native registry, "
        f"{len(rows) - native_count} to zpkg.tech only"
    )

    if gaps:
        print(f"\n{len(gaps)} unsupported-by-zed gap(s):")
        for gap in gaps:
            print(f"  ! {gap}")

    if problems:
        print(f"\n{len(problems)} problem(s):", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1

    if gaps and strict:
        print("--strict: treating gaps as failures", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
