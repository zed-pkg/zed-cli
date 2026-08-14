#!/usr/bin/env bash
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo "r2_load_protected_credentials.sh takes no arguments" >&2
  exit 2
}

: "$RUNNER_TEMP"
: "$GITHUB_RUN_ID"
: "$GITHUB_RUN_ATTEMPT"
: "$GITHUB_REPOSITORY"
: "$GITHUB_REF"
: "$GITHUB_WORKFLOW"
: "$GITHUB_JOB"
: "$GITHUB_SHA"
: "$GITHUB_ENV"
: "$SECRET_ACCOUNT_ID"
: "$SECRET_API_TOKEN"
: "$SECRET_PARENT_ACCESS_KEY_ID"

[[ "$GITHUB_EVENT_NAME" == workflow_dispatch ]]
[[ "$GITHUB_REPOSITORY" == zed-pkg/zed-cli ]]
[[ "$GITHUB_REF" == refs/heads/main ]]
[[ "$GITHUB_SHA" =~ ^[0-9a-f]{40}$ ]]
[[ "$SECRET_ACCOUNT_ID" =~ ^[0-9a-f]{32}$ ]]
[[ $(printf %s "$SECRET_API_TOKEN" | wc -c) -ge 20 ]]
[[ $(printf %s "$SECRET_PARENT_ACCESS_KEY_ID" | wc -c) -ge 16 ]]

delegation_file="$RUNNER_TEMP/r2-parent-delegation.json"
[[ ! -e "$delegation_file" ]]
trap 'rm -f "$delegation_file"' ERR INT TERM

echo "::add-mask::$SECRET_ACCOUNT_ID"
echo "::add-mask::$SECRET_API_TOKEN"
echo "::add-mask::$SECRET_PARENT_ACCESS_KEY_ID"

jq -cn \
  --arg run "$GITHUB_RUN_ID" \
  --arg attempt "$GITHUB_RUN_ATTEMPT" \
  --arg sha "$GITHUB_SHA" \
  --arg repository "$GITHUB_REPOSITORY" \
  --arg ref "$GITHUB_REF" \
  --arg workflow "$GITHUB_WORKFLOW" \
  --arg job "$GITHUB_JOB" \
  --arg account "$SECRET_ACCOUNT_ID" \
  --arg api "$SECRET_API_TOKEN" \
  --arg access "$SECRET_PARENT_ACCESS_KEY_ID" \
  '{
    run:$run,
    attempt:$attempt,
    sha:$sha,
    repository:$repository,
    ref:$ref,
    workflow:$workflow,
    job:$job,
    account:$account,
    api:$api,
    access:$access
  }' >"$delegation_file"
chmod 0600 "$delegation_file"

jq -e \
  --arg run "$GITHUB_RUN_ID" \
  --arg attempt "$GITHUB_RUN_ATTEMPT" \
  --arg sha "$GITHUB_SHA" \
  --arg repository "$GITHUB_REPOSITORY" \
  --arg ref "$GITHUB_REF" \
  --arg workflow "$GITHUB_WORKFLOW" \
  --arg job "$GITHUB_JOB" '
    .run == $run
    and .attempt == $attempt
    and .sha == $sha
    and .repository == $repository
    and .ref == $ref
    and .workflow == $workflow
    and .job == $job
    and (.account | test("^[0-9a-f]{32}$"))
    and (.api | type == "string" and length >= 20)
    and (.access | type == "string" and length >= 16)
    and (keys | sort
      == ["access", "account", "api", "attempt", "job", "ref", "repository", "run", "sha", "workflow"])
  ' "$delegation_file" >/dev/null

echo 'R2_CREDENTIAL_SOURCE=protected-actions-environment' >>"$GITHUB_ENV"
trap - ERR INT TERM
