#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: tests/polyglot-consumer-project-shapes-e2e.sh /absolute/path/to/zed" >&2
  exit 2
fi

zed=$1
if [[ ! -x "$zed" ]]; then
  echo "zed executable not found: $zed" >&2
  exit 2
fi
zed="$(cd -- "$(dirname -- "$zed")" && pwd -P)/$(basename -- "$zed")"
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
fixture_source="$repo_root/tests/fixtures/polyglot"

remove_suite_root=false
if [[ -n "${ZED_PROJECT_SHAPES_E2E_ROOT:-}" ]]; then
  suite_root=$ZED_PROJECT_SHAPES_E2E_ROOT
  if [[ -e "$suite_root" ]]; then
    echo "ZED_PROJECT_SHAPES_E2E_ROOT must not already exist: $suite_root" >&2
    exit 2
  fi
  mkdir -p "$suite_root"
else
  suite_root="$(mktemp -d "${TMPDIR:-/tmp}/zed-project-shapes.XXXXXX")"
  remove_suite_root=true
fi

cleanup() {
  if $remove_suite_root && [[ "${ZED_PROJECT_SHAPES_E2E_KEEP:-0}" != 1 ]]; then
    rm -rf -- "$suite_root"
  else
    printf 'polyglot project-shapes workspace: %s\n' "$suite_root"
  fi
}
trap cleanup EXIT INT TERM HUP

fixture="$suite_root/polyglot-fixture"
registry="$suite_root/registry"
home="$suite_root/zed-home"
projects="$suite_root/projects"
cp -R "$fixture_source" "$fixture"
mkdir -p "$registry" "$home" "$projects"
registry_url="file://$registry"

fail() {
  echo "polyglot consumer project shapes: $*" >&2
  exit 1
}

checksum() {
  cksum < "$1"
}

(
  cd "$fixture"
  ZED_PKG_HOME="$home/author" \
    "$zed" publish \
      --registry "$registry_url" \
      --skip-vcs-checks
)

for target in nodejs python golang rust ruby; do
  test -d "$registry/packages/zed-pkg/poly-fixture-$target"
done

write_project() {
  local host=$1
  local root=$2
  mkdir -p "$root/deep/nested"
  case "$host" in
    npm)
      printf '%s\n' '{"name":"zed-shape-npm","private":true,"version":"0.0.0"}' > "$root/package.json"
      ;;
    golang)
      cat > "$root/go.mod" <<'EOF_GO'
module example.com/zed-shape-go

go 1.22
EOF_GO
      ;;
    rust)
      cat > "$root/Cargo.toml" <<'EOF_RUST'
[package]
name = "zed-shape-rust"
version = "0.0.0"
edition = "2021"
EOF_RUST
      mkdir -p "$root/src"
      printf 'fn main() {}\n' > "$root/src/main.rs"
      ;;
    ruby)
      printf "source 'https://rubygems.org'\n" > "$root/Gemfile"
      ;;
    python)
      cat > "$root/pyproject.toml" <<'EOF_PY'
[project]
name = "zed-shape-python"
version = "0.0.0"
EOF_PY
      ;;
    gleam)
      cat > "$root/gleam.toml" <<'EOF_GLEAM'
name = "zed_shape_gleam"
version = "0.0.0"
target = "erlang"
EOF_GLEAM
      mkdir -p "$root/src"
      printf 'pub fn main() { Nil }\n' > "$root/src/zed_shape_gleam.gleam"
      ;;
    dart)
      cat > "$root/pubspec.yaml" <<'EOF_DART'
name: zed_shape_dart
version: 0.0.0
environment:
  sdk: '>=3.5.0 <4.0.0'
EOF_DART
      ;;
    *) fail "unknown project host: $host" ;;
  esac
}

marker_for() {
  case "$1" in
    npm) echo package.json ;;
    golang) echo go.mod ;;
    rust) echo Cargo.toml ;;
    ruby) echo Gemfile ;;
    python) echo pyproject.toml ;;
    gleam) echo gleam.toml ;;
    dart) echo pubspec.yaml ;;
    *) fail "unknown project host: $1" ;;
  esac
}

package_for() {
  case "$1" in
    npm) echo poly-fixture-nodejs ;;
    golang) echo poly-fixture-golang ;;
    rust) echo poly-fixture-rust ;;
    ruby) echo poly-fixture-ruby ;;
    python) echo poly-fixture-python ;;
    gleam|dart) echo poly-fixture-rust ;;
    *) fail "unknown project host: $1" ;;
  esac
}

