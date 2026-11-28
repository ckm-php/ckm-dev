#!/usr/bin/env bash
# T6: the publish gate's checks, with no network.
#   - release-check.sh accepts only a clean, successful "PHP build" dispatch
#     on main at the publish run's own commit (fixtures/release-check/).
#   - release-set.sh lays out and checks the release set that gets published:
#     both platforms per version, nothing extra, and exact SHA256SUMS.
#
# Usage: build/php/tests/release-check-selftest.sh

PHP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
# shellcheck source=../lib/common.sh
. "$PHP_DIR/lib/common.sh"
ck_require_cmd jq

FIXTURES="$PHP_DIR/tests/fixtures/release-check"
T_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/ck-release-check-selftest.XXXXXX")
T_ROOT=$(cd "$T_ROOT" && pwd -P)
trap 'rm -rf -- "$T_ROOT"' EXIT

T_PASSED=0
T_SHA=0123456789abcdef0123456789abcdef01234567
T_RUN=44490557365

t_fail() {
  printf 'release-check-selftest: FAIL: %s\n' "$*" >&2
  if [ -s "$T_ROOT/stderr" ]; then
    printf '  output:\n%s\n' "$(cat "$T_ROOT/stderr")" >&2
  fi
  exit 1
}

t_ok() {
  T_PASSED=$((T_PASSED + 1))
  printf 'ok - %s\n' "$*"
}

# t_check <what> <run jq filter> <build-info jq filter> <expected reason, or
# empty for success> [VAR=value...]: runs release-check.sh on the fixtures
# changed by the two filters, with the good environment changed by the
# assignments.
t_check() {
  local what=$1 run_filter=$2 info_filter=$3 reason=$4 status=0
  shift 4
  jq "$run_filter" "$FIXTURES/run.json" >"$T_ROOT/run.json" || t_fail "$what: bad run filter"
  jq "$info_filter" "$FIXTURES/build-info.json" >"$T_ROOT/build-info.json" ||
    t_fail "$what: bad build-info filter"
  env -i PATH="$PATH" \
    RUN_JSON="$T_ROOT/run.json" BUILD_INFO="$T_ROOT/build-info.json" \
    RUN_ID="$T_RUN" COMMIT="$T_SHA" EXPECTED_WORKFLOW_PATH=.github/workflows/php-build.yml \
    GITHUB_SHA="$T_SHA" GITHUB_REF=refs/heads/main GITHUB_REPOSITORY=ckm-php/ckm-dev \
    "$@" bash "$PHP_DIR/release-check.sh" >"$T_ROOT/stdout" 2>"$T_ROOT/stderr" || status=$?
  if [ -z "$reason" ]; then
    [ "$status" -eq 0 ] || t_fail "$what: refused (exit $status)"
    grep -q 'is a clean, successful' "$T_ROOT/stdout" || t_fail "$what: no success line"
  else
    [ "$status" -eq 1 ] || t_fail "$what: exit $status, expected 1"
    grep -qF -- "$reason" "$T_ROOT/stderr" || t_fail "$what: the reason does not name \"$reason\""
  fi
  t_ok "$what"
}

# release-check.sh
t_check "a clean, successful dispatch on main is accepted" . . ""
t_check "a push event is refused" '.event = "push"' . "the run's event is \"push\""
t_check "another branch is refused" '.head_branch = "feature"' . "the run's branch is \"feature\""
t_check "another head commit is refused" \
  '.head_sha = "fedcba9876543210fedcba9876543210fedcba98"' . "the run's commit is"
t_check "a failed run is refused" '.conclusion = "failure"' . "the run's conclusion is \"failure\""
t_check "a run in progress is refused" '.status = "in_progress" | .conclusion = null' . \
  "the run's status is \"in_progress\""
t_check "another workflow is refused" '.path = ".github/workflows/ci.yml"' . \
  "the run's workflow path is \".github/workflows/ci.yml\""
t_check "a run from a fork is refused" '.head_repository.full_name = "someone/ckm-dev"' . \
  "the run's head repository"
t_check "another run id is refused" '.id = 1' . "the run's id is \"1\""
t_check "a run JSON that is not an object is refused" '[.]' . "the run's id is missing"
t_check "clean=false is refused" . '.clean = false' "build-info's clean marker is \"false\""
t_check "clean as a string is refused" . '.clean = "true"' "build-info's clean marker is missing or not a JSON boolean"
t_check "a missing clean marker is refused" . 'del(.clean)' "build-info's keys"
t_check "a missing line scope is refused" . 'del(.lines)' "build-info's keys"
t_check "a malformed line scope is refused" . '.lines = ["8.x"]' \
  "build-info's scope is missing or invalid"
