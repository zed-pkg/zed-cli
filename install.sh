#!/usr/bin/env bash
#
# zed-cli installer — install the `zed` binary with curl | bash:
#
#   curl -fsSL https://raw.githubusercontent.com/zed-pkg/zed-cli/main/install.sh | bash
#
# What it does:
#   * detects your OS/arch and picks the matching release target
#   * downloads the latest zed-<target>.tar.gz from the GitHub Release
#   * installs the `zed` binary into ~/.local/bin
#   * adds ~/.local/bin to PATH in your shell profile when needed (idempotent)
#
# NOTE: ~/.local/bin also holds executables from `zed global install`, so a
# user needs at most one Zed-related PATH entry. Package profiles and the
# content-addressed store remain under ZED_PKG_HOME (default: ~/.zed-pkg).
#
# Environment overrides:
#   ZED_VERSION       pin a release tag (e.g. v0.1.0) instead of latest
#   ZED_INSTALL_DIR   absolute install location (default: ~/.local/bin)
#   ZED_PROFILE       shell profile to edit for PATH (default: auto-detected)
#   ZED_NO_MODIFY_PATH 1 to leave shell startup files unchanged
#   NO_COLOR          disable colored output
set -euo pipefail

REPO="zed-pkg/zed-cli"
EXE_NAME="zed"
INSTALL_DIR="${ZED_INSTALL_DIR:-$HOME/.local/bin}"

# --- output helpers --------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  c_reset="$(printf '\033[0m')"
  c_bold="$(printf '\033[1m')"
  c_red="$(printf '\033[31m')"
  c_green="$(printf '\033[32m')"
  c_yellow="$(printf '\033[33m')"
  c_blue="$(printf '\033[34m')"
else
  c_reset="" c_bold="" c_red="" c_green="" c_yellow="" c_blue=""
fi

info()    { printf '%s==>%s %s\n' "$c_blue" "$c_reset" "$*"; }
success() { printf '%s+%s %s\n' "$c_green" "$c_reset" "$*"; }
warn()    { printf '%swarning:%s %s\n' "$c_yellow" "$c_reset" "$*" >&2; }
error()   { printf '%serror:%s %s\n' "$c_red" "$c_reset" "$*" >&2; }

# --- cleanup ---------------------------------------------------------------
workdir=""
cleanup() {
  if [ -n "$workdir" ] && [ -d "$workdir" ]; then
    find "$workdir" -depth -delete
  fi
}
trap cleanup EXIT

# --- prerequisites ---------------------------------------------------------
require() {
  command -v "$1" >/dev/null 2>&1 || {
    error "'$1' is required but was not found in PATH."
    exit 1
  }
}
require curl
require find
require install
require tar

case "$INSTALL_DIR" in
  /*) ;;
  *)
    error "ZED_INSTALL_DIR must be an absolute path: $INSTALL_DIR"
    exit 1
    ;;
esac
case "$INSTALL_DIR" in
  *$'\n'* | *$'\r'*)
    error "ZED_INSTALL_DIR must not contain newlines."
    exit 1
    ;;
esac

case "${ZED_NO_MODIFY_PATH:-0}" in
  "" | 0 | false | no) modify_path=1 ;;
  1 | true | yes) modify_path=0 ;;
  *)
    error "ZED_NO_MODIFY_PATH must be 0/1, true/false, or yes/no."
    exit 1
    ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  checksum_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  checksum_tool="shasum"
else
  error "'sha256sum' or 'shasum' is required to verify the release archive."
  exit 1
fi

# --- detect platform -------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux) target_os="unknown-linux-musl" ;;
  Darwin) target_os="apple-darwin" ;;
  *)
    error "unsupported operating system: $os (Linux and macOS only)"
    exit 1
    ;;
esac

case "$arch" in
  x86_64 | amd64) target_arch="x86_64" ;;
  aarch64 | arm64) target_arch="aarch64" ;;
  *)
    error "unsupported architecture: $arch (x86_64 and aarch64 only)"
    exit 1
    ;;
esac

target="${target_arch}-${target_os}"

# --- resolve release tag ---------------------------------------------------
resolve_tag() {
  if [ -n "${ZED_VERSION:-}" ]; then
    printf '%s' "$ZED_VERSION"
    return
  fi
  # GitHub redirects /releases/latest to /releases/tag/<tag>. Reading the final
  # URL needs no API token, so it never hits the unauthenticated rate limit.
  local eff
  if ! eff="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"; then
    error "could not reach GitHub to determine the latest release."
    exit 1
  fi
  local tag="${eff##*/}"
  if [ -z "$tag" ] || [ "$tag" = "latest" ] || [ "$tag" = "releases" ]; then
    error "no published release found for ${REPO}."
    exit 1
  fi
  printf '%s' "$tag"
}

