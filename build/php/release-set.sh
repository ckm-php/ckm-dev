#!/usr/bin/env bash
# Lays out and checks a release set: one folder per runtime release, named by
# its tag `<name>-<version>` (D25), as `ck-release payload build --releases`
# reads it and as the publish workflow uploads it.
#
#   release-set.sh [--allow-partial] <artifacts-dir> <out-dir>
#       <artifacts-dir> holds the "PHP build" run's `php-<os>-<X.Y>`
#       artifacts merged into one flat folder (actions/download-artifact
#       with `merge-multiple`, which also gives the same layout for one
#       artifact as for several): each archive
#       `php-<version>-<os>-<cpu>.tar.gz` with its descriptor `.json`, and
#       nothing else. Writes <out-dir> (which must not exist) with one folder
#       per version holding every platform's archive and descriptor plus
#       SHA256SUMS, then checks it as `--check` does.
#
#   release-set.sh [--allow-partial] --check <dir>
#       Checks a release set: each folder holds exactly the archive and the
#       descriptor of each required platform and SHA256SUMS; each descriptor
#       names its archive, version and platform, and its SHA256 and size
#       equal the archive's; SHA256SUMS is byte for byte what `sha256sum`
#       writes for the archives in name order, and lists nothing else.
#
# Every version needs both v1 platforms, linux-x86_64 and macos-aarch64.
# `--allow-partial` accepts a version with only some of them, for test
# builds of one OS; a publishable set never uses it.

RS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$RS_DIR/lib/common.sh"

# Glob order is byte order, as `ck-release` and SHA256SUMS sort names.
export LC_ALL=C

# The v1 platforms (D17) that build PHP, as <os>-<cpu>, in byte order: the
# order of their archives in SHA256SUMS.
RS_PLATFORMS="linux-x86_64 macos-aarch64"
RS_TAG_RE='^php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*$'
RS_STEM_RE='^(php-[0-9]+\.[0-9]+\.[0-9]+-ck[1-9][0-9]*)-(linux|macos)-(x86_64|aarch64)$'
RS_PARTIAL=0
RS_TMP=$(mktemp -d "${TMPDIR:-/tmp}/ck-release-set.XXXXXX")
trap 'rm -rf -- "$RS_TMP"' EXIT

rs_usage() {
  cat >&2 <<'EOF'
usage: release-set.sh [--allow-partial] <artifacts-dir> <out-dir>
       release-set.sh [--allow-partial] --check <dir>
EOF
  exit 2
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

# rs_cpu_of <os>: the one CPU each OS is built for.
rs_cpu_of() {
  case "$1" in
    linux) echo x86_64 ;;
    macos) echo aarch64 ;;
    *) ck_die "release-set: unknown OS $1" ;;
  esac
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

# rs_descriptor <descriptor> <archive> <name-version> <os>: checks the
# descriptor against its archive and the version and OS it must describe.
rs_descriptor() {
  local descriptor=$1 archive=$2 tag=$3 os=$4 cpu stem fields expected
  cpu=$(rs_cpu_of "$os")
  stem="$tag-$os-$cpu"
  rs_real_file "$descriptor"
  rs_real_file "$archive"
  [ "$(basename "$descriptor")" = "$stem.json" ] ||
    ck_die "release-set: $descriptor is not named $stem.json"
  [ "$(basename "$archive")" = "$stem.tar.gz" ] ||
    ck_die "release-set: $archive is not named $stem.tar.gz"
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
  expected="${tag%%-*} ${tag#*-} $os $cpu $stem.tar.gz tar.gz $(rs_sha256 "$archive") $(rs_size "$archive")"
  [ "$fields" = "$expected" ] ||
    ck_die "release-set: $descriptor does not describe $archive (name, version, platform, file, format, SHA256 or size differ)"
}