t_check "a malformed OS scope is refused" . '.oses = ["linux2"]' \
  "build-info's scope is missing or invalid"
t_check "publishable=false is refused" . '.publishable = false' \
  "build-info's publishable marker is \"false\""
t_check "a missing publishable marker is refused" . 'del(.publishable)' "build-info's keys"
t_check "build-info for another commit is refused" \
  . '.commit = "fedcba9876543210fedcba9876543210fedcba98"' "build-info's commit is"
t_check "build-info for another run is refused" . '.run_id = 2' "build-info's run id is \"2\""
t_check "a commit other than GITHUB_SHA is refused" . . "is not this workflow's own commit" \
  COMMIT=fedcba9876543210fedcba9876543210fedcba98
t_check "a malformed commit is refused" . . "the commit is not 40 lower-case hex characters" \
  COMMIT=0123456789ABCDEF0123456789ABCDEF01234567
t_check "a short commit is refused" . . "the commit is not 40 lower-case hex characters" COMMIT=0123456
t_check "a malformed run id is refused" . . "the run id is not a number" 'RUN_ID=1;x'
t_check "a publish run off main is refused" . . "this workflow's ref is \"refs/heads/other\"" \
  GITHUB_REF=refs/heads/other

# release-set.sh, on synthetic archives (their contents are never read).

# t_artifact <dir> <os> <version>: writes one php-<os>-<line> artifact's
# archive and descriptor into <dir>, as the payload job's merged download
# holds them.
t_artifact() {
  local dir=$1 os=$2 version=$3 cpu stem sha size
  case "$os" in
    linux) cpu=x86_64 ;;
    macos) cpu=aarch64 ;;
  esac
  stem="php-$version-$os-$cpu"
  mkdir -p "$dir"
  printf 'php %s for %s\n' "$version" "$os" >"$dir/$stem.tar.gz"
  sha=$(sha256sum "$dir/$stem.tar.gz" 2>/dev/null || shasum -a 256 "$dir/$stem.tar.gz")
  sha=${sha%% *}
  size=$(wc -c <"$dir/$stem.tar.gz")
  jq -n --arg version "$version" --arg os "$os" --arg cpu "$cpu" --arg file "$stem.tar.gz" \
    --arg sha "$sha" --argjson size "$((size + 0))" \
    '{schema: 1, name: "php", version: $version, license: "PHP-3.01", os: $os, cpu: $cpu,
      file: $file, sha256: $sha, size: $size, format: "tar.gz"}' \
    >"$dir/$stem.json"
}

# t_release_artifacts <dir>: every publishable line on both platforms.
t_release_artifacts() {
  local version
  for version in 8.2.36-ck1 8.3.37-ck1 8.4.28-ck1 8.5.13-ck1; do
    t_artifact "$1" linux "$version"
    t_artifact "$1" macos "$version"
  done
}

# t_artifacts <dir>: every line, including CI-only 7.4, for partial test builds.
t_artifacts() {
  t_artifact "$1" linux 7.4.33-ck1
  t_artifact "$1" macos 7.4.33-ck1
  t_release_artifacts "$1"
}

# t_set <what> <expected reason, or empty> <release-set.sh args...>
t_set() {
  local what=$1 reason=$2 status=0
  shift 2
  bash "$PHP_DIR/release-set.sh" "$@" >"$T_ROOT/stdout" 2>"$T_ROOT/stderr" || status=$?
  if [ -z "$reason" ]; then
    [ "$status" -eq 0 ] || t_fail "$what: refused (exit $status)"
  else
    [ "$status" -eq 1 ] || t_fail "$what: exit $status, expected 1"
    grep -qF -- "$reason" "$T_ROOT/stderr" || t_fail "$what: the reason does not name \"$reason\""
  fi
  t_ok "$what"
}

t_release_artifacts "$T_ROOT/a-good"
t_set "a full set of artifacts is laid out" "" "$T_ROOT/a-good" "$T_ROOT/set"
t_sums=$(cat "$T_ROOT/set/php-8.5.13-ck1/SHA256SUMS")
t_expected=$(
  cd "$T_ROOT/set/php-8.5.13-ck1" &&
    { sha256sum php-8.5.13-ck1-linux-x86_64.tar.gz php-8.5.13-ck1-macos-aarch64.tar.gz 2>/dev/null ||
      shasum -a 256 php-8.5.13-ck1-linux-x86_64.tar.gz php-8.5.13-ck1-macos-aarch64.tar.gz; }
)
[ "$t_sums" = "$t_expected" ] || t_fail "SHA256SUMS is not what sha256sum writes"
[ "$(find "$T_ROOT/set" -type f | wc -l)" -eq 20 ] || t_fail "the set does not hold 20 files"
t_ok "SHA256SUMS is sha256sum's text form, in name order"
t_set "the laid-out set passes --check" "" --check "$T_ROOT/set"

