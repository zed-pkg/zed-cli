#!/usr/bin/env bash
set -euo pipefail

: "${GITHUB_WORKSPACE:?}"
: "${RUNNER_TEMP:?}"
: "${GITHUB_ENV:?}"
: "${DATABASE_URL:?}"
: "${R2_BUCKET:?}"
: "${S3_ENDPOINT_URL:?}"
: "${OBJECT_KEY:?}"
: "${ARTIFACT_SHA:?}"
: "${ARTIFACT_SIZE:?}"
: "${TEST_ORG:?}"
: "${TEST_VERSION:?}"
: "${PACKAGE_DIR:?}"
: "${DOWNLOAD_DIR:?}"
: "${PACK_DIR:?}"
: "${AWS_SESSION_TOKEN:?temporary R2 credentials require a session token}"

zed="$GITHUB_WORKSPACE/target/debug/zed"
zed_binary="$GITHUB_WORKSPACE/target/debug/zed-binary"
server="$GITHUB_WORKSPACE/registry-server/target/debug/zed-api-server"
server_log="$RUNNER_TEMP/zed-api-r2-android.log"
server_pid_file="$RUNNER_TEMP/zed-api-r2-android.pid"

cleanup() {
  status=$?
  set +e
  if [[ -f "$server_pid_file" ]]; then
    kill "$(cat "$server_pid_file")" 2>/dev/null || true
  fi
  if [[ -n "${AWS_SESSION_TOKEN:-}" ]]; then
    aws --endpoint-url "$S3_ENDPOINT_URL" s3api delete-object \
      --bucket "$R2_BUCKET" --key "$OBJECT_KEY" >/dev/null 2>&1 || true
  fi
  if [[ -f "$server_log" ]]; then
    sed -E \
      -e 's#https://[^[:space:]]+\?[^[:space:]]+#<redacted-presigned-url>#g' \
      -e 's/(X-Amz-[A-Za-z-]+=)[^&[:space:]]+/\1<redacted>/g' \
      "$server_log" >"$RUNNER_TEMP/zed-api-r2-android-redacted.log"
  fi
  rm -f "$RUNNER_TEMP/r2-parent-delegation.json" \
    "$RUNNER_TEMP/r2-temp-request.json" \
    "$RUNNER_TEMP/r2-temp-response.json"
  printf 'AWS_ACCESS_KEY_ID=\nAWS_SECRET_ACCESS_KEY=\nAWS_SESSION_TOKEN=\nZED_PKG_TOKEN=\n' \
    >>"$GITHUB_ENV"
  exit "$status"
}
trap cleanup EXIT

for binary in "$zed" "$zed_binary" "$server"; do
  [[ -x "$binary" ]]
done

if aws --endpoint-url "$S3_ENDPOINT_URL" s3api list-buckets \
    >"$RUNNER_TEMP/r2-unexpected-list.json" 2>"$RUNNER_TEMP/r2-list-denied.log"; then
  echo 'exact-object temporary credential unexpectedly allowed ListBuckets' >&2
  exit 1
fi
rm -f "$RUNNER_TEMP/r2-unexpected-list.json" "$RUNNER_TEMP/r2-list-denied.log"

export AUTO_MIGRATE=true
export BIND_ADDR=127.0.0.1:8080
export PUBLIC_BASE_URL=http://127.0.0.1:8080
export STORAGE_BACKEND=s3
export S3_BUCKET="$R2_BUCKET"
export S3_REGION=auto
export S3_FORCE_PATH_STYLE=true
export ZED_VERIFY_TAGS=off
export ZED_AUTH_DISABLED=1
export ZED_RATE_LIMIT_DISABLED=1
export DB_CONNECT_MAX_WAIT_SECS=30
export RUST_LOG=info

"$server" >"$server_log" 2>&1 &
echo $! >"$server_pid_file"
for _ in $(seq 1 60); do
  if curl -fsS http://127.0.0.1:8080/healthz >/dev/null; then
    break
  fi
  sleep 1
done
curl -fsS http://127.0.0.1:8080/healthz >/dev/null || {
  sed -E 's#https://[^[:space:]]+\?[^[:space:]]+#<redacted-url>#g' "$server_log" >&2
  exit 1
}

