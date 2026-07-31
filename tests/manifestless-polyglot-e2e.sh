#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: tests/manifestless-polyglot-e2e.sh /absolute/path/to/zed" >&2
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
if [[ ! -f "$fixture_source/.zpkg.toml" ]]; then
  echo "polyglot fixture not found: $fixture_source" >&2
  exit 2
fi

remove_suite_root=false
if [[ -n "${ZED_MANIFESTLESS_E2E_ROOT:-}" ]]; then
  suite_root=$ZED_MANIFESTLESS_E2E_ROOT
  if [[ -e "$suite_root" ]]; then
    echo "ZED_MANIFESTLESS_E2E_ROOT must not already exist: $suite_root" >&2
    exit 2
  fi
  mkdir -p "$suite_root"
else
  suite_root="$(mktemp -d "${TMPDIR:-/tmp}/zed-manifestless-polyglot.XXXXXX")"
  remove_suite_root=true
fi

cleanup() {
  if $remove_suite_root && [[ "${ZED_MANIFESTLESS_E2E_KEEP:-0}" != 1 ]]; then
    rm -rf -- "$suite_root"
  else
    printf 'manifestless polyglot E2E workspace: %s\n' "$suite_root"
  fi
}
trap cleanup EXIT

fixture="$suite_root/polyglot fixture"
registry="$suite_root/registry"
home="$suite_root/zed home"
projects="$suite_root/consumer projects"
cp -R "$fixture_source" "$fixture"
mkdir -p "$registry" "$home" "$projects"

(
  cd "$fixture"
  ZED_PKG_HOME="$home/author" \
    "$zed" publish \
      --registry "file://$registry" \
      --skip-vcs-checks
)

for target in nodejs python golang rust ruby; do
  test -d "$registry/packages/zed-pkg/poly-fixture-$target"
done

registry_url="file://$registry"

fail() {
  echo "manifestless polyglot E2E: $*" >&2
  exit 1
}

checksum() {
  cksum < "$1"
}

install_from() {
  local invocation=$1
  local package=$2
  local mode=$3
  shift 3
  (
    cd "$invocation"
    ZED_PKG_HOME="$home/consumer" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install "$package@=0.2.0" \
        --skip-manifest \
        --install-mode "$mode" \
        "$@"
  )
}

frozen_from() {
  local invocation=$1
  local mode=$2
  (
    cd "$invocation"
    ZED_PKG_HOME="$home/consumer" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install \
        --frozen \
        --skip-manifest \
        --install-mode "$mode"
  )
}

assert_common_install() {
  local root=$1
  local package=$2
  local marker=$3
  local marker_before=$4
  local mode=$5
  local package_root="$root/zed_modules/zed-pkg/$package"

  [[ ! -e "$root/.zpkg.toml" ]] || fail "$package created .zpkg.toml"
  [[ -f "$root/.zpkg.lock" ]] || fail "$package did not create .zpkg.lock"
  [[ -d "$package_root" || -L "$package_root" ]] || fail "$package was not installed"
  [[ "$(checksum "$marker")" == "$marker_before" ]] || fail "$package modified $marker"
  grep -Fq 'org = "zed-pkg"' "$root/.zpkg.lock"
  grep -Fq "name = \"$package\"" "$root/.zpkg.lock"
  grep -Fq 'version = "0.2.0"' "$root/.zpkg.lock"

  if [[ "$mode" == symlink ]]; then
    [[ -L "$package_root" ]] || fail "$package was expected to be a symlink"
  else
    [[ ! -L "$package_root" ]] || fail "$package was expected to be copied"
    [[ -z "$(find "$package_root" -type l -print -quit)" ]] || fail "$package copy contains a symlink"
  fi
}

restore_from_lock() {
  local root=$1
  local invocation=$2
  local package=$3
  local marker=$4
  local marker_before=$5
  local mode=$6
  local lock_before
  lock_before="$(checksum "$root/.zpkg.lock")"

  rm -rf -- "$root/zed_modules" "$root/node_modules" "$root/.zed"
  frozen_from "$invocation" "$mode"

  [[ "$(checksum "$root/.zpkg.lock")" == "$lock_before" ]] || fail "$package changed its frozen lock"
  assert_common_install "$root" "$package" "$marker" "$marker_before" "$mode"
}

run_node_case() {
  local root="$projects/node app"
  local invocation="$root/src/deep"
  local package=poly-fixture-nodejs
  mkdir -p "$invocation"
  printf '%s\n' '{"name":"manifestless-node-consumer","private":true}' > "$root/package.json"
  local marker_before
  marker_before="$(checksum "$root/package.json")"

  install_from "$invocation" "zed-pkg/$package" symlink
  assert_common_install "$root" "$package" "$root/package.json" "$marker_before" symlink
  [[ -L "$root/node_modules/@zed-pkg/$package" ]] || fail "Node adapter did not create its scoped link"
  (
    cd "$root"
    node - <<'NODE'
const { greet } = require("@zed-pkg/poly-fixture-nodejs");
if (greet("zed") !== "hello, zed") process.exit(1);
NODE
  )

  restore_from_lock "$root" "$invocation" "$package" "$root/package.json" "$marker_before" symlink
  (
    cd "$root"
    node -e 'const {greet}=require("@zed-pkg/poly-fixture-nodejs"); if (greet("zed") !== "hello, zed") process.exit(1)'
  )
  printf 'PASS: node adapter and frozen restore\n'
}

