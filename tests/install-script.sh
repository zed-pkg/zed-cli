#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary="$(mktemp -d)"
cleanup() {
  find "$temporary" -depth -delete
}
trap cleanup EXIT

case "$(uname -s)" in
  Linux) target_os="unknown-linux-musl" ;;
  Darwin) target_os="apple-darwin" ;;
  *)
    printf 'unsupported installer-test operating system\n' >&2
    exit 1
    ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) target_arch="x86_64" ;;
  aarch64 | arm64) target_arch="aarch64" ;;
  *)
    printf 'unsupported installer-test architecture\n' >&2
    exit 1
    ;;
esac

asset="zed-${target_arch}-${target_os}.tar.gz"
fixture_dir="$temporary/fixture"
fake_bin="$temporary/fake-bin"
mkdir -p "$fixture_dir/payload" "$fake_bin"

cat >"$fixture_dir/payload/zed" <<'PAYLOAD'
#!/bin/sh
printf 'zed 0.1.0-installer-test\n'
PAYLOAD
chmod 0755 "$fixture_dir/payload/zed"
tar -C "$fixture_dir/payload" -czf "$fixture_dir/$asset" zed

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$fixture_dir" && sha256sum "$asset" >"$asset.sha256")
else
  (cd "$fixture_dir" && shasum -a 256 "$asset" >"$asset.sha256")
fi

cat >"$fake_bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail
url="${!#}"
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      output="$2"
      shift 2
      ;;
    *) shift ;;
  esac
done
if [ -z "$output" ]; then
  printf 'fake curl expected -o\n' >&2
  exit 2
fi
case "$url" in
  *.sha256) cp "$ZED_TEST_CHECKSUM" "$output" ;;
  *) cp "$ZED_TEST_ARCHIVE" "$output" ;;
esac
FAKE_CURL
chmod 0755 "$fake_bin/curl"

run_installer() {
  local home="$1"
  shift
  env \
    HOME="$home" \
    SHELL=/bin/zsh \
    PATH="$fake_bin:$PATH" \
    ZED_VERSION=v0.1.0-installer-test \
    ZED_TEST_ARCHIVE="$fixture_dir/$asset" \
    ZED_TEST_CHECKSUM="$fixture_dir/$asset.sha256" \
    NO_COLOR=1 \
    "$@" \
    bash "$repo_root/install.sh"
}

home="$temporary/home"
mkdir -p "$home"
run_installer "$home"
test -x "$home/.local/bin/zed"
test "$("$home/.local/bin/zed" --version)" = "zed 0.1.0-installer-test"
grep -Fx '# >>> zed-pkg PATH >>>' "$home/.zshrc"
grep -Fx "export PATH='$home/.local/bin':\"\$PATH\"" "$home/.zshrc"

# Reinstalling is idempotent: it neither duplicates nor rewrites the block.
run_installer "$home"
test "$(grep -Fc '# >>> zed-pkg PATH >>>' "$home/.zshrc")" -eq 1

# Bash uses its interactive profile when present and its login profile as the
# fallback, without touching Zsh configuration.
bash_home="$temporary/bash-home"
mkdir -p "$bash_home"
printf '# existing bash config\n' >"$bash_home/.bashrc"
run_installer "$bash_home" SHELL=/bin/bash
grep -Fx '# >>> zed-pkg PATH >>>' "$bash_home/.bashrc"
test ! -e "$bash_home/.zshrc"

bash_login_home="$temporary/bash-login-home"
mkdir -p "$bash_login_home"
printf '# existing bash login config\n' >"$bash_login_home/.bash_profile"
run_installer "$bash_login_home" SHELL=/bin/bash
grep -Fx '# >>> zed-pkg PATH >>>' "$bash_login_home/.bash_profile"
test ! -e "$bash_login_home/.bashrc"

# Automation and managed environments can opt out of startup-file mutation.
no_modify_home="$temporary/no-modify-home"
mkdir -p "$no_modify_home"
run_installer "$no_modify_home" ZED_NO_MODIFY_PATH=1
test -x "$no_modify_home/.local/bin/zed"
test ! -e "$no_modify_home/.zshrc"

# Conventional user-bin setup supplied by the OS or another tool needs no
# redundant shell-profile edit.
existing_path_home="$temporary/existing-path-home"
mkdir -p "$existing_path_home/.local/bin"
run_installer "$existing_path_home" \
  PATH="$existing_path_home/.local/bin:$fake_bin:$PATH"
test -x "$existing_path_home/.local/bin/zed"
test ! -e "$existing_path_home/.zshrc"

# A hostile-looking custom path is rendered as inert, single-quoted shell data.
quoted_home="$temporary/quoted-home"
quoted_dir="$quoted_home/quote's \$bin; echo unsafe"
mkdir -p "$quoted_home"
quoted_output="$(run_installer "$quoted_home" ZED_INSTALL_DIR="$quoted_dir")"
grep -Fx "export PATH='$quoted_home/quote'\\''s \$bin; echo unsafe':\"\$PATH\"" \
  "$quoted_home/.zshrc"
grep -F "try:  '$quoted_home/quote'\\''s \$bin; echo unsafe/zed' --help" \
  <<<"$quoted_output"
bash -n "$quoted_home/.zshrc"

# The downloaded checksum must name exactly the requested asset and match it.
bad_checksum="$fixture_dir/bad.sha256"
printf '%064d  %s\n' 0 "$asset" >"$bad_checksum"
bad_home="$temporary/bad-home"
mkdir -p "$bad_home"
if env \
  HOME="$bad_home" \
  SHELL=/bin/zsh \
  PATH="$fake_bin:$PATH" \
  ZED_VERSION=v0.1.0-installer-test \
  ZED_TEST_ARCHIVE="$fixture_dir/$asset" \
  ZED_TEST_CHECKSUM="$bad_checksum" \
  NO_COLOR=1 \
  bash "$repo_root/install.sh"
then
  printf 'installer accepted a mismatched checksum\n' >&2
  exit 1
fi
test ! -e "$bad_home/.local/bin/zed"

printf 'install.sh contract passed\n'
