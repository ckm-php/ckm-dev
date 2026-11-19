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

# t_artifacts <dir>: both lines on both platforms.
t_artifacts() {
  t_artifact "$1" linux 7.4.33-ck1
  t_artifact "$1" macos 7.4.33-ck1
  t_artifact "$1" linux 8.5.12-ck1
  t_artifact "$1" macos 8.5.12-ck1
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

t_artifacts "$T_ROOT/a-good"
t_set "a full set of artifacts is laid out" "" "$T_ROOT/a-good" "$T_ROOT/set"
t_sums=$(cat "$T_ROOT/set/php-8.5.12-ck1/SHA256SUMS")
t_expected=$(
  cd "$T_ROOT/set/php-8.5.12-ck1" &&
    { sha256sum php-8.5.12-ck1-linux-x86_64.tar.gz php-8.5.12-ck1-macos-aarch64.tar.gz 2>/dev/null ||
      shasum -a 256 php-8.5.12-ck1-linux-x86_64.tar.gz php-8.5.12-ck1-macos-aarch64.tar.gz; }
)
[ "$t_sums" = "$t_expected" ] || t_fail "SHA256SUMS is not what sha256sum writes"
[ "$(find "$T_ROOT/set" -type f | wc -l)" -eq 10 ] || t_fail "the set does not hold 10 files"
t_ok "SHA256SUMS is sha256sum's text form, in name order"
t_set "the laid-out set passes --check" "" --check "$T_ROOT/set"

t_artifact "$T_ROOT/a-linux" linux 8.5.12-ck1
t_set "a version without its macOS archive is refused" "has no macos-aarch64 archive" \
  "$T_ROOT/a-linux" "$T_ROOT/set-linux"
t_set "--allow-partial accepts it for a test build" "" --allow-partial \
  "$T_ROOT/a-linux" "$T_ROOT/set-partial"

t_artifacts "$T_ROOT/a-extra"
printf 'x\n' >"$T_ROOT/a-extra/notes.txt"
t_set "an extra file in an artifact is refused" "is neither an archive nor a descriptor" \
  "$T_ROOT/a-extra" "$T_ROOT/set-extra"

t_artifacts "$T_ROOT/a-wrong"
printf 'changed\n' >>"$T_ROOT/a-wrong/php-8.5.12-ck1-macos-aarch64.tar.gz"
t_set "an archive that differs from its descriptor is refused" "does not describe" \
  "$T_ROOT/a-wrong" "$T_ROOT/set-wrong"

t_artifacts "$T_ROOT/a-alone"
rm "$T_ROOT/a-alone/php-7.4.33-ck1-linux-x86_64.json"
t_set "an archive without its descriptor is refused" "has no descriptor" \
  "$T_ROOT/a-alone" "$T_ROOT/set-alone"

t_artifacts "$T_ROOT/a-other"
jq '.version = "7.4.33-ck2"' "$T_ROOT/a-other/php-7.4.33-ck1-macos-aarch64.json" >"$T_ROOT/other.json"
mv "$T_ROOT/other.json" "$T_ROOT/a-other/php-7.4.33-ck1-macos-aarch64.json"
t_set "a descriptor for another version is refused" "does not describe" \
  "$T_ROOT/a-other" "$T_ROOT/set-other"

cp -R "$T_ROOT/set" "$T_ROOT/set-crlf"
sed 's/$/\r/' "$T_ROOT/set/php-7.4.33-ck1/SHA256SUMS" >"$T_ROOT/set-crlf/php-7.4.33-ck1/SHA256SUMS"
t_set "--check refuses SHA256SUMS with CRLF line ends" "SHA256SUMS is not exactly" \
  --check "$T_ROOT/set-crlf"

cp -R "$T_ROOT/set" "$T_ROOT/set-stray"
printf 'x\n' >"$T_ROOT/set-stray/php-8.5.12-ck1/.hidden"
t_set "--check refuses a stray file" "holds 6 entries, expected 5" --check "$T_ROOT/set-stray"

printf 'release-check-selftest: %d checks passed\n' "$T_PASSED"
