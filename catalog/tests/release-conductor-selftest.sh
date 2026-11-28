#!/usr/bin/env bash
# No-network self-test for catalog/release.sh. External commands are stubbed.
set -euo pipefail
IFS=$'\n\t'

SELF_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CONDUCTOR="$SELF_DIR/release.sh"
ROOT=$(mktemp -d "${TMPDIR:-/tmp/opencode}/release-conductor.XXXXXX")
export ROOT
trap 'rm -rf -- "$ROOT"' EXIT
mkdir -p "$ROOT/bin"

HEAD='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
CHECKS=0

pass() {
  CHECKS=$((CHECKS + 1))
  printf 'ok: %s\n' "$1"
}

fail() {
  printf 'not ok: %s\n' "$1" >&2
  if [[ -n ${CASE:-} && -d $CASE ]]; then
    local f
    for f in "$CASE"/out "$CASE"/publish1 "$CASE"/publish2 "$CASE"/auto1 "$CASE"/auto2 "$CASE"/auto3 "$CASE"/gh.log; do
      if [[ -f $f ]]; then
        printf '%s\n' "--- $f ---" >&2
        cat "$f" >&2
      fi
    done
  fi
  exit 1
}

assert_contains() {
  local file=$1 pattern=$2 label=$3
  if ! grep -qF -- "$pattern" "$file"; then
    printf '%s\n' "--- $file ---" >&2
    cat "$file" >&2
    fail "$label"
  fi
  pass "$label"
}

assert_not_contains() {
  local file=$1 pattern=$2 label=$3
  if grep -qF -- "$pattern" "$file"; then fail "$label"; fi
  pass "$label"
}

cat >"$ROOT/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_LOG"
args=("$@")
jq_filter=''
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ ${args[i]} == --jq ]]; then
    jq_filter=${args[i + 1]}
  fi
done
out_file="$FAKE_RUN_DIR/gh-output.$$"

