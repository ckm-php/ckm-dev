#!/usr/bin/env bash
# plan-matrix.sh <push|pull_request|workflow_dispatch|release> [<lines> <oses> <true|false>]
# Dispatch filters are whitespace-delimited. Push and PR smoke all configured pairs;
# release plans the publishable (non-CI-only) lines on their full platform sets.
# `publishable` is true only for a clean dispatch and for release plans. Stdout
# is one JSON object; no network, build tools or build root are needed.

PLAN_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$PLAN_DIR/lib/common.sh"

event=${1:-}
lines=
oses=
clean=false
case "$event" in
  push | pull_request | release)
    [ "$#" -eq 1 ] || ck_die "plan: $event takes no filters"
    ;;
  workflow_dispatch)
    [ "$#" -eq 4 ] || ck_die "plan: dispatch needs lines, oses and a clean flag"
    lines=$2 oses=$3 clean=$4
    case "$clean" in true | false) ;; *) ck_die "plan: clean must be true or false" ;; esac
    ;;
  *) ck_die "plan: unsupported event" ;;
esac

ck_require_cmd jq
jq -cen \
  --arg event "$event" --arg lines "$lines" --arg oses "$oses" --argjson clean "$clean" \
  --slurpfile manifests "$CK_REPO_ROOT/catalog/php-lines.json" \
  --slurpfile locks "$CK_REPO_ROOT/catalog/sources.lock.json" '
  def require($ok; $message): if $ok then . else error("plan: " + $message) end;
  def tokens: [splits("\\s+")] | map(select(length > 0)) | unique;

  # This allow-list describes implemented jobs, not line eligibility or release policy.
  # Add Windows only when its separate ZIP build/contract path is ready.
  {
    "linux-x86_64": {os: "linux", arch: "x86_64", runner: "ubuntu-24.04"},
    "macos-aarch64": {os: "macos", arch: "aarch64", runner: "macos-15"}
  } as $platforms
  | require(($manifests | length == 1) and ($locks | length == 1); "expected one manifest and one lock")
  | $manifests[0] as $manifest
  | $locks[0] as $lock
  | "0|[1-9][0-9]{0,3}" as $number
  | require($manifest.schema == 1 and ($manifest.lines | type == "array" and length > 0);
      "invalid PHP line manifest")
  | require($lock.schema == 1 and ($lock.entries | type == "array"); "invalid sources lock")
  | require(all($manifest.lines[];
      (.line | type == "string" and test("\\A(\($number))\\.(\($number))\\z"))
      and (.stack | . == "legacy" or . == "current")
      and (.stack as $stack | $manifest.stacks | has($stack))
      and (.build_revision | type == "number" and . >= 1 and . <= 4294967295 and . == floor)
      and (.ci_only | type == "boolean")
      and (.platforms | type == "array" and length > 0 and unique == sort
        and all(.[]; . as $id | type == "string" and ($platforms | has($id)))));
      "invalid line, stack, build revision, CI-only flag or platform policy")
  | ($manifest.lines | map(.line)) as $known_lines
  | require(($known_lines | unique | length) == ($known_lines | length); "duplicate PHP line")
  | ($manifest.lines | map(.platforms[]) | unique | map($platforms[.].os) | unique) as $known_oses
  | (if $event == "workflow_dispatch" then ($lines | tokens)
     elif $event == "release" then [$manifest.lines[] | select(.ci_only | not) | .line]
     else $known_lines end) as $want_lines
  | (if $event == "workflow_dispatch" then ($oses | tokens) else $known_oses end) as $want_oses
  | require(($want_lines | length > 0) and ($want_oses | length > 0); "choose at least one PHP line and one OS")
  | require(($want_lines - $known_lines | length == 0); "unknown PHP line in lines; allowed: \($known_lines | join(", "))")
  | require(($want_oses - $known_oses | length == 0); "unknown OS in oses; allowed: \($known_oses | join(", "))")
  | [$manifest.lines[] | select(.line as $line | $want_lines | index($line))] as $selected
  | [$selected[] as $line | $line.platforms[] | $platforms[.]
     | select(.os as $os | $want_oses | index($os))
     | . + ($line | {line, stack})] as $pairs
  | require(($want_lines - ($pairs | map(.line)) | length == 0)
        and ($want_oses - ($pairs | map(.os)) | length == 0); "a requested line or OS has no eligible build")
  | (($pairs | length) == ($selected | map(.platforms | length) | add)) as $complete
  | (($event == "release") or ($event == "workflow_dispatch" and $clean)) as $publishable
  | require(($publishable | not) or all($selected[]; .ci_only | not); "a clean build cannot include a CI-only PHP line")
  | require(($publishable | not) or ($complete and all($selected[];
      ([.platforms[] | $platforms[.].os] | unique) == $want_oses));
      "a clean build needs every eligible platform and one shared OS scope for all selected lines")
  | [$selected[] | .line as $line
     | [$lock.entries[] | select(.name == "php")
        | .version | select(type == "string" and (split(".")[:-1] | join(".")) == $line)] as $pins
     | require(($pins | length == 1) and ($pins[0] | test("\\A([0-9]+\\.){2}(\($number))\\z"));
         "PHP \($line) needs exactly one valid source pin")
     | {key: $line, value: "\($pins[0])-ck\(.build_revision)"}] | from_entries as $versions
  | {
      tool: {include: [$platforms["linux-x86_64"], ($pairs[] | {os, arch, runner})] | unique_by(.os, .arch)},
      deps: {include: [$pairs[] | {os, arch, runner, stack}] | unique_by(.os, .arch, .stack)},
      php: {include: [$pairs[] | . + {version: $versions[.line]}] | sort_by(.os, .arch, .line)},
      lines: [$selected[] | .line],
      oses: [$pairs[] | .os] | unique,
      publishable: $publishable,
      complete: $complete
    }'
