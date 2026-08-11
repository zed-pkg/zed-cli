#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_TEMP:?}"
: "${GITHUB_RUN_ID:?}"
: "${GITHUB_RUN_ATTEMPT:?}"
: "${GITHUB_REPOSITORY:?}"
: "${GITHUB_WORKFLOW:?}"
: "${GITHUB_JOB:?}"
: "${PR_HEAD_SHA:?reviewed pull-request head SHA is required}"
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
    --arg attempt "$GITHUB_RUN_ATTEMPT" \
    --arg sha "$PR_HEAD_SHA" \
    --arg repository "$GITHUB_REPOSITORY" \
    --arg workflow "$GITHUB_WORKFLOW" \
    --arg job "$GITHUB_JOB" \
    --arg account "$SECRET_ACCOUNT_ID" \
    --arg api "$SECRET_API_TOKEN" \
    --arg access "$SECRET_PARENT_ACCESS_KEY_ID" \
    '{run:$run,attempt:$attempt,sha:$sha,repository:$repository,workflow:$workflow,job:$job,account:$account,api:$api,access:$access}' \
    >"$delegation_file"
  chmod 0600 "$delegation_file"
  echo 'R2_CREDENTIAL_SOURCE=actions-environment' >>"$GITHUB_ENV"
  exit 0
fi

: "${GH_TOKEN:?GitHub token is required for encrypted handoff}"
: "${PR_NUMBER:?pull-request number is required for encrypted handoff}"
: "${PR_HEAD_REPOSITORY:?}"
[[ "$GITHUB_EVENT_NAME" == pull_request ]]
[[ "$PR_HEAD_REPOSITORY" == "$GITHUB_REPOSITORY" ]]
[[ "$PR_HEAD_SHA" =~ ^[0-9a-f]{40}$ ]]

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
  -out "$private_key" >/dev/null 2>&1
chmod 0600 "$private_key"
openssl pkey -in "$private_key" -pubout -outform DER -out "$public_der"
public_key=$(base64 -w0 <"$public_der")

# The annotation carries only the ephemeral public key. The runner retains the
# private key locally and accepts ciphertext from the exact repository owner.
echo "::notice title=Encrypted R2 handoff requested::run-id=${GITHUB_RUN_ID}; run-attempt=${GITHUB_RUN_ATTEMPT}; head-sha=${PR_HEAD_SHA}; public-key-base64=${public_key}"
echo "Encrypted R2 handoff requested for run $GITHUB_RUN_ID attempt $GITHUB_RUN_ATTEMPT; the public key is available in this check's annotations."

marker="<!-- zed-r2-handoff:${GITHUB_RUN_ID}:${GITHUB_RUN_ATTEMPT}:${PR_HEAD_SHA} -->"
ciphertext=''
for _ in $(seq 1 180); do
  comments=$(curl --fail --silent --show-error --location \
    --proto '=https' --proto-redir '=https' \
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

jq -e \
  --arg run "$GITHUB_RUN_ID" \
  --arg attempt "$GITHUB_RUN_ATTEMPT" \
  --arg sha "$PR_HEAD_SHA" \
  --arg repository "$GITHUB_REPOSITORY" \
  --arg workflow "$GITHUB_WORKFLOW" \
  --arg job "$GITHUB_JOB" '
  .run == $run
  and .attempt == $attempt
  and .sha == $sha
  and .repository == $repository
  and .workflow == $workflow
  and .job == $job
  and (.account | test("^[0-9a-f]{32}$"))
  and (.api | type == "string" and length >= 20)
  and (.access | type == "string" and length >= 16)
  and (keys | sort == ["access", "account", "api", "attempt", "job", "repository", "run", "sha", "workflow"])
' "$plain_file" >/dev/null
mv "$plain_file" "$delegation_file"
chmod 0600 "$delegation_file"
for value in account api access; do
  secret=$(jq -r ".$value" "$delegation_file")
  echo "::add-mask::$secret"
done
echo 'R2_CREDENTIAL_SOURCE=encrypted-pr-handoff' >>"$GITHUB_ENV"
