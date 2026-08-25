#!/usr/bin/env bash
set -euo pipefail

# Qualifies the public CLI binary against a running, loopback-only Studio stack.
# The stack must enable Studio's explicit insecure development identity and a
# mock Control publication endpoint. No OAuth token or production endpoint is
# accepted by this fixture.

api_url="${REPORCH_STUDIO_API_URL:-http://127.0.0.1:58080}"
cli="${REPORCH_E2E_CLI:-target/debug/reporch}"
timeout_seconds="${REPORCH_E2E_TIMEOUT_SECONDS:-300}"

case "${api_url}" in
  http://127.0.0.1:* | http://localhost:* | http://\[::1\]:*) ;;
  *)
    printf 'live Studio CLI qualification requires a loopback HTTP API, got %s\n' \
      "${api_url}" >&2
    exit 2
    ;;
esac

command -v jq >/dev/null
[[ -x "${cli}" ]]

new_id() {
  if command -v uuidgen >/dev/null; then
    uuidgen | tr '[:upper:]' '[:lower:]'
  else
    node -e 'console.log(crypto.randomUUID())'
  fi
}

run_id="$(new_id)"
author="cli-e2e-author-${run_id}"
reviewer="cli-e2e-reviewer-${run_id}"
run_dir="$(mktemp -d /tmp/reporch-cli-live.XXXXXX)"
project_dir="${run_dir}/sum"

cleanup() {
  status="$?"
  if [[ "${status}" -ne 0 || "${REPORCH_E2E_KEEP:-0}" == "1" ]]; then
    printf 'qualification checkout retained at %s\n' "${run_dir}" >&2
  else
    rm -r "${run_dir}"
  fi
  exit "${status}"
}
trap cleanup EXIT

export REPORCH_STUDIO_API_URL="${api_url}"
export REPORCH_STUDIO_ALLOW_INSECURE_HTTP=true
export REPORCH_STUDIO_DEV_SUBJECT="${author}"

create_json="$(${cli} --format json --no-input project create \
  --title 'Public CLI live qualification' \
  --problem-type standard \
  --directory "${project_dir}" \
  --idempotency-key "live-create-${run_id}")"
project_id="$(jq -er '.data.id' <<<"${create_json}")"

check_json="$(${cli} --format json --no-input --cwd "${project_dir}" check)"
jq -e '
  .data.valid == true
  and .data.authoring_schema == "reporch.authoring-spec.v2"
  and .data.release_schema == "reporch.release-manifest.v2"
' <<<"${check_json}" >/dev/null

submit_json="$(${cli} --format json --no-input --cwd "${project_dir}" submit \
  --message 'Public CLI live qualification' \
  --timeout-seconds "${timeout_seconds}")"
review_id="$(jq -er '.data.review.id' <<<"${submit_json}")"
validation_id="$(jq -er '.data.validation.detail.id' <<<"${submit_json}")"
jq -e '.data.validation.detail.status == "passed"' <<<"${submit_json}" >/dev/null
[[ "$(jq -er '.last_validation_run_id' "${project_dir}/.reporch/state.json")" == \
  "${validation_id}" ]]

review_json="$(${cli} --format json --no-input --cwd "${project_dir}" review show \
  --review-id "${review_id}")"
jq -e --arg review_id "${review_id}" --arg project_id "${project_id}" '
  .data.id == $review_id
  and .data.project_id == $project_id
  and .data.status == "in_review"
' <<<"${review_json}" >/dev/null

pool_json="$(${cli} --format json --no-input --cwd "${project_dir}" review request \
  --review-id "${review_id}" \
  --pool \
  --idempotency-key "live-pool-${run_id}")"
pool_id="$(jq -er '.data.id' <<<"${pool_json}")"

set +e
author_claim="$(${cli} --format json --no-input review claim \
  --pool-request-id "${pool_id}" 2>&1)"
author_claim_status="$?"
set -e
[[ "${author_claim_status}" -eq 5 ]]
jq -e '.error_code == "review.separation_required"' <<<"${author_claim}" >/dev/null

export REPORCH_STUDIO_DEV_SUBJECT="${reviewer}"
${cli} --format json --no-input review inbox >/dev/null
claim_json="$(${cli} --format json --no-input review claim \
  --pool-request-id "${pool_id}")"
jq -e '.data.status == "claimed"' <<<"${claim_json}" >/dev/null
approve_json="$(${cli} --format json --no-input review approve \
  --pool-request-id "${pool_id}" \
  --comment 'Independent public CLI qualification passed' \
  --idempotency-key "live-approve-${run_id}")"
jq -e '
  .data.status == "approved"
  and .data.decision.approval_source == "review_pool"
' <<<"${approve_json}" >/dev/null

export REPORCH_STUDIO_DEV_SUBJECT="${author}"
release_json="$(${cli} --format json --no-input --cwd "${project_dir}" release build \
  --timeout-seconds "${timeout_seconds}" \
  --idempotency-key "live-release-${run_id}")"
release_id="$(jq -er '.data.id' <<<"${release_json}")"
jq -e '.data.status == "ready"' <<<"${release_json}" >/dev/null
[[ "$(jq -er '.last_release_id' "${project_dir}/.reporch/state.json")" == "${release_id}" ]]

publish_json="$(${cli} --format json --no-input --yes --cwd "${project_dir}" \
  publication publish \
  --timeout-seconds "${timeout_seconds}" \
  --idempotency-key "live-publish-${run_id}")"
jq -e '.data.status == "published"' <<<"${publish_json}" >/dev/null

jq -n \
  --arg project_id "${project_id}" \
  --arg validation_id "${validation_id}" \
  --arg pool_request_id "${pool_id}" \
  --arg release_id "${release_id}" \
  --arg package_digest "$(jq -er '.data.package_digest' <<<"${release_json}")" \
  '{
    status: "passed",
    manual_ids_supplied: false,
    author_claim_exit_code: 5,
    project_id: $project_id,
    validation_id: $validation_id,
    pool_request_id: $pool_request_id,
    release_id: $release_id,
    package_digest: $package_digest
  }'
