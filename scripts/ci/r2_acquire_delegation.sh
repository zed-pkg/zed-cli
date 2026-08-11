#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_TEMP:?}"
: "${GITHUB_RUN_ID:?}"
: "${GITHUB_ENV:?}"

private_key="$RUNNER_TEMP/r2-handoff-private.pem"
public_der="$RUNNER_TEMP/r2-handoff-public.der"
ciphertext_file="$RUNNER_TEMP/r2-handoff.bin"
delegation_file="$RUNNER_TEMP/r2-parent-delegation.json"
plain_file="$RUNNER_TEMP/r2-handoff-plain.json"
trap 'rm -f "$private_key" "$public_der" "$ciphertext_file" "$plain_file"' EXIT

if [[ -n "${SECRET_ACCOUNT_ID:-}" \
   && -n "${SECRET_API_TOKEN:-}" \
   && -n "${SECRET_PARENT_ACCESS_KEY_ID:-}" ]]; then
  echo "::add-mask::$SECRET_ACCOUNT_ID"
  echo "::add-mask::$SECRET_API_TOKEN"
  echo "::add-mask::$SECRET_PARENT_ACCESS_KEY_ID"
  jq -cn \
    --arg run "$GITHUB_RUN_ID" \
    --arg account "$SECRET_ACCOUNT_ID" \
    --arg api "$SECRET_API_TOKEN" \
    --arg access "$SECRET_PARENT_ACCESS_KEY_ID" \
    '{run:$run,account:$account,api:$api,access:$access}' \
    >"$delegation_file"
  chmod 0600 "$delegation_file"
  echo 'R2_CREDENTIAL_SOURCE=actions-environment' >>"$GITHUB_ENV"
  exit 0
fi

: "${GH_TOKEN:?GitHub token is required for encrypted handoff}"
: "${PR_NUMBER:?pull-request number is required for encrypted handoff}"
: "${PR_HEAD_REPOSITORY:?}"
: "${GITHUB_REPOSITORY:?}"
[[ "$GITHUB_EVENT_NAME" == pull_request ]]
[[ "$PR_HEAD_REPOSITORY" == "$GITHUB_REPOSITORY" ]]

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
  -out "$private_key" >/dev/null 2>&1
chmod 0600 "$private_key"
openssl pkey -in "$private_key" -pubout -outform DER -out "$public_der"
public_key=$(base64 -w0 <"$public_der")

request_body=$(printf '%s\n%s\n%s\n' \
  "<!-- zed-r2-request:${GITHUB_RUN_ID} -->" \
  "run-id: ${GITHUB_RUN_ID}" \
  "public-key-base64: ${public_key}")
curl --fail --silent --show-error --location \
  --request POST \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H 'Accept: application/vnd.github+json' \
  -H 'X-GitHub-Api-Version: 2022-11-28' \
  --data "$(jq -cn --arg body "$request_body" '{body:$body}')" \
  "https://api.github.com/repos/$GITHUB_REPOSITORY/issues/$PR_NUMBER/comments" \
  >"$RUNNER_TEMP/r2-request-comment.json"
jq -e '.id | type == "number"' "$RUNNER_TEMP/r2-request-comment.json" >/dev/null
rm -f "$RUNNER_TEMP/r2-request-comment.json"
echo "Encrypted R2 handoff requested for run $GITHUB_RUN_ID."

marker="<!-- zed-r2-handoff:${GITHUB_RUN_ID} -->"
ciphertext=''
for _ in $(seq 1 180); do
  comments=$(curl --fail --silent --show-error --location \
    -H "Authorization: Bearer $GH_TOKEN" \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' \
    "https://api.github.com/repos/$GITHUB_REPOSITORY/issues/$PR_NUMBER/comments?per_page=100")
  ciphertext=$(jq -r --arg marker "$marker" '
    [ .[]
      | select(.user.login == "ORESoftware")
      | select(.author_association == "OWNER"
          or .author_association == "MEMBER"
          or .author_association == "COLLABORATOR")
      | select(.body | startswith($marker))
      | {
          created_at,
          cipher: (try (.body
            | capture("ciphertext-base64: (?<cipher>[A-Za-z0-9+/=]+)")
            | .cipher) catch "")
        }
      | select(.cipher != "")
    ]
    | sort_by(.created_at)
    | last
    | .cipher // empty
  ' <<<"$comments")
  [[ -n "$ciphertext" ]] && break
  sleep 5
done
[[ -n "$ciphertext" ]]
printf '%s' "$ciphertext" | base64 --decode >"$ciphertext_file"
[[ $(stat -c %s "$ciphertext_file") -eq 384 ]]

openssl pkeyutl -decrypt \
  -inkey "$private_key" \
  -in "$ciphertext_file" \
  -pkeyopt rsa_padding_mode:oaep \
  -pkeyopt rsa_oaep_md:sha256 \
  -pkeyopt rsa_mgf1_md:sha256 \
  >"$plain_file"

jq -e --arg run "$GITHUB_RUN_ID" '
  .run == $run
  and (.account | test("^[0-9a-f]{32}$"))
  and (.api | type == "string" and length >= 20)
  and (.access | type == "string" and length >= 16)
  and (keys | sort == ["access", "account", "api", "run"])
' "$plain_file" >/dev/null
mv "$plain_file" "$delegation_file"
chmod 0600 "$delegation_file"
for value in account api access; do
  secret=$(jq -r ".$value" "$delegation_file")
  echo "::add-mask::$secret"
done
echo 'R2_CREDENTIAL_SOURCE=encrypted-pr-handoff' >>"$GITHUB_ENV"
