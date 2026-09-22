#!/bin/sh
set -eu

target="${ZED_PKG_TEST_TARGET:?ZED_PKG_TEST_TARGET is required}"

test -f "$target/.auth-shared.toml"
test ! -e "$target/.shared-auth.toml"
test -x "$target/target/release/zed"
test -x "$target/target/release/zed-gitops"
test -x "$target/target/release/zed-git-install"
"$target/target/release/zed" --version
"$target/target/release/zed" validate --manifest "$target/.zpkg.toml" --lock "$target/.zpkg.lock" --json >/dev/null
"$target/target/release/zed" gitops validate --help
"$target/target/release/zed-git-install" --help >/dev/null
