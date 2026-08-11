#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_WORKSPACE:?}"
: "${RUNNER_TEMP:?}"
: "${GITHUB_RUN_ID:?}"
: "${GITHUB_RUN_ATTEMPT:?}"
: "${GITHUB_ENV:?}"

zed_binary="$GITHUB_WORKSPACE/target/debug/zed-binary"
[[ -x "$zed_binary" ]]

ndk_home=${ANDROID_NDK_LATEST_HOME:-${ANDROID_NDK_HOME:-}}
if [[ -z "$ndk_home" ]]; then
  sdk_root=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-/usr/local/lib/android/sdk}}
  ndk_home=$(find "$sdk_root/ndk" -mindepth 1 -maxdepth 1 -type d 2>/dev/null \
    | sort -V | tail -n 1)
fi
[[ -n "$ndk_home" && -d "$ndk_home" ]]
clang="$ndk_home/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang"
[[ -x "$clang" ]]

org="r2-android-${GITHUB_RUN_ID}"
version="0.0.0-r2.${GITHUB_RUN_ID}.${GITHUB_RUN_ATTEMPT}"
package_dir="$RUNNER_TEMP/r2-android-package"
download_dir="$RUNNER_TEMP/r2-android-download"
pack_dir="$RUNNER_TEMP/r2-android-pack"
pack_dir_2="$RUNNER_TEMP/r2-android-pack-second"
mkdir -p "$package_dir/bin" "$download_dir" "$pack_dir" "$pack_dir_2"

cat >"$package_dir/main.c" <<'C'
#include <stdio.h>

int main(void) {
    puts("hello from zed-pkg Android ARM64 via Cloudflare R2");
    return 0;
}
C
"$clang" -O2 -fPIE -pie -Wl,-z,relro,-z,now \
  "$package_dir/main.c" -o "$package_dir/bin/r2-android-smoke"
chmod 0755 "$package_dir/bin/r2-android-smoke"

file "$package_dir/bin/r2-android-smoke" | tee "$RUNNER_TEMP/r2-android-file.txt"
grep -Eiq 'ELF 64-bit.*ARM aarch64' "$RUNNER_TEMP/r2-android-file.txt"
readelf -h "$package_dir/bin/r2-android-smoke" >"$RUNNER_TEMP/r2-android-elf-header.txt"
grep -Eq 'Class:[[:space:]]+ELF64' "$RUNNER_TEMP/r2-android-elf-header.txt"
grep -Eq 'Machine:[[:space:]]+AArch64' "$RUNNER_TEMP/r2-android-elf-header.txt"
readelf -l "$package_dir/bin/r2-android-smoke" >"$RUNNER_TEMP/r2-android-elf-program.txt"
grep -Fq '/system/bin/linker64' "$RUNNER_TEMP/r2-android-elf-program.txt"

cat >"$package_dir/.zpkg.toml" <<TOML
[package]
org = "$org"
name = "r2-android-smoke"
version = "$version"
description = "Ephemeral Android ARM64 binary certification against real Cloudflare R2"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/$GITHUB_REPOSITORY"

[bin]
r2-android-smoke = "bin/r2-android-smoke"
TOML
printf 'MIT fixture license\n' >"$package_dir/LICENSE"
printf 'target/\n.zed/\n' >"$package_dir/.gitignore"

git -C "$package_dir" init -q
git -C "$package_dir" config user.name "Zed Android R2 E2E"
git -C "$package_dir" config user.email "zed-r2-e2e@example.invalid"
git -C "$package_dir" add .
git -C "$package_dir" commit -qm "Android R2 fixture $version"
git -C "$package_dir" tag "v$version"
[[ -z $(git -C "$package_dir" status --porcelain) ]]
fixture_commit=$(git -C "$package_dir" rev-parse HEAD)

cd "$package_dir"
"$zed_binary" pack \
  --target aarch64-linux-android \
  --os android \
  --arch aarch64 \
  --abi api24 \
  --vcs-commit "$fixture_commit" \
  --out "$pack_dir" \
  --json | tee "$RUNNER_TEMP/r2-android-preflight.json"
"$zed_binary" pack \
  --target aarch64-linux-android \
  --os android \
  --arch aarch64 \
  --abi api24 \
  --vcs-commit "$fixture_commit" \
  --out "$pack_dir_2" \
  --json >"$RUNNER_TEMP/r2-android-preflight-second.json"

archive=$(jq -r .archive "$RUNNER_TEMP/r2-android-preflight.json")
archive_2=$(jq -r .archive "$RUNNER_TEMP/r2-android-preflight-second.json")
sha=$(jq -r .sha256 "$RUNNER_TEMP/r2-android-preflight.json")
sha_2=$(jq -r .sha256 "$RUNNER_TEMP/r2-android-preflight-second.json")
size=$(jq -r .size "$RUNNER_TEMP/r2-android-preflight.json")
[[ -f "$archive" && -f "$archive_2" ]]
[[ "$sha" =~ ^[0-9a-f]{64}$ && "$sha" == "$sha_2" ]]
cmp "$archive" "$archive_2"
[[ $(sha256sum "$archive" | awk '{print $1}') == "$sha" ]]

object_key="artifacts/${sha}.zip"
printf 'TEST_ORG=%s\nTEST_VERSION=%s\nPACKAGE_DIR=%s\nDOWNLOAD_DIR=%s\nPACK_DIR=%s\nPREFLIGHT_ARCHIVE=%s\nARTIFACT_SHA=%s\nARTIFACT_SIZE=%s\nOBJECT_KEY=%s\n' \
  "$org" "$version" "$package_dir" "$download_dir" "$pack_dir" \
  "$archive" "$sha" "$size" "$object_key" >>"$GITHUB_ENV"
