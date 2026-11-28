#!/usr/bin/env bash
# No-network tests: real smoke/clean defaults and synthetic policy changes.
TEST_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=../lib/common.sh
. "$TEST_DIR/../lib/common.sh"

plan=$TEST_DIR/../plan-matrix.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/ck-matrix-test.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
lock=$CK_REPO_ROOT/catalog/sources.lock.json
manifest=$CK_REPO_ROOT/catalog/php-lines.json

reject() {
  local message=$1
  shift
  if bash "$plan" "$@" >"$tmp/out" 2>"$tmp/err"; then
    ck_die "plan self-test: unexpectedly accepted a selection"
  fi
  if [ -s "$tmp/out" ] || ! grep -qF -- "$message" "$tmp/err"; then
    cat "$tmp/err" >&2
    ck_die "plan self-test: expected refusal: $message"
  fi
}

smoke=$(bash "$plan" push)
[ "$smoke" = "$(bash "$plan" pull_request)" ]
[ "$smoke" = "$(bash "$plan" workflow_dispatch $'8.5\t7.4\n8.4 8.2\n8.3 8.5' 'macos linux linux' false)" ]
jq -e '
  .complete and (.tool.include == [
    {os: "linux", arch: "x86_64", runner: "ubuntu-24.04"},
    {os: "macos", arch: "aarch64", runner: "macos-15"}
  ])
  and ([.php.include[] | [.os, .line, .stack]] == [
    ["linux", "7.4", "legacy"], ["linux", "8.2", "current"],
    ["linux", "8.3", "current"], ["linux", "8.4", "current"],
    ["linux", "8.5", "current"], ["macos", "7.4", "legacy"],
    ["macos", "8.2", "current"], ["macos", "8.3", "current"],
    ["macos", "8.4", "current"], ["macos", "8.5", "current"]
  ])
  and (.deps.include | length == 4)
  and (.lines == ["7.4", "8.2", "8.3", "8.4", "8.5"])
  and (.oses == ["linux", "macos"])
  and (.publishable | not)
' <<<"$smoke" >/dev/null
printf 'plan self-test: push/PR smoke, stable order and deduplication OK\n'

clean=$(bash "$plan" workflow_dispatch '8.5' 'linux macos' true)
version=$(jq -er '.entries[] | select(.name == "php" and (.version | startswith("8.5."))) | .version' "$lock")
revision=$(jq -er '.lines[] | select(.line == "8.5") | .build_revision' "$manifest")
jq -e --arg version "$version-ck$revision" '
  .complete and (.php.include | length == 2)
  and all(.php.include[]; .line == "8.5" and .stack == "current" and .version == $version)
  and ([.deps.include[] | [.os, .stack]] == [["linux", "current"], ["macos", "current"]])
  and (.lines == ["8.5"]) and (.oses == ["linux", "macos"]) and .publishable
' <<<"$clean" >/dev/null
bash "$plan" workflow_dispatch '7.4' macos false | jq -e '
  (.complete | not) and ([.tool.include[].os] == ["linux", "macos"])
  and ([.deps.include[] | [.os, .stack]] == [["macos", "legacy"]])
  and ([.php.include[] | [.os, .line]] == [["macos", "7.4"]])
' >/dev/null
printf 'plan self-test: selected stacks, release version, full clean and partial smoke OK\n'

release=$(bash "$plan" release)
jq -e --slurpfile manifest "$manifest" --slurpfile lock "$lock" '
  ($manifest[0].lines
   | map(select(.ci_only | not)
         | . as $line
         | $lock[0].entries[] as $pin
         | select($pin.name == "php"
                  and (($pin.version | split(".")[:-1] | join(".")) == $line.line))
         | "\($pin.version)-ck\($line.build_revision)")
   | sort) as $versions
  | .complete
  and (.php.include | length == 8)
  and (([.php.include[].version] | unique | sort) == $versions)
  and ([.php.include[].line] | unique == ["8.2", "8.3", "8.4", "8.5"])
  and all(.php.include[]; .stack == "current")
  and ([.php.include[] | "\(.os)-\(.arch)"] | unique == ["linux-x86_64", "macos-aarch64"])
  and (.lines == ["8.2", "8.3", "8.4", "8.5"])
  and (.oses == ["linux", "macos"]) and .publishable
' <<<"$release" >/dev/null
printf 'plan self-test: release plan excludes CI-only lines and keeps every platform OK\n'

reject 'choose at least one' workflow_dispatch ' ' linux false
reject 'choose at least one' workflow_dispatch 8.5 ' ' false
reject 'unknown PHP line' workflow_dispatch '8.5 *' linux false
reject 'unknown PHP line' workflow_dispatch '8.5 8.0' linux false
reject 'unknown OS' workflow_dispatch 8.5 'linux windows' false
reject 'clean must be true or false' workflow_dispatch 8.5 linux TRUE
reject 'needs every eligible platform' workflow_dispatch 8.5 linux true
reject 'needs every eligible platform' workflow_dispatch 8.5 macos true
reject 'CI-only PHP line' workflow_dispatch '7.4 8.5' 'linux macos' true
printf 'plan self-test: empty/unknown filters, partial clean and CI-only clean refused\n'

# Add a synthetic line without touching the production policy. Its stack is
# only needed on Linux, and the version must follow the lock and revision.
mkdir -p "$tmp/build/php/lib" "$tmp/catalog"
cp "$plan" "$tmp/build/php/plan-matrix.sh"
cp "$TEST_DIR/../lib/common.sh" "$tmp/build/php/lib/"
plan=$tmp/build/php/plan-matrix.sh
jq '.lines += [{line: "9.9", stack: "current", platforms: ["linux-x86_64"], ci_only: false, build_revision: 3}]' \
  "$manifest" >"$tmp/catalog/php-lines.json"
jq '.entries += [{name: "php", version: "9.9.7"}]' "$lock" >"$tmp/catalog/sources.lock.json"
bash "$plan" workflow_dispatch '7.4 9.9' 'linux macos' false | jq -e '
  (.complete) and (.php.include | length == 3)
  and ([.deps.include[] | [.os, .stack]] == [
    ["linux", "current"], ["linux", "legacy"], ["macos", "legacy"]
  ])
  and ([.php.include[] | select(.line == "9.9") | [.os, .version]] == [["linux", "9.9.7-ck3"]])
' >/dev/null
reject 'no eligible build' workflow_dispatch '9.9' macos false
reject 'one shared OS scope' workflow_dispatch '8.5 9.9' 'linux macos' true
cp "$lock" "$tmp/catalog/sources.lock.json"
reject 'exactly one valid source pin' workflow_dispatch '9.9' linux false
jq '.lines[0].platforms += ["windows-x86_64"]' "$manifest" >"$tmp/catalog/php-lines.json"
reject 'invalid line, stack, build revision, CI-only flag or platform policy' push
printf 'plan self-test: manifest changes affect pairings; mixed clean scope, missing pin and unsupported platform refused\n'
