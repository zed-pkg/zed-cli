#!/usr/bin/env bash
#
# zed-cli installer — install the `zed` binary with curl | bash:
#
#   curl -fsSL https://raw.githubusercontent.com/zed-pkg/zed-cli/main/install.sh | bash
#
# What it does:
#   * detects your OS/arch and picks the matching release target
#   * downloads the latest zed-<target>.tar.gz from the GitHub Release
#   * installs the `zed` binary into ~/.zed/bin
#   * adds ~/.zed/bin to PATH in your shell profile (idempotent)
#
# NOTE: ~/.zed/bin holds the `zed` executable (what lands on PATH). It is
# deliberately separate from ~/.zed-pkg, the content-addressed package store
# that `zed` itself manages (ZED_PKG_HOME). Do not conflate the two.
#
# Environment overrides:
#   ZED_VERSION       pin a release tag (e.g. v0.1.0) instead of latest
#   ZED_INSTALL_DIR   install location (default: ~/.zed/bin)
#   ZED_PROFILE       shell profile to edit for PATH (default: auto-detected)
#   NO_COLOR          disable colored output
set -euo pipefail

REPO="zed-pkg/zed-cli"
EXE_NAME="zed"
INSTALL_DIR="${ZED_INSTALL_DIR:-$HOME/.zed/bin}"

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
  if [ -n "$workdir" ]; then rm -rf "$workdir"; fi
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
require tar

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

tar -xzf "${workdir}/${asset}" -C "$workdir"
if [ ! -f "${workdir}/${EXE_NAME}" ]; then
  error "archive ${asset} did not contain a '${EXE_NAME}' binary."
  exit 1
fi

# --- install ---------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
mv -f "${workdir}/${EXE_NAME}" "${INSTALL_DIR}/${EXE_NAME}"
chmod +x "${INSTALL_DIR}/${EXE_NAME}"
success "installed ${EXE_NAME} -> ${INSTALL_DIR}/${EXE_NAME}"

# --- PATH injection (idempotent) -------------------------------------------
detect_profile() {
  if [ -n "${ZED_PROFILE:-}" ]; then
    printf '%s' "$ZED_PROFILE"
    return
  fi
  case "$(basename "${SHELL:-}")" in
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

# Portable marker: reference $HOME literally when the dir lives under it, so the
# written profile line survives a home-directory path change.
case "$INSTALL_DIR" in
  "$HOME"/*) path_marker="\$HOME/${INSTALL_DIR#"$HOME"/}" ;;
  *) path_marker="$INSTALL_DIR" ;;
esac

profile="$(detect_profile)"
if [ -f "$profile" ] && grep -Fq "$path_marker" "$profile"; then
  info "PATH already configured in ${profile}"
else
  {
    printf '\n# Added by the zed-cli installer\n'
    # shellcheck disable=SC2016  # write $PATH literally; it expands at shell init
    printf 'export PATH="%s:$PATH"\n' "$path_marker"
  } >>"$profile"
  success "added ${INSTALL_DIR} to PATH in ${profile}"
fi

# --- done ------------------------------------------------------------------
printf '\n'
if version_line="$("${INSTALL_DIR}/${EXE_NAME}" --version 2>/dev/null)"; then
  success "${version_line} is ready"
else
  success "${EXE_NAME} is ready"
fi

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) : ;; # already on PATH for this session
  *)
    info "restart your shell, or run this to use it now:"
    # shellcheck disable=SC2016  # print $PATH literally as a copy-paste command
    printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    ;;
esac
info "try:  ${EXE_NAME} --help"