run_python_case() {
  local root="$projects/python app"
  local invocation="$root/src/deep"
  local package=poly-fixture-python
  mkdir -p "$invocation"
  cat > "$root/pyproject.toml" <<'TOML'
[project]
name = "manifestless-python-consumer"
version = "0.0.0"
TOML
  local marker_before
  marker_before="$(checksum "$root/pyproject.toml")"

  install_from "$invocation" "zed-pkg/$package" copy
  assert_common_install "$root" "$package" "$root/pyproject.toml" "$marker_before" copy
  [[ -f "$root/.zed/pythonpath" ]] || fail "Python adapter did not create .zed/pythonpath"
  grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/pythonpath"
  (
    cd "$root"
    PYTHONPATH="$(cat .zed/pythonpath)" python -c \
      'from zed_poly import greet; assert greet("zed") == "hello, zed"'
  )

  restore_from_lock "$root" "$invocation" "$package" "$root/pyproject.toml" "$marker_before" copy
  (
    cd "$root"
    PYTHONPATH="$(cat .zed/pythonpath)" python -c \
      'from zed_poly import greet; assert greet("zed") == "hello, zed"'
  )
  printf 'PASS: python adapter and frozen restore\n'
}

run_go_case() {
  local root="$projects/go app"
  local invocation="$root/cmd/app/deep"
  local package=poly-fixture-golang
  mkdir -p "$invocation" "$root/cmd/app"
  cat > "$root/go.mod" <<'GOMOD'
module example.com/manifestless-consumer

go 1.22
GOMOD
  cat > "$root/cmd/app/main.go" <<'GO'
package main

import (
  "fmt"
  poly "github.com/zed-pkg/poly-fixture/go"
)

func main() { fmt.Print(poly.Greet("zed")) }
GO
  local marker_before
  marker_before="$(checksum "$root/go.mod")"

  install_from "$invocation" "zed-pkg/$package" copy
  assert_common_install "$root" "$package" "$root/go.mod" "$marker_before" copy
  [[ -f "$root/.zed/go.work" ]] || fail "Go adapter did not create .zed/go.work"
  grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/go.work"
  [[ "$(cd "$root" && GOWORK="$root/.zed/go.work" go run ./cmd/app)" == "hello, zed" ]]

  restore_from_lock "$root" "$invocation" "$package" "$root/go.mod" "$marker_before" copy
  [[ "$(cd "$root" && GOWORK="$root/.zed/go.work" go run ./cmd/app)" == "hello, zed" ]]
  printf 'PASS: go adapter and frozen restore\n'
}

run_rust_case() {
  local root="$projects/rust app"
  local invocation="$root/src/deep"
  local package=poly-fixture-rust
  mkdir -p "$invocation" "$root/src"
  cat > "$root/Cargo.toml" <<'TOML'
[package]
name = "manifestless-rust-consumer"
version = "0.0.0"
edition = "2021"

[dependencies]
zed-poly-fixture = "=0.2.0"
TOML
  cat > "$root/src/main.rs" <<'RS'
fn main() {
    print!("{}", zed_poly_fixture::greet("zed"));
}
RS
  local marker_before
  marker_before="$(checksum "$root/Cargo.toml")"

  install_from "$invocation" "zed-pkg/$package" copy
  assert_common_install "$root" "$package" "$root/Cargo.toml" "$marker_before" copy
  [[ -f "$root/.zed/cargo-paths.toml" ]] || fail "Rust adapter did not create .zed/cargo-paths.toml"
  grep -Fq "zed_modules/zed-pkg/$package" "$root/.zed/cargo-paths.toml"
  mkdir -p "$root/.cargo"
  cp "$root/.zed/cargo-paths.toml" "$root/.cargo/config.toml"
  [[ "$(cd "$root" && cargo run --offline --quiet)" == "hello, zed" ]]

  restore_from_lock "$root" "$invocation" "$package" "$root/Cargo.toml" "$marker_before" copy
  cp "$root/.zed/cargo-paths.toml" "$root/.cargo/config.toml"
  [[ "$(cd "$root" && cargo run --offline --quiet)" == "hello, zed" ]]
  printf 'PASS: rust adapter and frozen restore\n'
}