# rs_check <dir>: checks a release set (see the header).
rs_check() {
  local dir=$1 folder tag entry version platform os cpu stem found sums count want
  local folders=0
  rs_real_dir "$dir"
  for folder in "$dir"/* "$dir"/.[!.]* "$dir"/..?*; do
    [ -e "$folder" ] || [ -L "$folder" ] || continue
    tag=$(basename "$folder")
    rs_real_dir "$folder"
    [[ $tag =~ $RS_TAG_RE ]] || ck_die "release-set: $folder is not named php-<X.Y.Z>-ck<N>"
    version=${tag#php-}

    found=0
    sums="$RS_TMP/SHA256SUMS"
    : >"$sums"
    for platform in $RS_PLATFORMS; do
      os=${platform%-*}
      cpu=${platform#*-}
      stem="$tag-$os-$cpu"
      if [ -e "$folder/$stem.tar.gz" ] || [ -e "$folder/$stem.json" ] ||
        [ -L "$folder/$stem.tar.gz" ] || [ -L "$folder/$stem.json" ]; then
        rs_descriptor "$folder/$stem.json" "$folder/$stem.tar.gz" "php-$version" "$os"
        printf '%s  %s\n' "$(rs_sha256 "$folder/$stem.tar.gz")" "$stem.tar.gz" >>"$sums"
        found=$((found + 1))
      elif [ "$RS_PARTIAL" -eq 0 ]; then
        ck_die "release-set: $folder has no $platform archive; every version needs $RS_PLATFORMS"
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
}

# rs_build <artifacts-dir> <out-dir>: lays out the artifacts (see the header).
rs_build() {
  local artifacts=$1 out=$2 entry name stem tag os cpu archive matched
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
      *.json) stem=${name%.json} ;;
      *) ck_die "release-set: $entry is neither an archive nor a descriptor" ;;
    esac
    [[ $stem =~ $RS_STEM_RE ]] ||
      ck_die "release-set: $entry is not named php-<X.Y.Z>-ck<N>-<os>-<cpu>"
    [ -e "$artifacts/$stem.tar.gz" ] || ck_die "release-set: $entry has no archive $stem.tar.gz"
    [ -e "$artifacts/$stem.json" ] || ck_die "release-set: $entry has no descriptor $stem.json"
  done

  matched=0
  for archive in "$artifacts"/*.tar.gz; do
    [ -e "$archive" ] || [ -L "$archive" ] || continue
    stem=$(basename "$archive" .tar.gz)
    [[ $stem =~ $RS_STEM_RE ]] || ck_die "release-set: $archive is not named php-<X.Y.Z>-ck<N>-<os>-<cpu>"
    tag=${BASH_REMATCH[1]}
    os=${BASH_REMATCH[2]}
    cpu=${BASH_REMATCH[3]}
    [ "$cpu" = "$(rs_cpu_of "$os")" ] || ck_die "release-set: $archive is not for a v1 platform ($RS_PLATFORMS)"
    rs_descriptor "$artifacts/$stem.json" "$archive" "$tag" "$os"
    mkdir -p "$out/$tag"
    cp -- "$archive" "$out/$tag/$stem.tar.gz"
    cp -- "$artifacts/$stem.json" "$out/$tag/$stem.json"
    matched=$((matched + 1))
  done
  [ "$matched" -gt 0 ] || ck_die "release-set: $artifacts holds no archive"

  # SHA256SUMS lists the archives in name order, as sha256sum writes them.
  for tag in "$out"/*; do
    : >"$tag/SHA256SUMS"
    for archive in "$tag"/*.tar.gz; do
      printf '%s  %s\n' "$(rs_sha256 "$archive")" "$(basename "$archive")" >>"$tag/SHA256SUMS"
    done
  done

  rs_check "$out"
}

rs_main() {
  ck_require_cmd jq cmp
  if [ "${1:-}" = --allow-partial ]; then
    RS_PARTIAL=1
    shift
  fi
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