export ZED_PKG_REGISTRY=http://127.0.0.1:8080
export ZED_PKG_INTERACTIVE=false
output=$("$server" create-token --name real-r2-android-e2e --expires-in-days 1)
token=$(printf '%s\n' "$output" | tail -n 1)
case "$token" in
  zpkg_*) ;;
  *) printf '%s\n' "$output" >&2; exit 1 ;;
esac
echo "::add-mask::$token"
export ZED_PKG_TOKEN="$token"
printf 'ZED_PKG_TOKEN=%s\n' "$token" >>"$GITHUB_ENV"
"$zed" org claim "$TEST_ORG"

cd "$PACKAGE_DIR"
"$zed_binary" publish \
  --target aarch64-linux-android \
  --os android \
  --arch aarch64 \
  --abi api24 \
  --out "$PACK_DIR" \
  --json | tee "$RUNNER_TEMP/r2-android-publish.json"
jq -e \
  --arg package "$TEST_ORG/r2-android-smoke" \
  --arg version "$TEST_VERSION" \
  --arg sha "$ARTIFACT_SHA" \
  --argjson size "$ARTIFACT_SIZE" '
    .package == $package
    and .version == $version
    and .target == "aarch64-linux-android"
    and .uploaded == true
    and .sha256 == $sha
    and .size == $size
  ' "$RUNNER_TEMP/r2-android-publish.json" >/dev/null

aws --endpoint-url "$S3_ENDPOINT_URL" s3api head-object \
  --bucket "$R2_BUCKET" --key "$OBJECT_KEY" \
  >"$RUNNER_TEMP/r2-android-head.json"
jq -e --argjson size "$ARTIFACT_SIZE" '
  .ContentLength == $size
  and .ContentType == "application/zip"
  and .CacheControl == "public, max-age=31536000, immutable"
' "$RUNNER_TEMP/r2-android-head.json" >/dev/null

registry_archive="$DOWNLOAD_DIR/registry.zip"
direct_archive="$DOWNLOAD_DIR/direct-r2.zip"
presigned_archive="$DOWNLOAD_DIR/presigned-mobile.zip"
"$zed_binary" download \
  "$TEST_ORG/r2-android-smoke@$TEST_VERSION" \
  --out "$registry_archive" \
  --target aarch64-linux-android \
  --json | tee "$RUNNER_TEMP/r2-android-download.json"
aws --endpoint-url "$S3_ENDPOINT_URL" s3api get-object \
  --bucket "$R2_BUCKET" --key "$OBJECT_KEY" "$direct_archive" \
  >"$RUNNER_TEMP/r2-android-get.json"
presigned_url=$(aws --endpoint-url "$S3_ENDPOINT_URL" s3 presign \
  "s3://$R2_BUCKET/$OBJECT_KEY" --expires-in 300)
echo "::add-mask::$presigned_url"
curl --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' \
  --output "$presigned_archive" "$presigned_url"

for archive in "$registry_archive" "$direct_archive" "$presigned_archive"; do
  [[ $(sha256sum "$archive" | awk '{print $1}') == "$ARTIFACT_SHA" ]]
  "$zed_binary" verify "$archive" \
    --target aarch64-linux-android --json >/dev/null
done
cmp "$registry_archive" "$direct_archive"
cmp "$registry_archive" "$presigned_archive"

if "$zed_binary" verify "$presigned_archive" \
    --target x86_64-unknown-linux-gnu >/dev/null 2>&1; then
  echo 'Android artifact unexpectedly verified as a Linux GNU x86_64 target' >&2
  exit 1
fi

extracted="$DOWNLOAD_DIR/extracted"
mkdir -p "$extracted"
python3 - "$presigned_archive" "$extracted" <<'PY'
import pathlib
import sys
import zipfile

archive = pathlib.Path(sys.argv[1])
out = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(archive) as bundle:
    member = bundle.getinfo("pkg/bin/r2-android-smoke")
    target = out / "r2-android-smoke"
    target.write_bytes(bundle.read(member))
