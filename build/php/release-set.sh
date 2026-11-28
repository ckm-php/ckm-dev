#!/usr/bin/env bash
# Lays out and checks a release set: one folder per runtime release, named by
# its tag `<name>-<version>` (D25), as `ck-release payload build --releases`
# reads it and as the publish workflow uploads it.
#
#   release-set.sh [--allow-partial | --scope <lines> <oses>] <artifacts-dir> <out-dir>
#       <artifacts-dir> holds the "PHP build" run's `php-<os>-<X.Y>`
#       artifacts merged into one flat folder (actions/download-artifact
#       with `merge-multiple`, which also gives the same layout for one
#       artifact as for several): each archive
#       `php-<version>-<os>-<cpu>.tar.gz` (`.zip` on Windows) with its
#       descriptor `.json`, and nothing else. Writes <out-dir> (which must not
#       exist) with one folder per version holding every required platform's
#       archive and descriptor plus SHA256SUMS, then checks it as `--check`
#       does.
#
#   release-set.sh [--allow-partial | --scope <lines> <oses>] --check <dir>
#       Checks a release set: each folder holds exactly the archive and the
#       descriptor of each platform the manifest requires for that version and
#       SHA256SUMS; each descriptor names its archive, version and platform,
#       and its SHA256 and size equal the archive's; SHA256SUMS is byte for
#       byte what `sha256sum` writes for the archives in name order, and lists
#       nothing else.
#
# The expected tags, versions and platforms come from `plan-matrix.sh`, which
# reads the same manifest and lock as the build planner. `--scope` replays the
# clean dispatch's recorded `lines` and `oses`, so a first catalog can contain
# one line while later catalogs can contain more. The default `release` plan
# covers every non-CI-only line on its full platform set. `--allow-partial` uses
# `push` (all lines) and accepts a version with only some of its platforms, for
# test builds of one OS.

RS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$RS_DIR/lib/common.sh"

# Glob order is byte order, as `ck-release` and SHA256SUMS sort names.
export LC_ALL=C

RS_TAG_RE='^php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*$'
RS_STEM_RE='^(php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*)-(linux|macos|windows)-(x86_64|aarch64)$'
RS_PARTIAL=0
RS_REQUIRE_ALL=1
RS_POLICY_MODE=release
RS_POLICY_LABEL=release
RS_SCOPE_LINES=
RS_SCOPE_OSES=
RS_POLICY_JSON=
RS_EXPECTED=
RS_TMP=$(mktemp -d "${TMPDIR:-/tmp}/ck-release-set.XXXXXX")
trap 'rm -rf -- "$RS_TMP"' EXIT

rs_usage() {
  cat >&2 <<'EOF'
usage: release-set.sh [--allow-partial | --scope <lines> <oses>] <artifacts-dir> <out-dir>
       release-set.sh [--allow-partial | --scope <lines> <oses>] --check <dir>
EOF
  exit 2
}

# plan-matrix.sh is the one reader of line/platform eligibility. Keep its
# refusal text but give every planner failure this script's exit code.
rs_plan() {
  local err
  if ! RS_POLICY_JSON=$(bash "$RS_DIR/plan-matrix.sh" "$@" 2>"$RS_TMP/plan.err"); then
    err=$(tr '\n' ' ' <"$RS_TMP/plan.err")
    ck_die "release-set: the planner refused the policy: $err"
  fi
}

rs_load_policy() {
  if [ "$RS_POLICY_MODE" = workflow_dispatch ]; then
    rs_plan workflow_dispatch "$RS_SCOPE_LINES" "$RS_SCOPE_OSES" true
  else
    rs_plan "$RS_POLICY_MODE"
  fi
  RS_EXPECTED=$(jq -r '
    .php.include
    | group_by(.version)[]
    | "php-\(.[0].version)\t" + (map("\(.os)-\(.arch)") | unique | sort | join(" "))
  ' <<<"$RS_POLICY_JSON")
  [ -n "$RS_EXPECTED" ] || ck_die "release-set: the matrix planner has no expected releases"
}

# rs_platforms_of <tag>: the required platforms as <os>-<cpu>, byte order.
rs_platforms_of() {
  awk -F '\t' -v tag="$1" '$1 == tag {print $2; exit}' <<<"$RS_EXPECTED"
}

# rs_format_of <os>: the archive format the release tool uses for that OS.
rs_format_of() {
  case "$1" in
    windows) echo zip ;;
    *) echo tar.gz ;;
  esac
}

