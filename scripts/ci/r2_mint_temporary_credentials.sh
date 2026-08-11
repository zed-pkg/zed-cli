#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_TEMP:?}"
: "${GITHUB_ENV:?}"
: "${R2_BUCKET:?}"
: "${OBJECT_KEY:?}"
: "${R2_CREDENTIAL_SOURCE:?}"

delegation_file="$RUNNER_TEMP/r2-parent-delegation.json"
[[ -s "$delegation_file" ]]
account=$(jq -r .account "$delegation_file")
api_token=$(jq -r .api "$delegation_file")
parent_access=$(jq -r .access "$delegation_file")
for secret in "$account" "$api_token" "$parent_access"; do
  echo "::add-mask::$secret"
done
[[ "$account" =~ ^[0-9a-f]{32}$ ]]
endpoint="https://${account}.r2.cloudflarestorage.com"

verify_file="$RUNNER_TEMP/r2-token-verify.json"
verify_status=$(curl --silent --show-error --location \
  --output "$verify_file" --write-out '%{http_code}' \
  -H "Authorization: Bearer $api_token" \
  "https://api.cloudflare.com/client/v4/accounts/$account/tokens/verify")
if [[ ! "$verify_status" =~ ^2 ]] || ! jq -e '.success == true' "$verify_file" >/dev/null; then
  jq -r '.errors[]?.message // empty' "$verify_file" >&2 || true
  echo "Cloudflare API token verification failed with HTTP $verify_status" >&2
  exit 1
fi

list_file="$RUNNER_TEMP/r2-bucket-list.json"
list_status=$(curl --silent --show-error --location \
  --output "$list_file" --write-out '%{http_code}' \
  -H "Authorization: Bearer $api_token" \
  -H 'Content-Type: application/json' \
  "https://api.cloudflare.com/client/v4/accounts/$account/r2/buckets?per_page=1000")
if [[ ! "$list_status" =~ ^2 ]] || ! jq -e '.success == true' "$list_file" >/dev/null; then
  jq -r '.errors[]?.message // empty' "$list_file" >&2 || true
  echo "Cloudflare R2 bucket listing failed with HTTP $list_status" >&2
  exit 1
fi

bucket_created=false
if ! jq -e --arg bucket "$R2_BUCKET" '
    any((.result.buckets // [])[]?; .name == $bucket)
  ' "$list_file" >/dev/null; then
  create_file="$RUNNER_TEMP/r2-bucket-create.json"
  create_status=$(curl --silent --show-error --location \
    --output "$create_file" --write-out '%{http_code}' \
    --request POST \
    -H "Authorization: Bearer $api_token" \
    -H 'Content-Type: application/json' \
    --data "$(jq -cn --arg name "$R2_BUCKET" '{name:$name}')" \
    "https://api.cloudflare.com/client/v4/accounts/$account/r2/buckets")
  if [[ ! "$create_status" =~ ^2 ]] || ! jq -e '.success == true' "$create_file" >/dev/null; then
    jq -r '.errors[]?.message // empty' "$create_file" >&2 || true
    echo "Cloudflare R2 bucket creation failed with HTTP $create_status" >&2
    exit 1
  fi
  bucket_created=true
fi

temp_request="$RUNNER_TEMP/r2-temp-request.json"
temp_response="$RUNNER_TEMP/r2-temp-response.json"
jq -cn \
  --arg bucket "$R2_BUCKET" \
  --arg parent "$parent_access" \
  --arg object "$OBJECT_KEY" \
  '{
    bucket:$bucket,
    parentAccessKeyId:$parent,
    permission:"object-read-write",
    ttlSeconds:3600,
    objects:[$object]
  }' >"$temp_request"
temp_status=$(curl --silent --show-error --location \
  --output "$temp_response" --write-out '%{http_code}' \
  --request POST \
  -H "Authorization: Bearer $api_token" \
  -H 'Content-Type: application/json' \
  --data @"$temp_request" \
  "https://api.cloudflare.com/client/v4/accounts/$account/r2/temp-access-credentials")
if [[ ! "$temp_status" =~ ^2 ]] || ! jq -e '
    .success == true
    and (.result.accessKeyId | type == "string" and length > 0)
    and (.result.secretAccessKey | type == "string" and length > 0)
    and (.result.sessionToken | type == "string" and length > 0)
  ' "$temp_response" >/dev/null; then
  jq -r '.errors[]?.message // empty' "$temp_response" >&2 || true
  echo "Cloudflare R2 temporary credential mint failed with HTTP $temp_status" >&2
  exit 1
fi

temp_access=$(jq -r .result.accessKeyId "$temp_response")
temp_secret=$(jq -r .result.secretAccessKey "$temp_response")
temp_session=$(jq -r .result.sessionToken "$temp_response")
for secret in "$temp_access" "$temp_secret" "$temp_session"; do
  echo "::add-mask::$secret"
done
printf 'AWS_ACCESS_KEY_ID=%s\nAWS_SECRET_ACCESS_KEY=%s\nAWS_SESSION_TOKEN=%s\nS3_ENDPOINT_URL=%s\n' \
  "$temp_access" "$temp_secret" "$temp_session" "$endpoint" >>"$GITHUB_ENV"

jq -cn \
  --arg bucket "$R2_BUCKET" \
  --arg object "$OBJECT_KEY" \
  --argjson created "$bucket_created" \
  --arg source "$R2_CREDENTIAL_SOURCE" \
  '{
    provider:"cloudflare-r2",
    bucket:$bucket,
    object:$object,
    bucket_created:$created,
    credential_source:$source,
    delegated_permission:"object-read-write",
    delegated_ttl_seconds:3600,
    delegated_scope:"exact-object",
    parent_secret_access_key_exposed:false
  }' >"$RUNNER_TEMP/r2-delegation.json"

rm -f "$delegation_file" "$temp_request" "$temp_response" "$list_file" \
  "$verify_file" "$RUNNER_TEMP/r2-bucket-create.json"