# A clean dispatch records its exact scope, so the first catalog can hold
# 8.5 alone while later ones can hold more.
t_artifact "$T_ROOT/a-scope" linux 8.5.13-ck1
t_artifact "$T_ROOT/a-scope" macos 8.5.13-ck1
t_set "a recorded 8.5-only publish scope is laid out" "" --scope '8.5' 'linux macos' \
  "$T_ROOT/a-scope" "$T_ROOT/set-scope"
t_set "the scoped set passes --check" "" --scope '8.5' 'linux macos' --check "$T_ROOT/set-scope"
[ "$(find "$T_ROOT/set-scope" -type f | wc -l)" -eq 5 ] ||
  t_fail "the scoped set does not hold five files"
t_ok "the scoped set holds only 8.5's two platforms, descriptors and SHA256SUMS"
t_set "a scope refuses an extra publishable line" "not an expected release in the clean 8.5 on linux macos plan" \
  --scope '8.5' 'linux macos' --check "$T_ROOT/set"
t_set "a scope requires every line it names" "has no php-8.2.36-ck1 release folder" \
  --scope '8.2 8.5' 'linux macos' --check "$T_ROOT/set-scope"
t_set "a clean scope refuses a CI-only line" "clean build cannot include a CI-only PHP line" \
  --scope '7.4' 'linux macos' --check "$T_ROOT/set-scope"

t_release_artifacts "$T_ROOT/a-ci-only"
t_artifact "$T_ROOT/a-ci-only" linux 7.4.33-ck1
t_artifact "$T_ROOT/a-ci-only" macos 7.4.33-ck1
t_set "a CI-only line is refused in a publish set" "not an expected release in the release plan" \
  "$T_ROOT/a-ci-only" "$T_ROOT/set-ci-only"
t_set "--allow-partial accepts the same smoke artifacts" "" --allow-partial \
  "$T_ROOT/a-ci-only" "$T_ROOT/set-smoke"

t_artifact "$T_ROOT/a-linux" linux 8.5.13-ck1
t_set "a version without its macOS archive is refused" "has no macos-aarch64 archive" \
  "$T_ROOT/a-linux" "$T_ROOT/set-linux"
t_set "--allow-partial accepts it for a test build" "" --allow-partial \
  "$T_ROOT/a-linux" "$T_ROOT/set-partial"

t_release_artifacts "$T_ROOT/a-extra"
printf 'x\n' >"$T_ROOT/a-extra/notes.txt"
t_set "an extra file in an artifact is refused" "is neither an archive nor a descriptor" \
  "$T_ROOT/a-extra" "$T_ROOT/set-extra"

t_release_artifacts "$T_ROOT/a-wrong"
printf 'changed\n' >>"$T_ROOT/a-wrong/php-8.5.13-ck1-macos-aarch64.tar.gz"
t_set "an archive that differs from its descriptor is refused" "does not describe" \
  "$T_ROOT/a-wrong" "$T_ROOT/set-wrong"

t_release_artifacts "$T_ROOT/a-alone"
rm "$T_ROOT/a-alone/php-8.5.13-ck1-linux-x86_64.json"
t_set "an archive without its descriptor is refused" "has no descriptor" \
  "$T_ROOT/a-alone" "$T_ROOT/set-alone"

t_release_artifacts "$T_ROOT/a-other"
jq '.version = "8.5.13-ck2"' "$T_ROOT/a-other/php-8.5.13-ck1-macos-aarch64.json" >"$T_ROOT/other.json"
mv "$T_ROOT/other.json" "$T_ROOT/a-other/php-8.5.13-ck1-macos-aarch64.json"
t_set "a descriptor for another version is refused" "does not describe" \
  "$T_ROOT/a-other" "$T_ROOT/set-other"

cp -R "$T_ROOT/set" "$T_ROOT/set-crlf"
sed 's/$/\r/' "$T_ROOT/set/php-8.5.13-ck1/SHA256SUMS" >"$T_ROOT/set-crlf/php-8.5.13-ck1/SHA256SUMS"
t_set "--check refuses SHA256SUMS with CRLF line ends" "SHA256SUMS is not exactly" \
  --check "$T_ROOT/set-crlf"

cp -R "$T_ROOT/set" "$T_ROOT/set-stray"
printf 'x\n' >"$T_ROOT/set-stray/php-8.5.13-ck1/.hidden"
t_set "--check refuses a stray file" "holds 6 entries, expected 5" --check "$T_ROOT/set-stray"

