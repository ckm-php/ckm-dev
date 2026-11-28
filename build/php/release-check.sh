#!/usr/bin/env bash
# The publish gate's run check (D25), used by php-publish.yml's `prepare`
# job before anything is created. It accepts a candidate build only when it
# is a clean, successful "PHP build" dispatch on main at the commit being
# published. A pure function of its inputs: it reads two files and the
# environment, and touches no network, so tests/release-check-selftest.sh
# (T6) can test every refusal.
#
# Usage: build/php/release-check.sh
#
# Environment (all required):
#   RUN_JSON                path of the candidate run's JSON, as the REST API
#                           returns GET /repos/<repo>/actions/runs/<id>
#   BUILD_INFO              path of the run's build-info.json artifact
#   RUN_ID                  the run id the publish dispatch names
#   COMMIT                  the commit the publish dispatch names
#   EXPECTED_WORKFLOW_PATH  .github/workflows/php-build.yml
#   GITHUB_SHA, GITHUB_REF, GITHUB_REPOSITORY  the publish run's own, as
#                           GitHub sets them
#
# Prints one line on success. Exits 1 with the reason on stderr otherwise.

RC_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$RC_DIR/lib/common.sh"

rc_refuse() {
  ck_die "release-check: refused: $*"
}

# rc_get <file> <what> <jq path> <string|number|boolean>: prints the value at
# the path, refusing a file that is not a JSON object, a missing value or one
# of another JSON type.
rc_get() {
  local file=$1 what=$2 path=$3 type=$4 value
  if ! value=$(jq -er --arg type "$type" \
    "if type == \"object\" then ($path) else error(\"not an object\") end
     | if type == \$type then tostring else error(\"wrong type\") end" \
    "$file" 2>/dev/null); then
    rc_refuse "$what is missing or not a JSON $type"
  fi
  printf '%s\n' "$value"
}

# rc_expect <what> <actual> <expected>
rc_expect() {
  [ "$2" = "$3" ] || rc_refuse "$1 is \"$2\", expected \"$3\""
}

rc_main() {
  local var value hex40='^[0-9a-f]{40}$' digits='^[1-9][0-9]{0,19}$'
  local repo_re='^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' keys
  ck_require_cmd jq

  for var in RUN_JSON BUILD_INFO RUN_ID COMMIT EXPECTED_WORKFLOW_PATH \
    GITHUB_SHA GITHUB_REF GITHUB_REPOSITORY; do
    [ -n "${!var:-}" ] || rc_refuse "$var is not set"
  done

  # The dispatch inputs and the publish run itself.
  [[ $RUN_ID =~ $digits ]] || rc_refuse "the run id is not a number"
  [[ $COMMIT =~ $hex40 ]] || rc_refuse "the commit is not 40 lower-case hex characters"
  [[ $GITHUB_SHA =~ $hex40 ]] || rc_refuse "GITHUB_SHA is not 40 lower-case hex characters"
  [ "$COMMIT" = "$GITHUB_SHA" ] ||
    rc_refuse "the commit $COMMIT is not this workflow's own commit $GITHUB_SHA"
  rc_expect "this workflow's ref" "$GITHUB_REF" refs/heads/main
  [[ $GITHUB_REPOSITORY =~ $repo_re ]] || rc_refuse "GITHUB_REPOSITORY is not owner/repo"
  case "$EXPECTED_WORKFLOW_PATH" in
    .github/workflows/*.yml) ;;
    *) rc_refuse "EXPECTED_WORKFLOW_PATH is not a workflow file" ;;
  esac
  [ -f "$RUN_JSON" ] || rc_refuse "RUN_JSON is not a file"
  [ -f "$BUILD_INFO" ] || rc_refuse "BUILD_INFO is not a file"

  # The candidate run, as GitHub reports it.
  value=$(rc_get "$RUN_JSON" "the run's id" .id number)
  rc_expect "the run's id" "$value" "$RUN_ID"
  value=$(rc_get "$RUN_JSON" "the run's workflow path" .path string)
  rc_expect "the run's workflow path" "$value" "$EXPECTED_WORKFLOW_PATH"
  value=$(rc_get "$RUN_JSON" "the run's repository" .repository.full_name string)
  rc_expect "the run's repository" "$value" "$GITHUB_REPOSITORY"
  value=$(rc_get "$RUN_JSON" "the run's head repository" .head_repository.full_name string)
  rc_expect "the run's head repository" "$value" "$GITHUB_REPOSITORY"
  value=$(rc_get "$RUN_JSON" "the run's event" .event string)
  rc_expect "the run's event" "$value" workflow_dispatch
  value=$(rc_get "$RUN_JSON" "the run's branch" .head_branch string)
  rc_expect "the run's branch" "$value" main
  value=$(rc_get "$RUN_JSON" "the run's commit" .head_sha string)
  rc_expect "the run's commit" "$value" "$COMMIT"
  value=$(rc_get "$RUN_JSON" "the run's status" .status string)
  rc_expect "the run's status" "$value" completed
  value=$(rc_get "$RUN_JSON" "the run's conclusion" .conclusion string)
  rc_expect "the run's conclusion" "$value" success

  # What the run wrote about itself. Only a clean run restored no cache.
  keys=$(jq -cr 'if type == "object" then keys | join(",") else "" end' "$BUILD_INFO" 2>/dev/null) ||
    rc_refuse "build-info is not JSON"
  rc_expect "build-info's keys" "$keys" \
    "clean,commit,event,lines,oses,publishable,ref,run_attempt,run_id,workflow"
  value=$(rc_get "$BUILD_INFO" "build-info's clean marker" .clean boolean)
  rc_expect "build-info's clean marker" "$value" true
  value=$(rc_get "$BUILD_INFO" "build-info's commit" .commit string)
  rc_expect "build-info's commit" "$value" "$COMMIT"
  value=$(rc_get "$BUILD_INFO" "build-info's run id" .run_id number)
  rc_expect "build-info's run id" "$value" "$RUN_ID"
  value=$(rc_get "$BUILD_INFO" "build-info's workflow" .workflow string)
  rc_expect "build-info's workflow" "$value" "$EXPECTED_WORKFLOW_PATH"
  value=$(rc_get "$BUILD_INFO" "build-info's event" .event string)
  rc_expect "build-info's event" "$value" workflow_dispatch
  value=$(rc_get "$BUILD_INFO" "build-info's ref" .ref string)
  rc_expect "build-info's ref" "$value" refs/heads/main
  value=$(rc_get "$BUILD_INFO" "build-info's run attempt" .run_attempt number)
  [[ $value =~ $digits ]] || rc_refuse "build-info's run attempt is not a positive number"

  # The exact scope is replayed through plan-matrix.sh by the publish
  # workflow. Check its shape here so that nothing untrusted reaches jq's
  # join or a shell command in an unexpected form.
  if ! jq -e '
    type == "object"
    and (.lines | type == "array" and length > 0
         and all(.[]; type == "string" and test("^[0-9]+\\.[0-9]+$")))
    and (.oses | type == "array" and length > 0
         and all(.[]; type == "string" and test("^[a-z]+$")))
  ' "$BUILD_INFO" >/dev/null 2>&1; then
    rc_refuse "build-info's scope is missing or invalid"
  fi
  value=$(rc_get "$BUILD_INFO" "build-info's publishable marker" .publishable boolean)
  rc_expect "build-info's publishable marker" "$value" true

  printf 'release-check: run %s at %s is a clean, successful %s dispatch on main\n' \
    "$RUN_ID" "$COMMIT" "$EXPECTED_WORKFLOW_PATH"
}

rc_main "$@"