PY
chmod 0755 "$extracted/r2-android-smoke"
file "$extracted/r2-android-smoke" | tee "$RUNNER_TEMP/r2-android-downloaded-file.txt"
grep -Eiq 'ELF 64-bit.*ARM aarch64' "$RUNNER_TEMP/r2-android-downloaded-file.txt"
readelf -h "$extracted/r2-android-smoke" >"$RUNNER_TEMP/r2-android-downloaded-elf-header.txt"
grep -Eq 'Machine:[[:space:]]+AArch64' "$RUNNER_TEMP/r2-android-downloaded-elf-header.txt"
readelf -l "$extracted/r2-android-smoke" >"$RUNNER_TEMP/r2-android-downloaded-elf-program.txt"
grep -Fq '/system/bin/linker64' "$RUNNER_TEMP/r2-android-downloaded-elf-program.txt"

corrupted="$DOWNLOAD_DIR/corrupted.zip"
cp "$direct_archive" "$corrupted"
python3 - "$corrupted" <<'PY'
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[max(32, len(data) // 2)] ^= 1
path.write_bytes(data)
PY
if "$zed_binary" verify "$corrupted" --target aarch64-linux-android \
    >/dev/null 2>&1; then
  echo 'corrupted R2 download unexpectedly verified' >&2
  exit 1
fi
rm -f "$corrupted"

pids=()
for index in $(seq 1 6); do
  (
    out="$DOWNLOAD_DIR/concurrent-$index.zip"
    "$zed_binary" download \
      "$TEST_ORG/r2-android-smoke@$TEST_VERSION" \
      --out "$out" --target aarch64-linux-android --json >/dev/null
    [[ $(sha256sum "$out" | awk '{print $1}') == "$ARTIFACT_SHA" ]]
  ) &
  pids+=("$!")
done
for pid in "${pids[@]}"; do wait "$pid"; done

before=$(aws --endpoint-url "$S3_ENDPOINT_URL" s3api head-object \
  --bucket "$R2_BUCKET" --key "$OBJECT_KEY")
output=$("$zed_binary" publish \
  --target aarch64-linux-android \
  --os android \
  --arch aarch64 \
  --abi api24 \
  --out "$PACK_DIR" 2>&1)
printf '%s\n' "$output"
printf '%s\n' "$output" \
  | grep -Eiq 'already published.*identical|identical binary ZIP|skipping'
after=$(aws --endpoint-url "$S3_ENDPOINT_URL" s3api head-object \
  --bucket "$R2_BUCKET" --key "$OBJECT_KEY")
[[ $(jq -r .ContentLength <<<"$before") == $(jq -r .ContentLength <<<"$after") ]]
[[ $(jq -r .ETag <<<"$before") == $(jq -r .ETag <<<"$after") ]]

jq -cn \
  --arg package "$TEST_ORG/r2-android-smoke@$TEST_VERSION" \
  --arg target aarch64-linux-android \
  --arg sha "$ARTIFACT_SHA" \
  --argjson size "$ARTIFACT_SIZE" \
  '{
    provider:"cloudflare-r2",
    package:$package,
    target:$target,
    sha256:$sha,
    size:$size,
    deterministic_pack:true,
    registry_download_verified:true,
    direct_s3_download_verified:true,
    presigned_mobile_download_verified:true,
    corrupted_download_rejected:true,
    wrong_target_rejected:true,
    downloaded_payload_format:"ELF64-AArch64-Android",
    physical_device_executed:false
  }' >"$RUNNER_TEMP/r2-android-certification.json"

aws --endpoint-url "$S3_ENDPOINT_URL" s3api delete-object \
  --bucket "$R2_BUCKET" --key "$OBJECT_KEY" >/dev/null
for _ in $(seq 1 15); do
  if ! aws --endpoint-url "$S3_ENDPOINT_URL" s3api head-object \
      --bucket "$R2_BUCKET" --key "$OBJECT_KEY" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if aws --endpoint-url "$S3_ENDPOINT_URL" s3api head-object \
    --bucket "$R2_BUCKET" --key "$OBJECT_KEY" >/dev/null 2>&1; then
  echo 'ephemeral R2 object still exists after deletion' >&2
  exit 1
fi
