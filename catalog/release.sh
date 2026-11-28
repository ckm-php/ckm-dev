#!/usr/bin/env bash
# Local release conductor for the first signed catalog.
#
# One repeatable command advances the safe phases and stops at each human
# gate. It never approves the release environment, signs, uploads, writes Git
# history, or runs the app.
set -euo pipefail
IFS=$'\n\t'

REPO='ckm-php/ckm-dev'
CI_WORKFLOW='ci.yml'
BUILD_WORKFLOW='php-build.yml'
PUBLISH_WORKFLOW='php-publish.yml'
CATALOG_WORKFLOW='catalog-check.yml'
EXPECTED_LINES='8.5'
EXPECTED_OSES='linux macos'
EXPECTED_SEQUENCE='1'
MAX_CANDIDATE_AGE_DAYS=7

STATE_DIR="${CK_RELEASE_STATE_DIR:-$HOME/ck-release-work}"
STATE_FILE="$STATE_DIR/release-state.json"
CANDIDATE_DIR="$STATE_DIR/candidate"
RELEASES_DIR="$STATE_DIR/releases"
POLL_SECONDS="${CK_RELEASE_POLL_SECONDS:-15}"
MAX_POLLS="${CK_RELEASE_MAX_POLLS:-240}"

die() {
  printf 'release: error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '%s\n' "$*"
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing command: $1"
}

now() {
  date -u +%Y-%m-%dT%H:%M:%SZ
}

parse_date() {
  if date -u -d "$1" +%s >/dev/null 2>&1; then
    date -u -d "$1" +%s
  else
    date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "$1" +%s
  fi
}

expiry_90_days() {
  if date -u -d '+90 days' +%Y-%m-%dT00:00:00Z >/dev/null 2>&1; then
    date -u -d '+90 days' +%Y-%m-%dT00:00:00Z
  else
    date -u -v+90d +%Y-%m-%dT00:00:00Z
  fi
}

current_head() {
  local head
  head=$(gh api "repos/$REPO/commits/main" --jq '.sha')
  [[ $head =~ ^[0-9a-f]{40}$ ]] || die "main head is not a 40-hex commit: $head"
  printf '%s\n' "$head"
}

state_init() {
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR" 2>/dev/null || true
  if [[ ! -f $STATE_FILE ]]; then
    local head
    head=$(current_head)
    jq -n \
      --arg head "$head" \
      --arg candidate "$CANDIDATE_DIR" \
      --arg now "$(now)" \
      '{
        version: 1,
        head: $head,
        phase: "new",
        created_at: $now,
        updated_at: $now,
        probe_run_id: "",
        clean_run_id: "",
        clean_commit: "",
        clean_created_at: "",
        scope_lines: "",
        scope_oses: "",
        candidate_dir: $candidate,
        tag: "",
        hashes: "",
        publish_run_id: "",
        published: "false",
        catalog_prepared: "false",
        catalog_run_id: "",
        expiry: ""
      }' >"$STATE_FILE"
    chmod 600 "$STATE_FILE" 2>/dev/null || true
  fi
}

require_state() {
  need jq
  state_init
  [[ $(jq -r '.version // ""' "$STATE_FILE") == 1 ]] || die "unsupported state file version"
}

field() {
  jq -r --arg key "$1" '.[$key] // ""' "$STATE_FILE"
}

set_field() {
  local tmp
  tmp=$(mktemp "$STATE_DIR/.release-state.XXXXXX")
  jq --arg key "$1" --arg value "$2" --arg at "$(now)" \
    '.[$key] = $value | .updated_at = $at' "$STATE_FILE" >"$tmp"
  mv "$tmp" "$STATE_FILE"
  chmod 600 "$STATE_FILE" 2>/dev/null || true
}

require_head_unchanged() {
  local head state_head
  head=$(current_head)
  state_head=$(field head)
  [[ $head == "$state_head" ]] || die "main head changed from $state_head to $head; state is stale"
  printf '%s\n' "$head"
}

require_clean() {
  local phase age created
  phase=$(field phase)
  case "$phase" in
    clean-ready|publish-dispatched|published|catalog-prepared|catalog-checked) ;;
    *) die "no clean candidate recorded (phase: ${phase:-none})" ;;
  esac
  [[ $(field clean_commit) == "$(field head)" ]] || die "clean candidate is not at the state's main head"
  [[ $(field scope_lines) == "$EXPECTED_LINES" ]] || die "candidate lines are not $EXPECTED_LINES"
  [[ $(field scope_oses) == "$EXPECTED_OSES" ]] || die "candidate OSes are not $EXPECTED_OSES"
  created=$(field clean_created_at)
  [[ -n $created ]] || die "candidate has no creation time"
  age=$(( $(date +%s) - $(parse_date "$created") ))
  (( age <= MAX_CANDIDATE_AGE_DAYS * 86400 )) || die "clean candidate is older than $MAX_CANDIDATE_AGE_DAYS days"
}