adapter_for() {
  case "$1" in
    npm) echo node ;;
    golang) echo go ;;
    rust) echo rust ;;
    ruby|gleam) echo none ;;
    python) echo python ;;
    dart) echo dart ;;
    *) fail "unknown project host: $1" ;;
  esac
}

assert_adapter() {
  local host=$1
  local root=$2
  local package=$3
  case "$host" in
    npm)
      [[ -L "$root/node_modules/@zed-pkg/$package" ]] || fail "npm adapter link missing"
      ;;
    golang)
      [[ -f "$root/.zed/go.work" ]] || fail "go adapter output missing"
      grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/go.work"
      ;;
    rust)
      [[ -f "$root/.zed/cargo-paths.toml" ]] || fail "rust adapter output missing"
      grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/cargo-paths.toml"
      ;;
    python)
      [[ -f "$root/.zed/pythonpath" ]] || fail "python adapter output missing"
      grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/pythonpath"
      ;;
    dart)
      [[ -f "$root/.zed/pub-deps.yaml" ]] || fail "dart adapter output missing"
      grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/pub-deps.yaml"
      ;;
    ruby|gleam)
      [[ -f "$root/.zed/paths.json" ]] || fail "$host universal paths index missing"
      ;;
  esac
}

run_case() {
  local host=$1
  local root="$projects/$host"
  local package
  local adapter
  package="$(package_for "$host")"
  adapter="$(adapter_for "$host")"
  write_project "$host" "$root"
  local marker="$root/$(marker_for "$host")"
  local marker_before
  marker_before="$(checksum "$marker")"

  extra=()
  if [[ "$host" == gleam || "$host" == dart ]]; then
    # No Gleam/Dart target exists in this fixture yet. Prove that the admission
    # boundary rejects a Rust target first, then exercise explicit universal
    # placement with the user's opt-in rather than silently weakening safety.
    if (
      cd "$root/deep/nested"
      ZED_PKG_HOME="$home/consumer" \
      ZED_PKG_REGISTRY="$registry_url" \
        "$zed" install "zed-pkg/$package@=0.2.0" \
          --skip-manifest \
          --install-mode copy \
          --adapter "$adapter"
    ); then
      fail "$host accepted a mismatched Rust target without explicit override"
    fi
    [[ ! -e "$root/.zpkg.lock" ]] || fail "$host mismatch failure wrote a lockfile"
    [[ ! -e "$root/zed_modules" ]] || fail "$host mismatch failure materialized a package"
    extra+=(--allow-ecosystem-mismatch)
  fi

  (
    cd "$root/deep/nested"
    ZED_PKG_HOME="$home/consumer" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install "zed-pkg/$package@=0.2.0" \
        --skip-manifest \
        --install-mode copy \
        --adapter "$adapter" \
        "${extra[@]}"
  )

  [[ ! -e "$root/.zpkg.toml" ]] || fail "$host install created .zpkg.toml"
  [[ -f "$root/.zpkg.lock" ]] || fail "$host install did not create .zpkg.lock"
  [[ "$(checksum "$marker")" == "$marker_before" ]] || fail "$host install modified its native manifest"
  [[ -d "$root/zed_modules/zed-pkg/$package" ]] || fail "$host package was not materialized"
  grep -Fq 'org = "zed-pkg"' "$root/.zpkg.lock"
  grep -Fq "name = \"$package\"" "$root/.zpkg.lock"
  grep -Fq 'version = "0.2.0"' "$root/.zpkg.lock"
  assert_adapter "$host" "$root" "$package"

  lock_before="$(checksum "$root/.zpkg.lock")"
  rm -rf -- "$root/zed_modules" "$root/node_modules" "$root/.zed"
  (
    cd "$root/deep/nested"
    ZED_PKG_HOME="$home/consumer" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install \
        --frozen \
        --skip-manifest \
        --install-mode copy \
        --adapter "$adapter" \
        "${extra[@]}"
  )
  [[ "$(checksum "$root/.zpkg.lock")" == "$lock_before" ]] || fail "$host frozen restore changed lockfile"
  [[ "$(checksum "$marker")" == "$marker_before" ]] || fail "$host frozen restore modified its native manifest"
  [[ -d "$root/zed_modules/zed-pkg/$package" ]] || fail "$host frozen restore did not materialize package"
  assert_adapter "$host" "$root" "$package"
  printf 'PASS: %-7s project detection, install, adapter, and frozen restore\n' "$host"
}

for host in npm golang rust ruby python gleam dart; do
  run_case "$host"
done

printf '\npolyglot consumer project shapes E2E: PASS\n'