run_ruby_case() {
  local root="$projects/ruby app"
  local invocation="$root/lib/deep"
  local package=poly-fixture-ruby
  mkdir -p "$invocation"
  printf "source 'https://rubygems.org'\n" > "$root/Gemfile"
  local marker_before
  marker_before="$(checksum "$root/Gemfile")"

  install_from "$invocation" "zed-pkg/$package" symlink
  assert_common_install "$root" "$package" "$root/Gemfile" "$marker_before" symlink
  [[ ! -e "$root/.zed/pythonpath" ]]
  [[ ! -e "$root/.zed/go.work" ]]
  [[ ! -e "$root/.zed/cargo-paths.toml" ]]
  [[ "$(ruby -I"$root/zed_modules/zed-pkg/$package/lib" -e 'require "zed_poly"; print ZedPoly.greet("zed")')" == "hello, zed" ]]

  restore_from_lock "$root" "$invocation" "$package" "$root/Gemfile" "$marker_before" symlink
  [[ "$(ruby -I"$root/zed_modules/zed-pkg/$package/lib" -e 'require "zed_poly"; print ZedPoly.greet("zed")')" == "hello, zed" ]]
  printf 'PASS: universal/ruby placement and frozen restore\n'
}

run_negative_spec_cases() {
  local root="$projects/negative specs"
  mkdir -p "$root"
  printf '{"name":"negative-spec-consumer","private":true}\n' > "$root/package.json"

  for args in \
    'zed-pkg/poly-fixture-nodejs@' \
    'zed-pkg/poly-fixture-nodejs/extra@=0.2.0'
  do
    if (
      cd "$root"
      ZED_PKG_HOME="$home/consumer" \
      ZED_PKG_REGISTRY="$registry_url" \
        "$zed" install "$args" --skip-manifest --install-mode copy
    ); then
      fail "invalid package operand unexpectedly succeeded: $args"
    fi
    [[ ! -e "$root/.zpkg.lock" ]]
    [[ ! -e "$root/zed_modules" ]]
    [[ ! -e "$root/node_modules" ]]
  done

  if (
    cd "$root"
    ZED_PKG_HOME="$home/consumer" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install \
        'zed-pkg/poly-fixture-nodejs@=0.2.0' \
        'zed-pkg/poly-fixture-nodejs@=9.9.9' \
        --skip-manifest \
        --install-mode copy
  ); then
    fail "conflicting requirements unexpectedly succeeded"
  fi
  [[ ! -e "$root/.zpkg.lock" ]]
  [[ ! -e "$root/zed_modules" ]]
  printf 'PASS: invalid and conflicting package operands fail atomically\n'
}

run_ambiguous_monorepo_case() {
  local root="$projects/ambiguous monorepo"
  mkdir -p "$root/apps/web" "$root/services/api"
  printf '{"name":"web","private":true}\n' > "$root/apps/web/package.json"
  cat > "$root/services/api/Cargo.toml" <<'TOML'
[package]
name = "api"
version = "0.0.0"
edition = "2021"
TOML

  install_from "$root" 'zed-pkg/poly-fixture-ruby' copy
  [[ -f "$root/.zpkg.lock" ]] || fail "ambiguous monorepo did not keep the requested root"
  [[ ! -e "$root/apps/web/.zpkg.lock" ]]
  [[ ! -e "$root/services/api/.zpkg.lock" ]]
  [[ -d "$root/zed_modules/zed-pkg/poly-fixture-ruby" ]]
  printf 'PASS: ambiguous monorepo falls back to the requested root\n'
}

run_concurrent_store_case() {
  local parallel_root="$projects/concurrent"
  mkdir -p "$parallel_root"
  local pids=()
  for index in 1 2 3 4 5 6; do
    local root="$parallel_root/app-$index"
    mkdir -p "$root/src/deep"
    printf '{"name":"parallel-%s","private":true}\n' "$index" > "$root/package.json"
    (
      install_from "$root/src/deep" 'zed-pkg/poly-fixture-nodejs' copy
    ) >"$suite_root/parallel-$index.log" 2>&1 &
    pids+=("$!")
  done

  local failed=0
  for pid in "${pids[@]}"; do
    if ! wait "$pid"; then
      failed=1
    fi
  done
  if [[ $failed -ne 0 ]]; then
    cat "$suite_root"/parallel-*.log >&2
    fail "parallel installs sharing one store failed"
  fi
  for index in 1 2 3 4 5 6; do
    local root="$parallel_root/app-$index"
    [[ -f "$root/.zpkg.lock" ]]
    [[ -d "$root/zed_modules/zed-pkg/poly-fixture-nodejs" ]]
    [[ -d "$root/node_modules/@zed-pkg/poly-fixture-nodejs" ]]
  done
  printf 'PASS: concurrent manifestless installs share one store safely\n'
}

run_node_case
run_python_case
run_go_case
run_rust_case
run_ruby_case
run_negative_spec_cases
run_ambiguous_monorepo_case
run_concurrent_store_case

printf '\nmanifestless polyglot E2E: PASS\n'