run_status() {
  gh run view "$1" -R "$REPO" --json status,conclusion,headSha \
    --jq '[.status, (.conclusion // "-"), .headSha] | @tsv'
}

wait_run() {
  local id=$1 label=$2 polls=0 status conclusion _sha line
  while :; do
    line=$(run_status "$id")
    IFS=$'\t' read -r status conclusion _sha <<<"$line"
    note "$label run $id: $status ${conclusion}"
    if [[ $status == completed ]]; then
      [[ $conclusion == success ]] || die "$label run $id ended with $conclusion"
      return 0
    fi
    polls=$((polls + 1))
    (( polls < MAX_POLLS )) || die "timed out waiting for $label run $id"
    sleep "$POLL_SECONDS"
  done
}

latest_dispatch_run() {
  gh run list -R "$REPO" --workflow "$1" --event workflow_dispatch -L 1 \
    --json databaseId --jq '.[0].databaseId // ""'
}

dispatch() {
  local workflow=$1 previous id='' tries=0
  shift
  previous=$(latest_dispatch_run "$workflow")
  gh workflow run "$workflow" -R "$REPO" "$@"
  while :; do
    id=$(latest_dispatch_run "$workflow")
    if [[ -n $id && $id != "$previous" ]]; then
      printf '%s\n' "$id"
      return 0
    fi
    tries=$((tries + 1))
    (( tries < 10 )) || die "could not find a new $workflow run"
    sleep "$POLL_SECONDS"
  done
}

check_ci() {
  local commit=$1 id
  id=$(gh run list -R "$REPO" --workflow "$CI_WORKFLOW" -L 20 \
    --json databaseId,headSha,status,conclusion |
    jq -r --arg commit "$commit" 'map(select(.headSha == $commit)) | first | .databaseId // ""')
  [[ -n $id ]] || die "no CI run found for $commit"
  wait_run "$id" 'CI'
}