printf '%szed-cli installer%s\n' "$c_bold" "$c_reset"
info "platform: ${os} ${arch}  ->  target ${target}"

tag="$(resolve_tag)"
case "$tag" in
  "" | *[!A-Za-z0-9._-]*)
    error "invalid release tag: $tag"
    exit 1
    ;;
esac
asset="${EXE_NAME}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
info "release:  ${tag}"

# --- download & extract ----------------------------------------------------
workdir="$(mktemp -d)"
info "downloading ${asset} ..."
if ! curl -fSL --proto '=https' --tlsv1.2 -o "${workdir}/${asset}" "$url"; then
  error "download failed: ${url}"
  exit 1
fi

checksum="${asset}.sha256"
if ! curl -fSL --proto '=https' --tlsv1.2 -o "${workdir}/${checksum}" "${url}.sha256"; then
  error "checksum download failed: ${url}.sha256"
  exit 1
fi
IFS=' ' read -r expected_checksum checksum_name <"${workdir}/${checksum}" || true
checksum_name="${checksum_name# }"
checksum_name="${checksum_name#\*}"
case "$expected_checksum" in
  "" | *[!a-f0-9]*)
    error "release checksum for ${asset} is malformed."
    exit 1
    ;;
esac
if [ "${#expected_checksum}" -ne 64 ] || [ "$checksum_name" != "$asset" ]; then
  error "release checksum does not name exactly ${asset}."
  exit 1
fi
if [ "$checksum_tool" = "sha256sum" ]; then
  actual_line="$(sha256sum "${workdir}/${asset}")"
else
  actual_line="$(shasum -a 256 "${workdir}/${asset}")"
fi
IFS=' ' read -r actual_checksum _ <<<"$actual_line"
if [ "$actual_checksum" != "$expected_checksum" ]; then
  error "checksum verification failed for ${asset}."
  exit 1
fi
success "verified ${asset} checksum"

# Extract only the expected file to stdout. This avoids materializing any
# archive paths supplied by a compromised or malformed release.
if ! tar -xOzf "${workdir}/${asset}" -- "$EXE_NAME" >"${workdir}/${EXE_NAME}"; then
  error "archive ${asset} did not contain a regular '${EXE_NAME}' payload."
  exit 1
fi
if [ ! -s "${workdir}/${EXE_NAME}" ]; then
  error "archive ${asset} did not contain a '${EXE_NAME}' binary."
  exit 1
fi

# --- install ---------------------------------------------------------------
existing_zed="$(command -v "$EXE_NAME" 2>/dev/null || true)"
if [ -n "$existing_zed" ] && [ "$existing_zed" != "${INSTALL_DIR}/${EXE_NAME}" ]; then
  warn "another 'zed' command is already available at ${existing_zed}. The Zed editor uses the same command name; PATH order will decide which one runs."