# rs_cpu_of <os>: the one CPU each OS is built for.
rs_cpu_of() {
  case "$1" in
    linux | windows) echo x86_64 ;;
    macos) echo aarch64 ;;
    *) ck_die "release-set: unknown OS $1" ;;
  esac
}

# rs_sha256 <file>: prints the file's SHA256 in lower-case hex.
rs_sha256() {
  local out
  if command -v sha256sum >/dev/null 2>&1; then
    out=$(sha256sum -b -- "$1")
  else
    out=$(shasum -a 256 -b -- "$1")
  fi
  printf '%s\n' "${out%% *}"
}

# rs_size <file>: prints the file's size in bytes.
rs_size() {
  local size
  size=$(wc -c <"$1")
  printf '%s\n' "$((size + 0))"
}

# rs_real_dir <path>: dies unless the path is a folder and not a link.
rs_real_dir() {
  if [ -L "$1" ] || [ ! -d "$1" ]; then
    ck_die "release-set: $1 is not a folder (links are refused)"
  fi
}

# rs_real_file <path>: dies unless the path is a regular file and not a link.
rs_real_file() {
  if [ -L "$1" ] || [ ! -f "$1" ]; then
    ck_die "release-set: $1 is not a regular file (links are refused)"
  fi
}

# rs_descriptor <descriptor> <archive> <tag> <platform>: checks both files
# against the tag, platform, format and measured bytes.
rs_descriptor() {
  local descriptor=$1 archive=$2 tag=$3 platform=$4
  local os=${platform%-*} cpu=${platform#*-} format stem suffix fields expected
  format=$(rs_format_of "$os")
  case "$format" in
    tar.gz) suffix=.tar.gz ;;
    zip) suffix=.zip ;;
  esac
  stem="$tag-$os-$cpu"
  rs_real_file "$descriptor"
  rs_real_file "$archive"
  [ "$(basename "$descriptor")" = "$stem.json" ] ||
    ck_die "release-set: $descriptor is not named $stem.json"
  [ "$(basename "$archive")" = "$stem$suffix" ] ||
    ck_die "release-set: $archive is not named $stem$suffix"
  # Every field, in a fixed order; a missing, extra or mistyped field fails.
  fields=$(jq -er '
    if type == "object"
       and (keys == ["cpu", "file", "format", "license", "name", "os", "schema", "sha256", "size", "version"])
       and .schema == 1
       and (.name | type == "string") and (.version | type == "string")
       and (.os | type == "string") and (.cpu | type == "string")
       and (.file | type == "string") and (.format | type == "string")
       and (.license | type == "string")
       and (.sha256 | type == "string" and test("^[0-9a-f]{64}$"))
       and (.size | type == "number" and . >= 1 and . == floor)
    then [.name, .version, .os, .cpu, .file, .format, .sha256, (.size | tostring)] | join(" ")
    else error("bad descriptor") end' "$descriptor" 2>/dev/null) ||
    ck_die "release-set: $descriptor is not a descriptor (fields or types are wrong)"
  expected="${tag%%-*} ${tag#*-} $os $cpu $stem$suffix $format $(rs_sha256 "$archive") $(rs_size "$archive")"
  [ "$fields" = "$expected" ] ||
    ck_die "release-set: $descriptor does not describe $archive (name, version, platform, file, format, SHA256 or size differ)"
}