{
  case "${args[0]}" in
    api)
      case "${args[1]}" in
        repos/ckm-php/ckm-dev/commits/main) printf '{"sha":"%s"}\n' "$FAKE_HEAD" ;;
        repos/ckm-php/ckm-dev/immutable-releases) printf '{"enabled":true}\n' ;;
        repos/ckm-php/ckm-dev/git/ref/tags/*) ;;
        *) printf 'unexpected gh api: %s\n' "${args[1]}" >&2; exit 99 ;;
      esac
      ;;
    variable)
      printf 'armed\n'
      ;;
    run)
      case "${args[1]}" in
        list)
          workflow=''
          for arg in "${args[@]}"; do
            case "$arg" in
              *.yml) workflow=$arg ;;
            esac
          done
          if [[ $workflow == ci.yml ]]; then
            printf '[{"databaseId":"%s","headSha":"%s","status":"completed","conclusion":"success"}]\n' \
              "$FAKE_CI_RUN_ID" "$FAKE_HEAD"
          else
            if [[ ${FAKE_STALE_FIRST:-0} == 1 && ! -e $FAKE_RUN_DIR/stale-used ]]; then
              : >"$FAKE_RUN_DIR/stale-used"
              printf '[{"databaseId":"99999"}]\n'
            else
              id=$(cat "$FAKE_RUN_DIR/last.$workflow" 2>/dev/null || true)
              printf '[{"databaseId":"%s"}]\n' "$id"
            fi
          fi
          ;;
        view)
          id=${args[2]}
          status=$(cat "$FAKE_RUN_DIR/$id.status" 2>/dev/null || printf 'completed')
          conclusion=$(cat "$FAKE_RUN_DIR/$id.conclusion" 2>/dev/null || printf 'success')
          printf '{"status":"%s","conclusion":"%s","headSha":"%s"}\n' \
            "$status" "$conclusion" "$FAKE_HEAD"
          ;;
        download)
          out=''
          for ((i = 0; i < ${#args[@]}; i++)); do
            if [[ ${args[i]} == -D ]]; then out=${args[i + 1]}; fi
          done
          [[ -n $out ]] || { printf 'missing -D\n' >&2; exit 99; }
          mkdir -p "$out/php-8.5.13-ck1"
          set_dir="$out/php-8.5.13-ck1"
          printf '%s  %s\n' "$FAKE_HASH1" 'php-8.5.13-ck1-linux-x86_64.tar.gz' >"$set_dir/SHA256SUMS"
          printf '%s  %s\n' "$FAKE_HASH2" 'php-8.5.13-ck1-macos-aarch64.tar.gz' >>"$set_dir/SHA256SUMS"
          : >"$set_dir/php-8.5.13-ck1-linux-x86_64.tar.gz"
          : >"$set_dir/php-8.5.13-ck1-macos-aarch64.tar.gz"
          ;;
        *) printf 'unexpected gh run: %s\n' "${args[1]}" >&2; exit 99 ;;
      esac
      ;;
    workflow)
      [[ ${args[1]} == run ]] || { printf 'unexpected workflow command\n' >&2; exit 99; }
      workflow=${args[2]}
      n=$(cat "$FAKE_RUN_DIR/counter" 2>/dev/null || printf '0')
      n=$((n + 1))
      printf '%s\n' "$n" >"$FAKE_RUN_DIR/counter"
      printf '%s%s\n' "$FAKE_RUN_PREFIX" "$n" >"$FAKE_RUN_DIR/last.$workflow"
      ;;
    release)
      case "${args[1]}" in
        view) printf '{"isDraft":false}\n' ;;
        download)
          tag=${args[2]}
          if [[ $tag == catalog-v1 ]]; then
            cat "$FAKE_CATALOG_BYTES"
          else
            out=''
            for ((i = 0; i < ${#args[@]}; i++)); do
              if [[ ${args[i]} == -D ]]; then out=${args[i + 1]}; fi
            done
            [[ -n $out ]] || { printf 'missing -D\n' >&2; exit 99; }
            mkdir -p "$out"
            : >"$out/php-8.5.13-ck1-linux-x86_64.tar.gz"
            : >"$out/php-8.5.13-ck1-macos-aarch64.tar.gz"
            printf '{}\n' >"$out/php-8.5.13-ck1-linux-x86_64.json"
            printf '{}\n' >"$out/php-8.5.13-ck1-macos-aarch64.json"
          fi
          ;;
        upload|delete-asset)
          printf 'FORBIDDEN %s\n' "$*" >>"$FAKE_LOG"
          exit 99
          ;;
        *) printf 'unexpected gh release: %s\n' "${args[1]}" >&2; exit 99 ;;
      esac
      ;;
    attestation)
      ;;
    *)
      printf 'unexpected gh command: %s\n' "${args[0]}" >&2
      exit 99
      ;;
  esac
} >"$out_file"

if [[ -n $jq_filter ]]; then
  jq -r "$jq_filter" "$out_file"
else
  cat "$out_file"
fi
EOF

cat >"$ROOT/bin/sha256sum" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == -c ]]; then
  printf '%s: OK\n' "${2:-SHA256SUMS}"
  exit 0
fi
printf '%s  %s\n' "$FAKE_HASH1" './php-8.5.13-ck1-linux-x86_64.tar.gz'
printf '%s  %s\n' "$FAKE_HASH2" './php-8.5.13-ck1-macos-aarch64.tar.gz'
EOF

cat >"$ROOT/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_LOG"
mkdir -p target/debug
cp "$ROOT/bin/ck-release" target/debug/ck-release
chmod +x target/debug/ck-release
EOF

cat >"$ROOT/bin/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_LOG"
args=("$@")
case "${args[0]:-}" in
  status)
    if [[ ${FAKE_GIT_DIRTY:-0} == 1 ]]; then printf '?? junk\n'; fi
    ;;
  rev-parse)
    printf '%s\n' "$FAKE_HEAD"
    ;;
  *)
    printf 'FORBIDDEN %s\n' "$*" >>"$FAKE_LOG"
    exit 99
    ;;
esac
EOF

cat >"$ROOT/bin/ck-release" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_LOG"
if [[ ${1:-} == payload ]]; then
  out=''
  while [[ $# -gt 0 ]]; do
    if [[ $1 == --out ]]; then out=$2; shift; fi
    shift || true
  done
  mkdir -p "$(dirname "$out")"
  cat >"$out" <<'JSON'
{
  "sequence": 1,
  "expires": "2027-02-26T00:00:00Z",
  "runtimes": [
    {
      "name": "php",
      "version": "8.5.13-ck1",
      "artifacts": [
        {"os": "linux", "arch": "x86_64", "sha256": "aaa", "url": "https://github.com/ckm-php/ckm-dev/releases/download/php-8.5.13-ck1/php-8.5.13-ck1-linux-x86_64.tar.gz"},
        {"os": "macos", "arch": "aarch64", "sha256": "bbb", "url": "https://github.com/ckm-php/ckm-dev/releases/download/php-8.5.13-ck1/php-8.5.13-ck1-macos-aarch64.tar.gz"}
      ]
    }
  ]
}
JSON
elif [[ ${1:-} == assets ]]; then
  printf 'ok: 2 artifacts match the release files\n'
fi
EOF

chmod +x "$ROOT/bin/"*

new_case() {
  local name=$1
  CASE="$ROOT/$name"
  mkdir -p "$CASE/home" "$CASE/state" "$CASE/rundir" "$CASE/work/catalog"
  export HOME="$CASE/home"
  export CK_RELEASE_STATE_DIR="$CASE/state"
  export CK_RELEASE_POLL_SECONDS=0
  export CK_RELEASE_MAX_POLLS=3
  export PATH="$ROOT/bin:$PATH"
  export FAKE_LOG="$CASE/gh.log"
  export FAKE_RUN_DIR="$CASE/rundir"
  export FAKE_HEAD="$HEAD"
  export FAKE_CI_RUN_ID='90001'
  export FAKE_RUN_PREFIX='1000'
  FAKE_HASH1=$(printf 'a%.0s' {1..64})
  FAKE_HASH2=$(printf 'b%.0s' {1..64})
  export FAKE_HASH1 FAKE_HASH2
  export FAKE_CATALOG_BYTES=''
  export FAKE_GIT_DIRTY=0
  export FAKE_STALE_FIRST=0
  : >"$FAKE_LOG"
  cd "$CASE/work"
}

# 1. prepare refuses a commit that is not main's head.
new_case wrong-head
if "$CONDUCTOR" prepare 0000000000000000000000000000000000000000 >"$CASE/out" 2>&1; then
  fail 'prepare accepted a wrong commit'
fi
assert_contains "$CASE/out" "not main's head" 'prepare refuses a wrong commit'

# 2. prepare refuses to dispatch a clean build after a failed probe.
new_case failed-probe
printf 'failure\n' >"$FAKE_RUN_DIR/10001.conclusion"
if "$CONDUCTOR" prepare >"$CASE/out" 2>&1; then
  fail 'prepare continued after a failed probe'
fi
assert_contains "$CASE/out" 'Catalog check probe run 10001: completed failure' 'failed probe is reported'
assert_not_contains "$FAKE_LOG" 'workflow run php-build.yml' 'no clean build after a failed probe'

# 3. prepare records probe, clean build and hash check.
new_case prepare-ok
"$CONDUCTOR" prepare >"$CASE/out"
jq -e '
  .phase == "clean-ready" and
  .probe_run_id != "" and
  .clean_run_id != "" and
  .clean_commit == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" and
  .scope_lines == "8.5" and
  .scope_oses == "linux macos" and
  (.hashes | length > 0)
' "$CK_RELEASE_STATE_DIR/release-state.json" >/dev/null
pass 'prepare records probe, clean run, scope and hashes'
assert_contains "$FAKE_LOG" 'workflow run php-build.yml -R ckm-php/ckm-dev --ref main -f clean=true -f lines=8.5 -f oses=linux macos' 'clean build uses the recorded scope'
assert_contains "$FAKE_LOG" 'run download 10002 -R ckm-php/ckm-dev -n release-set' 'candidate artifact is downloaded automatically'

new_case stale-latest
export FAKE_STALE_FIRST=1
"$CONDUCTOR" prepare >"$CASE/out"
jq -e '.probe_run_id == "10001" and .clean_run_id == "10002"' \
  "$CK_RELEASE_STATE_DIR/release-state.json" >/dev/null
pass 'dispatch records the new run, not the stale latest run'

# 4. publish refuses a stale candidate and a wrong scope.
new_case stale
"$CONDUCTOR" prepare >/dev/null
jq '.clean_created_at = "2020-01-01T00:00:00Z"' "$CK_RELEASE_STATE_DIR/release-state.json" >"$CASE/state.json"
mv "$CASE/state.json" "$CK_RELEASE_STATE_DIR/release-state.json"
if "$CONDUCTOR" publish >"$CASE/out" 2>&1; then
  fail 'publish accepted a stale candidate'
fi
assert_contains "$CASE/out" 'older than 7 days' 'publish refuses a stale candidate'

new_case wrong-scope
"$CONDUCTOR" prepare >/dev/null
jq '.scope_oses = "windows"' "$CK_RELEASE_STATE_DIR/release-state.json" >"$CASE/state.json"
mv "$CASE/state.json" "$CK_RELEASE_STATE_DIR/release-state.json"
if "$CONDUCTOR" publish >"$CASE/out" 2>&1; then
  fail 'publish accepted a wrong scope'
fi
assert_contains "$CASE/out" 'candidate OSes are not linux macos' 'publish refuses a wrong scope'

# 5. catalog-prepare stops before signing or uploading.
new_case sign-gate
"$CONDUCTOR" prepare >/dev/null
"$CONDUCTOR" publish >"$CASE/publish1"
assert_contains "$CASE/publish1" 'APPROVAL GATE' 'publish stops at the approval gate'
"$CONDUCTOR" publish >"$CASE/publish2"
assert_contains "$CASE/publish2" 'published release and tag' 'resumed publish verifies the release'
"$CONDUCTOR" catalog-prepare >"$CASE/out"
assert_contains "$CASE/out" 'SIGN GATE' 'catalog-prepare stops at the sign gate'
assert_contains "$CASE/out" 'openssl pkey' 'sign command is printed'
assert_not_contains "$FAKE_LOG" 'release upload' 'sign gate does not upload'
assert_not_contains "$FAKE_LOG" 'release delete-asset' 'sign gate does not delete the probe'
assert_not_contains "$FAKE_LOG" 'FORBIDDEN git' 'sign gate does not write Git'
assert_contains "$FAKE_LOG" 'status --porcelain' 'catalog-prepare checks the checkout is clean'
jq -e '.phase == "catalog-prepared" and .catalog_prepared == "true"' \
  "$CK_RELEASE_STATE_DIR/release-state.json" >/dev/null
pass 'catalog-prepare records its phase'

new_case dirty-checkout
"$CONDUCTOR" prepare >/dev/null
"$CONDUCTOR" publish >/dev/null
"$CONDUCTOR" publish >/dev/null
export FAKE_GIT_DIRTY=1
if "$CONDUCTOR" catalog-prepare >"$CASE/out" 2>&1; then
  fail 'catalog-prepare accepted a dirty checkout'
fi
assert_contains "$CASE/out" 'checkout is dirty' 'catalog-prepare refuses a dirty checkout'
assert_not_contains "$FAKE_LOG" 'payload build' 'dirty checkout does not build a payload'

# 6. auto advances through each human gate with the same command.
new_case auto
"$CONDUCTOR" auto >"$CASE/auto1"
assert_contains "$CASE/auto1" 'APPROVAL GATE' 'first auto stops at approval'
assert_contains "$FAKE_LOG" 'workflow run catalog-check.yml -R ckm-php/ckm-dev -f mode=probe' 'auto runs the probe'
assert_contains "$FAKE_LOG" 'workflow run php-build.yml -R ckm-php/ckm-dev --ref main -f clean=true -f lines=8.5 -f oses=linux macos' 'auto runs the clean build'
assert_contains "$FAKE_LOG" 'workflow run php-publish.yml -R ckm-php/ckm-dev -f run_id=10002 -f commit=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' 'auto dispatches publish with recorded ids'

"$CONDUCTOR" auto >"$CASE/auto2"
assert_contains "$CASE/auto2" 'SIGN GATE' 'second auto stops before signing'

printf 'signed-catalog-bytes\n' >"$CASE/work/catalog/catalog-v1.json"
export FAKE_CATALOG_BYTES="$CASE/work/catalog/catalog-v1.json"
"$CONDUCTOR" auto >"$CASE/auto3"
assert_contains "$FAKE_LOG" 'workflow run catalog-check.yml -R ckm-php/ckm-dev -f mode=run' 'third auto runs the final catalog check'
assert_contains "$CASE/auto3" 'CATALOG CHECK GREEN' 'third auto reports the final gate'
assert_contains "$CASE/auto3" 'git commit -m' 'third auto prints the data-only commit hint'
assert_not_contains "$FAKE_LOG" 'FORBIDDEN git' 'auto never runs Git'

printf 'release-conductor-selftest: %d checks passed\n' "$CHECKS"