fi
mkdir -p "$INSTALL_DIR"
install -m 0755 "${workdir}/${EXE_NAME}" "${INSTALL_DIR}/${EXE_NAME}"
success "installed ${EXE_NAME} -> ${INSTALL_DIR}/${EXE_NAME}"

# --- PATH injection (idempotent) -------------------------------------------
detect_profile() {
  if [ -n "${ZED_PROFILE:-}" ]; then
    printf '%s' "$ZED_PROFILE"
    return
  fi
  case "${SHELL##*/}" in
    zsh) printf '%s' "${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash)
      if [ -f "$HOME/.bashrc" ]; then
        printf '%s' "$HOME/.bashrc"
      elif [ -f "$HOME/.bash_profile" ]; then
        printf '%s' "$HOME/.bash_profile"
      else
        printf '%s' "$HOME/.profile"
      fi
      ;;
    *) printf '%s' "$HOME/.profile" ;;
  esac
}

shell_single_quote() {
  local value="${1//\'/\'\\\'\'}"
  printf "'%s'" "$value"
}

# Recognize older unmarked PATH lines before writing the managed block. New
# writes use an exact marker so comments or similarly named directories do not
# create duplicate entries on later installer runs.
case "$INSTALL_DIR" in
  "$HOME"/*) path_marker="\$HOME/${INSTALL_DIR#"$HOME"/}" ;;
  *) path_marker="$INSTALL_DIR" ;;
esac

path_block_begin="# >>> zed-pkg PATH >>>"
path_block_end="# <<< zed-pkg PATH <<<"
path_is_on_path=0
IFS=: read -r -a current_path_entries <<<"${PATH:-}"
for path_entry in "${current_path_entries[@]}"; do
  if [ "$path_entry" = "$INSTALL_DIR" ]; then
    path_is_on_path=1
    break
  fi
done
if [ "$modify_path" -eq 0 ]; then
  info "left shell startup files unchanged (ZED_NO_MODIFY_PATH=1)"
elif [ "$path_is_on_path" -eq 1 ]; then
  info "${INSTALL_DIR} is already on PATH; no shell profile change needed"
else
  profile="$(detect_profile)"
  if [ -d "$profile" ]; then
    error "shell profile path is a directory: ${profile}"
    exit 1
  fi
  mkdir -p "$(dirname "$profile")"
  if [ -f "$profile" ] && grep -Fqx "$path_block_begin" "$profile"; then
    info "PATH already configured in ${profile}"
  elif [ -f "$profile" ] && grep -Fq "$path_marker" "$profile"; then
    info "${profile} already references ${INSTALL_DIR}"
  else
    quoted_install_dir="$(shell_single_quote "$INSTALL_DIR")"
    {
      printf '\n%s\n' "$path_block_begin"
      # shellcheck disable=SC2016  # write $PATH literally; it expands at shell init
      printf 'export PATH=%s:"$PATH"\n' "$quoted_install_dir"
      printf '%s\n' "$path_block_end"
    } >>"$profile"
    success "added ${INSTALL_DIR} to PATH in ${profile}"
  fi
fi

# --- done ------------------------------------------------------------------
printf '\n'
if version_line="$("${INSTALL_DIR}/${EXE_NAME}" --version 2>/dev/null)"; then
  success "${version_line} is ready"
else
  success "${EXE_NAME} is ready"
fi

case "$path_is_on_path" in
  1) : ;;
  0)
    info "restart your shell, or run this to use it now:"
    # shellcheck disable=SC2016  # print $PATH literally as a copy-paste command
    printf '    export PATH=%s:"$PATH"\n' "$(shell_single_quote "$INSTALL_DIR")"
    ;;
esac
if [ "$(command -v "$EXE_NAME" 2>/dev/null || true)" = "${INSTALL_DIR}/${EXE_NAME}" ]; then
  info "try:  ${EXE_NAME} --help"
else
  info "try:  $(shell_single_quote "${INSTALL_DIR}/${EXE_NAME}") --help"
fi