# rs_check <dir>: checks a release set (see the header).
rs_check() {
  local dir=$1 folder tag entry platforms platform os cpu stem suffix found sums count want
  local folders=0
  rs_real_dir "$dir"
  for folder in "$dir"/* "$dir"/.[!.]* "$dir"/..?*; do
    [ -e "$folder" ] || [ -L "$folder" ] || continue
    tag=$(basename "$folder")
    rs_real_dir "$folder"
    [[ $tag =~ $RS_TAG_RE ]] || ck_die "release-set: $folder is not named php-<X.Y.Z>-ck<N>"
    platforms=$(rs_platforms_of "$tag")
    [ -n "$platforms" ] || ck_die "release-set: $tag is not an expected release in the $RS_POLICY_LABEL plan"

    found=0
    sums="$RS_TMP/SHA256SUMS"
    : >"$sums"
    for platform in $platforms; do
      os=${platform%-*}
      cpu=${platform#*-}
      format=$(rs_format_of "$os")
      case "$format" in
        tar.gz) suffix=.tar.gz ;;
        zip) suffix=.zip ;;
      esac
      stem="$tag-$os-$cpu"
      if [ -e "$folder/$stem$suffix" ] || [ -e "$folder/$stem.json" ] ||
        [ -L "$folder/$stem$suffix" ] || [ -L "$folder/$stem.json" ]; then
        rs_descriptor "$folder/$stem.json" "$folder/$stem$suffix" "$tag" "$platform"
        printf '%s  %s\n' "$(rs_sha256 "$folder/$stem$suffix")" "$stem$suffix" >>"$sums"
        found=$((found + 1))
      elif [ "$RS_PARTIAL" -eq 0 ]; then
        ck_die "release-set: $folder has no $platform archive; this version requires $platforms"
      fi
    done
    [ "$found" -gt 0 ] || ck_die "release-set: $folder holds no archive"

    # Nothing besides the pairs found and SHA256SUMS.
    count=0
    for entry in "$folder"/* "$folder"/.[!.]* "$folder"/..?*; do
      [ -e "$entry" ] || [ -L "$entry" ] || continue
      count=$((count + 1))
    done
    want=$((found * 2 + 1))
    if [ "$count" -ne "$want" ]; then
      ck_die "release-set: $folder holds $count entries, expected $want (the archives, their descriptors and SHA256SUMS only)"
    fi
    if [ -L "$folder/SHA256SUMS" ] || [ ! -f "$folder/SHA256SUMS" ] ||
      ! cmp -s "$sums" "$folder/SHA256SUMS"; then
      ck_die "release-set: $folder/SHA256SUMS is not exactly the sha256sum lines of its archives, in name order"
    fi
    folders=$((folders + 1))
  done
  [ "$folders" -gt 0 ] || ck_die "release-set: $dir holds no release folder"
  if [ "$RS_REQUIRE_ALL" -eq 1 ]; then
    while IFS=$'\t' read -r tag _; do
      [ -d "$dir/$tag" ] || ck_die "release-set: $dir has no $tag release folder (expected in the $RS_POLICY_LABEL plan)"
    done <<<"$RS_EXPECTED"
  fi
}

# rs_build <artifacts-dir> <out-dir>: lays out the artifacts (see the header).
rs_build() {
  local artifacts=$1 out=$2 entry name stem tag os cpu format suffix archive matched
  rs_real_dir "$artifacts"
  if [ -e "$out" ] || [ -L "$out" ]; then
    ck_die "release-set: $out already exists"
  fi
  mkdir "$out"

  # Every entry is an archive or a descriptor; each archive has its
  # descriptor, and each descriptor its archive.
  for entry in "$artifacts"/* "$artifacts"/.[!.]* "$artifacts"/..?*; do
    [ -e "$entry" ] || [ -L "$entry" ] || continue
    name=$(basename "$entry")
    case "$name" in
      *.tar.gz) stem=${name%.tar.gz} ;;
      *.zip) stem=${name%.zip} ;;
      *.json) stem=${name%.json} ;;
      *) ck_die "release-set: $entry is neither an archive nor a descriptor" ;;
    esac
    [[ $stem =~ $RS_STEM_RE ]] ||
      ck_die "release-set: $entry is not named php-<X.Y.Z>-ck<N>-<os>-<cpu>"
    os=${BASH_REMATCH[2]}
    format=$(rs_format_of "$os")
    case "$format" in
      tar.gz) suffix=.tar.gz ;;
      zip) suffix=.zip ;;
    esac
    [ -e "$artifacts/$stem$suffix" ] || ck_die "release-set: $entry has no archive $stem$suffix"
    [ -e "$artifacts/$stem.json" ] || ck_die "release-set: $entry has no descriptor $stem.json"
  done

  matched=0
  for archive in "$artifacts"/*.tar.gz "$artifacts"/*.zip; do
    [ -e "$archive" ] || [ -L "$archive" ] || continue
    name=$(basename "$archive")
    case "$name" in
      *.tar.gz) stem=${name%.tar.gz} ;;
      *.zip) stem=${name%.zip} ;;
    esac
    [[ $stem =~ $RS_STEM_RE ]] || ck_die "release-set: $archive is not named php-<X.Y.Z>-ck<N>-<os>-<cpu>"
    tag=${BASH_REMATCH[1]}
    os=${BASH_REMATCH[2]}
    cpu=${BASH_REMATCH[3]}
    [ "$cpu" = "$(rs_cpu_of "$os")" ] ||
      ck_die "release-set: $archive is not for a supported platform"
    [ -n "$(rs_platforms_of "$tag")" ] ||
      ck_die "release-set: $tag is not an expected release in the $RS_POLICY_LABEL plan"
    format=$(rs_format_of "$os")
    case "$format" in
      tar.gz) suffix=.tar.gz ;;
      zip) suffix=.zip ;;
    esac
    rs_descriptor "$artifacts/$stem.json" "$archive" "$tag" "$os-$cpu"
    mkdir -p "$out/$tag"
    cp -- "$archive" "$out/$tag/$stem$suffix"
    cp -- "$artifacts/$stem.json" "$out/$tag/$stem.json"
    matched=$((matched + 1))
  done
  [ "$matched" -gt 0 ] || ck_die "release-set: $artifacts holds no archive"

  # SHA256SUMS lists the archives in name order, as sha256sum writes them.
  for tag in "$out"/*; do
    : >"$tag/SHA256SUMS"
    : >"$RS_TMP/names"
    for archive in "$tag"/*.tar.gz "$tag"/*.zip; do
      [ -e "$archive" ] || [ -L "$archive" ] || continue
      printf '%s\n' "$(basename "$archive")" >>"$RS_TMP/names"
    done
    while IFS= read -r name; do
      printf '%s  %s\n' "$(rs_sha256 "$tag/$name")" "$name" >>"$tag/SHA256SUMS"
    done < <(sort "$RS_TMP/names")
  done

  rs_check "$out"
}

rs_main() {
  ck_require_cmd jq cmp
  case "${1:-}" in
    --allow-partial)
      RS_PARTIAL=1
      RS_REQUIRE_ALL=0
      RS_POLICY_MODE=push
      RS_POLICY_LABEL=push
      shift
      ;;
    --scope)
      [ "$#" -ge 3 ] || rs_usage
      [ "$RS_PARTIAL" -eq 0 ] || rs_usage
      RS_SCOPE_LINES=$2
      RS_SCOPE_OSES=$3
      RS_POLICY_MODE=workflow_dispatch
      RS_POLICY_LABEL="clean $RS_SCOPE_LINES on $RS_SCOPE_OSES"
      shift 3
      ;;
  esac
  rs_load_policy
  case "${1:-}" in
    --check)
      [ "$#" -eq 2 ] || rs_usage
      rs_check "$2"
      ;;
    -*) rs_usage ;;
    *)
      [ "$#" -eq 2 ] || rs_usage
      rs_build "$1" "$2"
      ;;
  esac
  printf 'release-set: ok\n'
}

rs_main "$@"
