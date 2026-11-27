#!/usr/bin/env bash
# GitHub can assign an untagged-* name to a draft. Use only the ID returned
# by creation, and set the intended tag explicitly when publishing.

RD_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$RD_DIR/lib/common.sh"
# shellcheck source=lib/fetch.sh
. "$RD_DIR/lib/fetch.sh"
ck_require_cmd gh jq

[[ ${GITHUB_REPOSITORY:-} =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  ck_die "release drafts: invalid repository"
[[ ${COMMIT:-} =~ ^[0-9a-f]{40}$ && $COMMIT = "${GITHUB_SHA:-}" && ${GITHUB_REF:-} = refs/heads/main ]] ||
  ck_die "release drafts: not on main at the checked commit"
[[ ${GITHUB_RUN_ID:-} =~ ^[1-9][0-9]*$ && ${GITHUB_RUN_ATTEMPT:-} =~ ^[1-9][0-9]*$ ]] ||
  ck_die "release drafts: invalid workflow run"
[[ ${TAGS:-} =~ ^php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*(\ php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*)*$ ]] ||
  ck_die "release drafts: invalid release tag set"

rd_repo="repos/$GITHUB_REPOSITORY"
rd_state="${RUNNER_TEMP:?}/release-ids"
rd_set="$RUNNER_TEMP/release-set"
rd_digits='^[1-9][0-9]*$'

rd_marker() {
  printf '<!-- ck-dev-publish:%s/%s:%s -->\n' "$GITHUB_RUN_ID" "$GITHUB_RUN_ATTEMPT" "$1"
}

rd_id() {
  local id
  id=$(cat "$rd_state/$1")
  [[ $id =~ $rd_digits ]] || ck_die "release drafts: invalid recorded ID for $1"
  printf '%s\n' "$id"
}

rd_matches() {
  local tag=$1 id=$2 marker
  marker=$(rd_marker "$tag")
  jq -e --arg id "$id" --arg tag "$tag" --arg commit "$COMMIT" --arg marker "$marker" '
    (.id | tostring) == $id and .draft == true and .target_commitish == $commit and
    (.tag_name == $tag or .tag_name == "" or .tag_name == null or
      ((.tag_name // "") | test("^untagged-[0-9a-f]+$"))) and
    ((.body | split("\n")) | index($marker) != null)
  ' >/dev/null
}

rd_no_tag() {
  gh api "$rd_repo/git/matching-refs/tags/$1" |
    jq -e --arg ref "refs/tags/$1" 'type == "array" and all(.[]; .ref != $ref)' >/dev/null ||
    ck_die "release drafts: cannot confirm tag $1 is unused"
}

rd_preflight() {
  local releases tag
  releases=$(gh api "$rd_repo/releases?per_page=100" --paginate \
    --jq '.[] | [.tag_name, (if .draft then .name else "" end)] | @tsv')
  for tag in $TAGS; do
    # A leftover immutable-release draft may list its temporary tag, not
    # its intended one. Its release title is still the one we gave it.
    if awk -F '\t' -v tag="$tag" -v title="PHP ${tag#php-}" \
      '$1 == tag || $2 == title { found=1 } END { exit !found }' <<<"$releases"; then
      ck_die "release drafts: a release or draft for $tag already exists"
    fi
    rd_no_tag "$tag"
  done
}

rd_create() {
  local tag version notes reply id
  mkdir -m 700 "$rd_state"
  for tag in $TAGS; do
    version=${tag#php-}
    notes="$RUNNER_TEMP/notes-$tag.md"
    {
      echo "PHP $version for Linux x86_64 and macOS on Apple Silicon, built by CK Dev's CI."
      echo
      echo "- Build run: $GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$RUN_ID"
      echo "- Commit: $COMMIT"
      echo "- Attestation: $ATTESTATION_URL"
      echo
      echo "Check an archive with \`gh attestation verify <file> --repo $GITHUB_REPOSITORY --signer-workflow $GITHUB_REPOSITORY/.github/workflows/php-publish.yml\` and against \`SHA256SUMS\`."
      echo
      rd_marker "$tag"
    } >"$notes"
    reply=$(gh api "$rd_repo/releases" --method POST -F draft=true -f make_latest=false \
      -f "tag_name=$tag" -f "target_commitish=$COMMIT" -f "name=PHP $version" -F "body=@$notes")
    id=$(jq -er '.id | select(type == "number")' <<<"$reply")
    [[ $id =~ $rd_digits ]] || ck_die "release drafts: invalid creation response ID for $tag"
    printf '%s\n' "$id" >"$rd_state/$tag"
    rd_matches "$tag" "$id" <<<"$reply" || ck_die "release drafts: unexpected creation response for $tag (ID $id)"
    echo "Draft $id for $tag"
  done
}

rd_read() {
  local tag=$1 id=$2 reply
  reply=$(gh api "$rd_repo/releases/$id") || return 1
  rd_matches "$tag" "$id" <<<"$reply" || return 1
  printf '%s\n' "$reply"
}

rd_upload() {
  local tag id file name
  for tag in $TAGS; do
    id=$(rd_id "$tag")
    rd_read "$tag" "$id" >/dev/null || ck_die "release drafts: cannot verify draft $id for $tag"
    for file in "$rd_set/$tag/"*; do
      name=${file##*/}
      [[ $name =~ ^[A-Za-z0-9._-]+$ && -f $file ]] || ck_die "release drafts: invalid asset for $tag"
      gh api "https://uploads.github.com/$rd_repo/releases/$id/assets?name=$name" --method POST \
        -H 'Content-Type: application/octet-stream' --input "$file" >/dev/null
    done
  done
}

rd_asset_list() {
  jq -ce '
    .assets | select(type == "array" and length > 0) |
    map({id, name, state, digest}) | sort_by(.id)
  '
}

rd_verify_assets() {
  local tag id reply uploaded snapshot files asset_id name state digest hash
  for tag in $TAGS; do
    id=$(rd_id "$tag")
    reply=$(rd_read "$tag" "$id") || ck_die "release drafts: cannot verify draft $id for $tag"
    snapshot=$(rd_asset_list <<<"$reply")
    files=$(jq -er '
      .[] | [.id, .name, .state, .digest] |
      select(.[0] | type == "number") |
      select(.[1] | type == "string") | @tsv
    ' <<<"$snapshot")
    # Keep a rejected item from being silently dropped by the query.
    [[ $(jq -r 'length' <<<"$snapshot") = "$(wc -l <<<"$files" | tr -d ' ')" ]] ||
      ck_die "release drafts: invalid asset list for $tag"
    uploaded="$RUNNER_TEMP/uploaded/$tag"
    mkdir -p "$uploaded"
    while IFS=$'\t' read -r asset_id name state digest; do
      [[ $asset_id =~ $rd_digits && $name =~ ^[A-Za-z0-9._-]+$ && $state = uploaded &&
        -f $rd_set/$tag/$name && ! -e $uploaded/$name ]] ||
        ck_die "release drafts: unexpected or duplicate asset for $tag"
      gh api "$rd_repo/releases/assets/$asset_id" -H 'Accept: application/octet-stream' >"$uploaded/$name"
      hash=$(ck_sha256 "$uploaded/$name")
      [[ $digest = "sha256:$hash" ]] ||
        ck_die "release drafts: asset digest does not match for $tag/$name"
    done <<<"$files"
    diff -r "$rd_set/$tag" "$uploaded"
    printf '%s\n' "$snapshot" >"$rd_state/$tag.assets"
  done
}

rd_publish() {
  local tag id reply checked actual sha
  for tag in $TAGS; do
    id=$(rd_id "$tag")
    checked=$(cat "$rd_state/$tag.assets")
    reply=$(rd_read "$tag" "$id") || ck_die "release drafts: cannot verify draft $id for $tag"
    actual=$(rd_asset_list <<<"$reply")
    [[ $checked = "$actual" ]] || ck_die "release drafts: assets changed since verification for $tag"
    rd_no_tag "$tag"
    reply=$(gh api "$rd_repo/releases/$id" --method PATCH -F draft=false -f make_latest=false \
      -f "tag_name=$tag" -f "target_commitish=$COMMIT")
    jq -e --arg id "$id" --arg tag "$tag" '
      (.id | tostring) == $id and .draft == false and .tag_name == $tag
    ' <<<"$reply" >/dev/null || ck_die "release drafts: unexpected publish response for $tag (ID $id)"
    sha=$(gh api "$rd_repo/git/ref/tags/$tag" | jq -er '.object.sha')
    [[ $sha = "$COMMIT" ]] || ck_die "release drafts: published tag $tag points at $sha, not $COMMIT"
    echo "Published $tag (release $id)"
  done
}

rd_report() {
  local tag id reply status=0
  for tag in $TAGS; do
    [ -f "$rd_state/$tag" ] || continue
    if ! id=$(rd_id "$tag") || ! reply=$(gh api "$rd_repo/releases/$id"); then
      ck_log "cannot confirm release for $tag; check GitHub Releases manually"
      status=1
    elif jq -e --arg id "$id" '(.id | tostring) == $id and .draft == false' <<<"$reply" >/dev/null; then
      echo "Keeping published release $id for $tag"
    elif rd_matches "$tag" "$id" <<<"$reply"; then
      echo "Draft $id for $tag remains; remove it by hand only if GitHub still labels it Draft."
    else
      ck_log "cannot confirm state of release $id for $tag; check GitHub Releases manually"
      status=1
    fi
  done
  return "$status"
}

case "${1:-}" in
  preflight) rd_preflight ;;
  create) rd_create ;;
  upload) rd_upload ;;
  verify-assets) rd_verify_assets ;;
  publish) rd_publish ;;
  report) rd_report ;;
  *) ck_die "usage: release-drafts.sh preflight|create|upload|verify-assets|publish|report" ;;
esac