verify_artifact() {
  local run_id=$1 tag='' hashes
  case "$CANDIDATE_DIR" in
    "$STATE_DIR"/*) ;;
    *) die "unsafe candidate directory: $CANDIDATE_DIR" ;;
  esac
  rm -rf -- "$CANDIDATE_DIR"
  mkdir -p "$CANDIDATE_DIR"
  gh run download "$run_id" -R "$REPO" -n release-set -D "$CANDIDATE_DIR"
  local dir
  for dir in "$CANDIDATE_DIR"/*; do
    [[ -d $dir ]] && tag=$(basename "$dir")
  done
  [[ -n $tag ]] || die "release set is empty"
  (cd "$CANDIDATE_DIR/$tag" && sha256sum -c SHA256SUMS >/dev/null)
  hashes=$(cd "$CANDIDATE_DIR/$tag" && sha256sum ./*.tar.gz)
  [[ -n $hashes ]] || die "no archive hashes recorded"
  set_field tag "$tag"
  set_field hashes "$hashes"
}

do_prepare() {
  local commit=${1:-} head phase probe clean
  need gh jq sha256sum
  require_state
  head=$(current_head)
  if [[ -n $commit ]]; then
    [[ $commit =~ ^[0-9a-f]{40}$ ]] || die "commit is not 40 lower-case hex characters"
    [[ $commit == "$head" ]] || die "commit is not main's head: $head"
  fi
  [[ $(field head) == "$head" ]] || die "state is for another head; reset or restart"
  phase=$(field phase)
  case "$phase" in
    new) ;;
    clean-ready|publish-dispatched|published|catalog-prepared|catalog-checked)
      note "clean candidate already recorded: run $(field clean_run_id)"
      return 0
      ;;
    *) die "unknown phase: $phase" ;;
  esac

  check_ci "$head"
  probe=$(field probe_run_id)
  if [[ -z $probe ]]; then
    probe=$(dispatch "$CATALOG_WORKFLOW" -f mode=probe)
    set_field probe_run_id "$probe"
  fi
  wait_run "$probe" 'Catalog check probe'

  clean=$(dispatch "$BUILD_WORKFLOW" \
    --ref main -f clean=true -f lines="$EXPECTED_LINES" -f oses="$EXPECTED_OSES")
  set_field phase 'clean-ready'
  set_field clean_run_id "$clean"
  set_field clean_commit "$head"
  set_field clean_created_at "$(now)"
  set_field scope_lines "$EXPECTED_LINES"
  set_field scope_oses "$EXPECTED_OSES"
  verify_artifact "$clean"
  note "clean candidate ready: run $clean at $head"
}

publish_preflight() {
  local enabled gate
  enabled=$(gh api "repos/$REPO/immutable-releases" --jq '.enabled')
  [[ $enabled == true ]] || die 'release immutability is not enabled'
  gate=$(gh variable get RELEASE_GATE -e release -R "$REPO" 2>/dev/null || true)
  [[ $gate == armed ]] || die 'release environment does not hold RELEASE_GATE=armed'
}

verify_published() {
  local tag draft
  tag=$(field tag)
  [[ -n $tag ]] || die "no release tag recorded"
  draft=$(gh release view "$tag" -R "$REPO" --json isDraft --jq '.isDraft')
  [[ $draft == false ]] || die "release $tag is still a draft"
  gh api "repos/$REPO/git/ref/tags/$tag" >/dev/null || die "tag $tag does not exist"
  note "published release and tag: $tag"
}

do_publish() {
  local head phase id
  need gh jq
  require_state
  head=$(require_head_unchanged)
  require_clean
  phase=$(field phase)
  [[ $phase != new ]] || die 'run prepare first'

  if [[ -z $(field publish_run_id) ]]; then
    publish_preflight
    note "publishing clean run $(field clean_run_id) at $(field clean_commit)"
    note "candidate hashes:"
    note "$(field hashes)"
    id=$(dispatch "$PUBLISH_WORKFLOW" \
      -f run_id="$(field clean_run_id)" -f commit="$head")
    set_field publish_run_id "$id"
    set_field phase 'publish-dispatched'
    note "publish run $id dispatched"
    note "APPROVAL GATE: approve 'release' in the GitHub UI, then run: catalog/release.sh auto"
    return 0
  fi

  wait_run "$(field publish_run_id)" 'PHP publish'
  verify_published
  set_field published 'true'
  set_field phase 'published'
}

print_sign_block() {
  cat <<'EOF'
SIGN GATE: catalog payload is ready. No key was touched.
Sign and upload offline in your own terminal, then run: catalog/release.sh auto

  openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem | \
    ./target/debug/ck-release sign --payload catalog/payload-v1.json --key - --first --out catalog/catalog-v1.json
  ./target/debug/ck-release verify catalog/catalog-v1.json --payload catalog/payload-v1.json
  gh release upload catalog-v1 catalog/catalog-v1.json -R ckm-php/ckm-dev
  gh release download catalog-v1 -R ckm-php/ckm-dev -p catalog-v1.json -O - | cmp - catalog/catalog-v1.json
  gh release delete-asset catalog-v1 transport-probe.txt -R ckm-php/ckm-dev -y
EOF
}

require_clean_checkout() {
  local expected=$1 status local_head
  need git
  status=$(git status --porcelain)
  [[ -z $status ]] || die 'checkout is dirty; commit or stash changes before preparing the catalog'
  local_head=$(git rev-parse HEAD)
  [[ $local_head == "$expected" ]] || die "checkout is at $local_head, not $expected"
}

do_catalog_prepare() {
  local head tag ck expiry n=0 f
  need gh jq cargo cmp
  require_state
  head=$(require_head_unchanged)
  require_clean_checkout "$head"
  [[ $(field published) == true ]] || die 'publish is not complete'
  if [[ $(field catalog_prepared) == true ]]; then
    note 'catalog payload already prepared'
    print_sign_block
    return 0
  fi
  [[ ! -e catalog/payload-v1.json ]] || die 'catalog/payload-v1.json already exists; move it aside first'
  tag=$(field tag)
  [[ -n $tag ]] || die 'no release tag recorded'

  export PATH="$HOME/.cargo/bin:$PATH"
  cargo build -q --locked -p ck-release
  ck=target/debug/ck-release

  case "$RELEASES_DIR" in
    "$STATE_DIR"/*) ;;
    *) die "unsafe releases directory: $RELEASES_DIR" ;;
  esac
  rm -rf -- "$RELEASES_DIR"
  mkdir -p "$RELEASES_DIR"
  gh release download "$tag" -R "$REPO" -D "$RELEASES_DIR/$tag"

  for f in "$RELEASES_DIR/$tag"/*.tar.gz; do
    [[ -e $f ]] || continue
    gh attestation verify "$f" --repo "$REPO" \
      --signer-workflow "$REPO/.github/workflows/php-publish.yml"
    n=$((n + 1))
  done
  [[ $n -eq 2 ]] || die "expected 2 verified archives, got $n"

  expiry=$(expiry_90_days)
  "$ck" payload build --releases "$RELEASES_DIR" \
    --sequence "$EXPECTED_SEQUENCE" --expires "$expiry" \
    --out catalog/payload-v1.json
  jq -e --arg version "${tag#php-}" '
    .sequence == 1 and
    (.runtimes | length == 1) and
    (.runtimes[0] | .name == "php" and .version == $version and
      ([.artifacts[] | "\(.os)/\(.arch)"] | sort == ["linux/x86_64", "macos/aarch64"]))
  ' catalog/payload-v1.json >/dev/null
  "$ck" assets verify --payload catalog/payload-v1.json --releases "$RELEASES_DIR"

  set_field catalog_prepared 'true'
  set_field phase 'catalog-prepared'
  set_field expiry "$expiry"
  note "catalog payload ready: sequence $EXPECTED_SEQUENCE, expires $expiry"
  print_sign_block
}

do_catalog_run() {
  local prev id
  need gh jq cmp
  require_state
  require_head_unchanged >/dev/null
  [[ $(field catalog_prepared) == true ]] || die 'run catalog-prepare first'
  [[ -f catalog/catalog-v1.json ]] || die 'catalog/catalog-v1.json is missing; sign first'
  gh release download catalog-v1 -R "$REPO" -p catalog-v1.json -O - \
    | cmp -s - catalog/catalog-v1.json || die 'uploaded catalog differs from the local signed file'

  prev=$(field catalog_run_id)
  if [[ -z $prev ]]; then
    id=$(dispatch "$CATALOG_WORKFLOW" -f mode=run)
    set_field catalog_run_id "$id"
    set_field phase 'catalog-checked'
  else
    id=$prev
  fi
  wait_run "$id" 'Catalog check run'
  cat <<'EOF'
CATALOG CHECK GREEN.
Commit and push only these two files yourself:

  git add catalog/payload-v1.json catalog/catalog-v1.json
  git commit -m "chore(catalog): catalog sequence 1 (php 8.5.13-ck1)"
  git push

Then test from the app step by step.
EOF
}

cmd_auto() {
  local head phase
  require_state
  head=$(require_head_unchanged)
  phase=$(field phase)
  case "$phase" in
    new)
      do_prepare "$head"
      do_publish
      ;;
    clean-ready)
      do_publish
      [[ $(field phase) == publish-dispatched ]] && return 0
      do_catalog_prepare
      ;;
    publish-dispatched)
      do_publish
      [[ $(field phase) == published ]] || return 0
      do_catalog_prepare
      ;;
    published)
      do_catalog_prepare
      ;;
    catalog-prepared|catalog-checked)
      do_catalog_run
      ;;
    *)
      die "unknown phase: $phase"
      ;;
  esac
}

cmd_next() {
  local phase
  if [[ ! -f $STATE_FILE ]]; then
    note 'No state yet. Run: catalog/release.sh auto'
    return 0
  fi
  require_state
  phase=$(field phase)
  case "$phase" in
    new) note 'Next: catalog/release.sh auto' ;;
    clean-ready) note 'Next: catalog/release.sh auto (will dispatch publish)' ;;
    publish-dispatched) note 'Approve "release" in GitHub UI, then run: catalog/release.sh auto' ;;
    published) note 'Next: catalog/release.sh auto (prepares the catalog payload)' ;;
    catalog-prepared) note 'Sign and upload offline, then run: catalog/release.sh auto' ;;
    catalog-checked) note 'Commit/push the two catalog JSON files, then test from the app' ;;
    *) die "unknown phase: $phase" ;;
  esac
}

cmd_prepare() {
  do_prepare "${1:-}"
  note 'Next: catalog/release.sh publish'
}

cmd_publish() {
  do_publish
  if [[ $(field phase) == published ]]; then
    note 'Next: catalog/release.sh catalog-prepare'
  fi
}

usage() {
  cat <<'EOF'
usage: catalog/release.sh <auto|next|prepare|publish|catalog-prepare|catalog-run> [commit]

  auto             run the next safe phase; repeat after each human gate
  next             show the current phase and exact next action
  prepare [commit] CI, probe, clean build and artifact checks
  publish          dispatch or resume the protected publish
  catalog-prepare  download, attest, build and verify the catalog payload
  catalog-run      check the uploaded catalog and print the commit hint
EOF
}

main() {
  local command=${1:-next}
  shift || true
  case "$command" in
    auto) cmd_auto ;;
    next) cmd_next ;;
    prepare) cmd_prepare "$@" ;;
    publish) cmd_publish ;;
    catalog-prepare) do_catalog_prepare ;;
    catalog-run) do_catalog_run ;;
    -h|--help|help) usage ;;
    *) usage >&2; die "unknown command: $command" ;;
  esac
}

main "$@"