# A draft's REST tag can differ from the tag that will be created on publish.
# The fake API rejects requests that use anything other than the recorded ID.
gh() {
  [ "$1" = api ] || return 1
  shift
  printf '%s\n' "$*" >>"$T_ROOT/api-calls"
  local repo=repos/ckm-php/ckm-dev endpoint=$1 name id file hash
  shift
  case "$endpoint" in
    "$repo/releases")
      [ "$*" = "--method POST -F draft=true -f make_latest=false -f tag_name=$T_TAG -f target_commitish=$T_SHA -f name=PHP 8.5.13-ck1 -F body=@$RUNNER_TEMP/notes-$T_TAG.md" ] ||
        return 1
      jq -n --arg target "$T_SHA" --rawfile body "$RUNNER_TEMP/notes-$T_TAG.md" \
        '{id: 42, tag_name: "untagged-d4b97137932eb183d68d", draft: true, target_commitish: $target, body: $body}' |
        tee "$T_ROOT/draft.json"
      ;;
    "$repo/releases?per_page=100")
      [ "$1" = --paginate ] && [ "$2" = --jq ] || return 1
      jq -r "$3" "$T_ROOT/releases.json"
      ;;
    "$repo/git/matching-refs/tags/$T_TAG") cat "$T_ROOT/refs.json" ;;
    "$repo/git/ref/tags/$T_TAG") cat "$T_ROOT/published-ref.json" ;;
    "$repo/releases/42")
      case "${1:-}" in
        --method)
          case "$2" in
            PATCH)
              [ "$*" = "--method PATCH -F draft=false -f make_latest=false -f tag_name=$T_TAG -f target_commitish=$T_SHA" ] ||
                return 1
              jq --arg tag "$T_TAG" '.draft = false | .tag_name = $tag' "$T_ROOT/draft.json"
              ;;
            *) return 1 ;;
          esac
          ;;
        '') jq --slurpfile assets "$T_ROOT/assets.jsonl" '.assets = $assets' "$T_ROOT/draft.json" ;;
        *) return 1 ;;
      esac
      ;;
    "https://uploads.github.com/$repo/releases/42/assets?name="*)
      name=${endpoint##*=}
      file="$RUNNER_TEMP/release-set/$T_TAG/$name"
      [ "$*" = "--method POST -H Content-Type: application/octet-stream --input $file" ] ||
        return 1
      id=$(($(wc -l <"$T_ROOT/assets.jsonl") + 100))
      cp "$file" "$T_ROOT/asset-$id"
      hash=$(sha256sum <"$file" 2>/dev/null || shasum -a 256 <"$file")
      jq -nc --arg name "$name" --argjson id "$id" --arg digest "sha256:${hash%% *}" \
        '{id: $id, name: $name, state: "uploaded", digest: $digest}' >>"$T_ROOT/assets.jsonl"
      ;;
    "$repo/releases/assets/"*)
      [ "$*" = "-H Accept: application/octet-stream" ] || return 1
      cat "$T_ROOT/asset-${endpoint##*/}"
      ;;
    *) return 1 ;;
  esac
}
export -f gh
export T_ROOT T_SHA T_TAG=php-8.5.13-ck1
export RUNNER_TEMP="$T_ROOT/runner"
export GITHUB_REPOSITORY=ckm-php/ckm-dev GITHUB_REF=refs/heads/main GITHUB_SHA="$T_SHA"
export GITHUB_SERVER_URL=https://github.com GITHUB_RUN_ID=46028817425 GITHUB_RUN_ATTEMPT=1
export TAGS="$T_TAG" RUN_ID="$T_RUN" COMMIT="$T_SHA" ATTESTATION_URL=https://github.com/example
mkdir "$RUNNER_TEMP"
mkdir "$RUNNER_TEMP/release-set"
cp -R "$T_ROOT/set-scope/$T_TAG" "$RUNNER_TEMP/release-set/"
: >"$T_ROOT/assets.jsonl"
printf '[]\n' >"$T_ROOT/releases.json"
printf '[]\n' >"$T_ROOT/refs.json"
jq -n --arg sha "$T_SHA" '{object: {sha: $sha}}' >"$T_ROOT/published-ref.json"

t_draft() {
  bash "$PHP_DIR/release-drafts.sh" "$@" >"$T_ROOT/stdout" 2>"$T_ROOT/stderr"
}

t_draft preflight || t_fail "no existing tag or release"
printf '%s\n' '[{"tag_name":"untagged-d4b97137932eb183d68d","draft":true,"name":"PHP 8.5.13-ck1"}]' \
  >"$T_ROOT/releases.json"
if t_draft preflight; then t_fail "a leftover untagged draft was missed"; fi
grep -q "a release or draft for $T_TAG already exists" "$T_ROOT/stderr" ||
  t_fail "wrong reason for refusing a leftover draft"
t_ok "preflight catches a leftover draft with a temporary tag"
printf '[]\n' >"$T_ROOT/releases.json"

t_draft create || t_fail "create an untagged draft by ID"
grep -qx 42 "$RUNNER_TEMP/release-ids/$T_TAG" || t_fail "no recorded draft ID"
t_draft upload || t_fail "upload to the untagged draft by ID"
t_draft verify-assets || t_fail "download from the untagged draft and verify by ID"
t_draft publish || t_fail "publish the untagged draft by ID"
grep -q "releases/42 --method PATCH" "$T_ROOT/api-calls" || t_fail "no publish by ID"
grep -q "git/ref/tags/$T_TAG" "$T_ROOT/api-calls" || t_fail "no published tag check"
t_ok "an untagged draft publishes by ID with its checked tag and commit"

mv "$RUNNER_TEMP/uploaded" "$T_ROOT/verified-upload"
printf 'wrong bytes\n' >>"$T_ROOT/asset-101"
if t_draft verify-assets; then t_fail "an altered remote asset was accepted"; fi
t_ok "assets are uploaded and read by ID, and altered bytes are refused"

: >"$T_ROOT/api-calls"
cp "$T_ROOT/assets.jsonl" "$T_ROOT/verified-assets.jsonl"
jq -c 'if .id == 101 then .id = 999 else . end' "$T_ROOT/verified-assets.jsonl" >"$T_ROOT/assets.jsonl"
if t_draft publish; then t_fail "a draft with a replaced asset was accepted"; fi
grep -q "assets changed since verification" "$T_ROOT/stderr" || t_fail "wrong reason for refusing replaced asset"
if grep -q -- '--method PATCH' "$T_ROOT/api-calls"; then t_fail "publish tried to use a replaced asset"; fi
cp "$T_ROOT/verified-assets.jsonl" "$T_ROOT/assets.jsonl"
t_ok "publish refuses a draft whose asset was replaced after verification"

: >"$T_ROOT/api-calls"
jq -n --arg ref "refs/tags/$T_TAG" '[{ref: $ref}]' >"$T_ROOT/refs.json"
if t_draft publish; then t_fail "an existing tag was not refused"; fi
if grep -q -- '--method PATCH' "$T_ROOT/api-calls"; then t_fail "attempted to reuse a tag"; fi
t_ok "a tag created after preparation is refused"
printf '[]\n' >"$T_ROOT/refs.json"

jq -n '{object: {sha: "fedcba9876543210fedcba9876543210fedcba98"}}' >"$T_ROOT/published-ref.json"
if t_draft publish; then t_fail "a published tag at the wrong commit was accepted"; fi
grep -q "published tag $T_TAG points at" "$T_ROOT/stderr" || t_fail "missing wrong-tag diagnostic"
t_ok "publishing stops if the newly published tag resolves to the wrong commit"

cp "$T_ROOT/draft.json" "$T_ROOT/created.json"
: >"$T_ROOT/api-calls"
jq '.body = "from another run"' "$T_ROOT/created.json" >"$T_ROOT/draft.json"
if t_draft publish; then t_fail "another run's draft was not refused"; fi
if t_draft report; then t_fail "report must flag an unrecognized draft"; fi
if grep -Eq -- '--method (PATCH|DELETE)' "$T_ROOT/api-calls"; then t_fail "touched an unrecognized draft"; fi
t_ok "an unrecognized draft is neither published nor deleted"

: >"$T_ROOT/api-calls"
jq '.draft = false' "$T_ROOT/created.json" >"$T_ROOT/draft.json"
t_draft report || t_fail "could not report a published release"
grep -q "Keeping published release 42" "$T_ROOT/stdout" || t_fail "did not identify the published release"
cp "$T_ROOT/created.json" "$T_ROOT/draft.json"
t_draft report || t_fail "could not report a leftover draft"
grep -q "Draft 42 for $T_TAG remains" "$T_ROOT/stdout" || t_fail "no leftover draft guidance"
if grep -Eq -- '--method (PATCH|DELETE|POST)' "$T_ROOT/api-calls"; then t_fail "report mutated a release"; fi
t_ok "failure reporting distinguishes published releases and drafts without mutating either"

printf 'release-check-selftest: %d checks passed\n' "$T_PASSED"
