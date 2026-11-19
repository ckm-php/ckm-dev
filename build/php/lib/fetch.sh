# shellcheck shell=bash
# Pinned downloads and safe unpacking for the PHP build. Source it after
# lib/common.sh. The function names are frozen: see build/php/README.md.
#
# Every function checks each step itself and calls ck_die on failure, so it
# behaves the same when a caller runs it under `if`, `||` or `!`, where bash
# turns `set -e` off. Nothing here sets a trap, so a caller's traps survive.

if [ -n "${CK_FETCH_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${CK_COMMON_SH_LOADED:-}" ]; then
  echo "ck-build: error: source lib/common.sh before lib/fetch.sh" >&2
  exit 1
fi
CK_FETCH_SH_LOADED=1

# The pins of the current build step, set by ck_pins_load.
CK_PINS=

# ck_pins_load <lock export args...>: runs `ck-release lock export` with the
# arguments and keeps its lines in CK_PINS. Dies when the export fails or
# prints nothing.
ck_pins_load() {
  local pins
  pins=$(ck_release lock export "$@") || ck_die "ck-release lock export $* failed"
  [ -n "$pins" ] || ck_die "ck-release lock export $* printed no pins"
  CK_PINS=$pins
}

# ck_pin <name>: prints the one CK_PINS line for <name>, eight tab-separated
# fields: name version url sha256 size license signature file. Dies when the
# name is missing, appears twice, or its line is malformed.
ck_pin() {
  local want=${1:-} found='' count=0 line name rest tab=$'\t'
  [ -n "$want" ] || ck_die "ck_pin: no name"
  [ -n "$CK_PINS" ] || ck_die "ck_pin $want: no pins loaded (call ck_pins_load first)"
  while IFS= read -r line; do
    name=${line%%"$tab"*}
    if [ "$name" = "$want" ]; then
      count=$((count + 1))
      found=$line
    fi
  done <<<"$CK_PINS"
  [ "$count" -gt 0 ] || ck_die "no pin for $want in the loaded pins"
  [ "$count" -eq 1 ] || ck_die "$count pins for $want in the loaded pins; expected one"
  # Exactly eight non-empty fields.
  rest=$found
  count=0
  while :; do
    count=$((count + 1))
    case "$rest" in
      *"$tab"*)
        [ -n "${rest%%"$tab"*}" ] || ck_die "pin line for $want has an empty field"
        rest=${rest#*"$tab"}
        ;;
      *)
        [ -n "$rest" ] || ck_die "pin line for $want has an empty field"
        break
        ;;
    esac
  done
  [ "$count" -eq 8 ] || ck_die "pin line for $want has $count fields; expected 8"
  printf '%s\n' "$found"
}

# ck_sha256 <file>: prints the file's SHA256 in lower-case hex.
ck_sha256() {
  local file=${1:-} os out
  [ -f "$file" ] || ck_die "ck_sha256: not a file: $file"
  os=$(ck_os) || exit 1
  case "$os" in
    linux) out=$(sha256sum <"$file") || ck_die "sha256sum failed on $file" ;;
    macos) out=$(shasum -a 256 <"$file") || ck_die "shasum failed on $file" ;;
  esac
  out=${out%% *}
  ck_is_sha256 "$out" || ck_die "unexpected hash output for $file: $out"
  printf '%s\n' "$out"
}

# ck_size <file>: prints the file's size in bytes.
ck_size() {
  local file=${1:-} out
  [ -f "$file" ] || ck_die "ck_size: not a file: $file"
  out=$(wc -c <"$file") || ck_die "wc failed on $file"
  out=$(printf '%s' "$out" | tr -d ' \t')
  case "$out" in
    '' | *[!0-9]*) ck_die "unexpected size output for $file: $out" ;;
  esac
  printf '%s\n' "$out"
}

# ck_is_sha256 <text>: true when text is 64 lower-case hex characters.
ck_is_sha256() {
  local re='^[0-9a-f]{64}$'
  [[ ${1:-} =~ $re ]]
}

# ck_download <url> <out> <max-bytes>: fetches an HTTPS URL into <out> with
# curl, HTTPS on every redirect hop, TLS 1.2 or later, and no ~/.curlrc.
# Removes <out> and dies on failure.
ck_download() {
  local url=${1:-} out=${2:-} max=${3:-}
  case "$url" in
    https://*) ;;
    *) ck_die "refusing a URL that is not HTTPS: $url" ;;
  esac
  case "$max" in
    '' | *[!0-9]*) ck_die "ck_download: not a byte count: $max" ;;
  esac
  [ -n "$out" ] || ck_die "ck_download: no output file"
  if ! curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
    --retry 3 --retry-delay 2 --connect-timeout 30 \
    --max-filesize "$max" -o "$out" "$url"; then
    rm -f -- "$out"
    ck_die "download failed: $url"
  fi
  [ -f "$out" ] || ck_die "download wrote no file: $url"
}

# ck_verify_file <file> <sha256> <size>: dies, after deleting the file,
# unless it has exactly that size and SHA256.
ck_verify_file() {
  local file=${1:-} sha=${2:-} size=${3:-} actual
  actual=$(ck_size "$file") || exit 1
  if [ "$actual" != "$size" ]; then
    rm -f -- "$file"
    ck_die "size mismatch for $file: expected $size bytes, got $actual; the file was deleted"
  fi
  actual=$(ck_sha256 "$file") || exit 1
  if [ "$actual" != "$sha" ]; then
    rm -f -- "$file"
    ck_die "SHA256 mismatch for $file: expected $sha, got $actual; the file was deleted"
  fi
}

# ck_check_pin_fields <name> <version> <url> <sha256> <size> <file>: dies
# unless the fields are safe to build a cache path and download from.
ck_check_pin_fields() {
  local name=${1:-} version=${2:-} url=${3:-} sha=${4:-} size=${5:-} file=${6:-}
  local name_re='^[a-z0-9][a-z0-9-]{0,31}$'
  local version_re='^[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}$'
  local file_re='^[A-Za-z0-9][A-Za-z0-9._+-]{0,127}$'
  [[ $name =~ $name_re ]] || ck_die "not a pin name: $name"
  [[ $version =~ $version_re ]] || ck_die "not a pin version: $version"
  case "$version" in *..*) ck_die "not a pin version: $version" ;; esac
  [[ $file =~ $file_re ]] || ck_die "not a safe file name: $file"
  case "$file" in *..*) ck_die "not a safe file name: $file" ;; esac
  case "$url" in
    https://*/"$file") ;;
    *) ck_die "the URL is not HTTPS or does not end in /$file: $url" ;;
  esac
  ck_is_sha256 "$sha" || ck_die "not a lower-case SHA256: $sha"
  case "$size" in
    '' | 0* | *[!0-9]*) ck_die "not a size in bytes: $size" ;;
  esac
}

# ck_fetch <name> <version> <url> <sha256> <size> <file>: makes sure
# <cache>/<name>-<version>/<file> exists and matches the pin, and prints its
# path. A cached file is checked again every time; a mismatch deletes it and
# dies, and is never fixed by downloading again. A new download goes to a
# temporary file in the same folder and is renamed only once it matches.
ck_fetch() {
  [ "$#" -eq 6 ] || ck_die "ck_fetch: expected 6 arguments, got $#"
  local name=$1 version=$2 url=$3 sha=$4 size=$5 file=$6 cache dir path temp
  ck_check_pin_fields "$name" "$version" "$url" "$sha" "$size" "$file"
  cache=$(ck_cache_dir) || exit 1
  dir="$cache/$name-$version"
  path="$dir/$file"
  if [ -L "$path" ] || { [ -e "$path" ] && [ ! -f "$path" ]; }; then
    ck_die "not a regular file in the download cache: $path"
  fi
  if [ -f "$path" ]; then
    ck_verify_file "$path" "$sha" "$size"
    printf '%s\n' "$path"
    return 0
  fi
  mkdir -p -- "$dir" || ck_die "cannot create $dir"
  temp=$(mktemp "$dir/.$file.XXXXXX") || ck_die "cannot create a temporary file in $dir"
  ck_log "downloading $url"
  ck_download "$url" "$temp" "$size"
  ck_verify_file "$temp" "$sha" "$size"
  if ! mv -f -- "$temp" "$path"; then
    rm -f -- "$temp"
    ck_die "cannot move the download into $path"
  fi
  printf '%s\n' "$path"
}

# ck_unpack <archive> <dest>: unpacks a .tar.gz, .tgz, .tar.xz or .tar.bz2
# whose members all sit under one top-level folder, and makes that folder
# <dest>. <dest> must not exist. Refuses members with an absolute path or a
# `..` part (checked in the listing before anything is written), links that
# are absolute, dangling or resolve outside the tree, and special files.
# Unpacks into a temporary folder beside <dest> and renames it into place, so
# an existing <dest> is always complete.
ck_unpack() {
  [ "$#" -eq 2 ] || ck_die "ck_unpack: expected 2 arguments, got $#"
  local archive=$1 dest=$2 parent listing member top='' staging root real link target
  case "$archive" in
    *.tar.gz | *.tgz | *.tar.xz | *.tar.bz2) ;;
    *) ck_die "not a supported archive (.tar.gz, .tgz, .tar.xz, .tar.bz2): $archive" ;;
  esac
  if [ -L "$archive" ] || [ ! -f "$archive" ]; then
    ck_die "not a regular file: $archive"
  fi
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    ck_die "already exists: $dest"
  fi
  parent=$(dirname -- "$dest") || ck_die "bad destination: $dest"
  [ -d "$parent" ] || ck_die "the parent folder does not exist: $parent"

  listing=$(tar -tf "$archive") || ck_die "cannot list $archive"
  [ -n "$listing" ] || ck_die "empty archive: $archive"
  while IFS= read -r member; do
    [ -n "$member" ] || continue
    case "$member" in
      /*) ck_die "refusing $archive: absolute member path: $member" ;;
    esac
    case "/$member/" in
      */../*) ck_die "refusing $archive: member path with '..': $member" ;;
    esac
    case "${member%%/*}" in
      '' | . | ..) ck_die "refusing $archive: member outside one top-level folder: $member" ;;
    esac
    if [ -z "$top" ]; then
      top=${member%%/*}
    elif [ "${member%%/*}" != "$top" ]; then
      ck_die "refusing $archive: more than one top-level entry ($top, ${member%%/*})"
    fi
  done <<<"$listing"
  [ -n "$top" ] || ck_die "refusing $archive: no members"

  staging=$(mktemp -d "$parent/.ck-unpack.XXXXXX") || ck_die "cannot create a temporary folder in $parent"
  mkdir "$staging/x" || ck_unpack_fail "$staging" "cannot create $staging/x"
  if ! tar -x --no-same-owner -f "$archive" -C "$staging/x"; then
    ck_unpack_fail "$staging" "cannot unpack $archive"
  fi
  root="$staging/x/$top"
  if [ -L "$root" ] || [ ! -d "$root" ]; then
    ck_unpack_fail "$staging" "refusing $archive: the top-level entry $top is not a folder"
  fi
  if [ -n "$(find "$root" \( -type b -o -type c -o -type p -o -type s \) -print)" ]; then
    ck_unpack_fail "$staging" "refusing $archive: it holds a device, pipe or socket"
  fi
  real=$(cd -P "$root" && pwd -P) || ck_unpack_fail "$staging" "cannot resolve $root"
  find "$root" -type l -print0 >"$staging/links" || ck_unpack_fail "$staging" "cannot list links in $root"
  while IFS= read -r -d '' link; do
    target=$(readlink "$link") || ck_unpack_fail "$staging" "cannot read link $link"
    case "$target" in
      /*) ck_unpack_fail "$staging" "refusing $archive: absolute link ${link#"$staging/x/"} -> $target" ;;
    esac
    if ! ck_link_inside "$link" "$real"; then
      ck_unpack_fail "$staging" "refusing $archive: link ${link#"$staging/x/"} -> $target is dangling or points outside the tree"
    fi
  done <"$staging/links"

  if [ -e "$dest" ] || [ -L "$dest" ]; then
    ck_unpack_fail "$staging" "already exists: $dest"
  fi
  mv -- "$root" "$dest" || ck_unpack_fail "$staging" "cannot move the unpacked tree to $dest"
  rm -rf -- "$staging"
}

# ck_unpack_fail <staging> <message...>: deletes the staging folder and dies.
ck_unpack_fail() {
  local staging=$1
  shift
  case "$staging" in
    */.ck-unpack.*) rm -rf -- "$staging" ;;
  esac
  ck_die "$@"
}

# ck_link_inside <link> <real root>: true when the link's target exists and,
# with every link on the way resolved, lies inside the root.
ck_link_inside() {
  local link=$1 root=$2 dir target full resolved
  dir=$(dirname -- "$link") || return 1
  target=$(readlink "$link") || return 1
  full="$dir/$target"
  if [ -d "$full" ]; then
    resolved=$(cd -P "$full" 2>/dev/null && pwd -P) || return 1
  elif [ -e "$full" ]; then
    resolved=$(cd -P "$(dirname -- "$full")" 2>/dev/null && pwd -P) || return 1
  else
    return 1
  fi
  case "$resolved" in
    "$root" | "$root"/*) return 0 ;;
  esac
  return 1
}

# ck_fetch_unpack <name> <parent>: fetches and checks the loaded pin for
# <name>, unpacks it as <parent>/<name>-<version>/ and prints that path. A
# folder already there is reused.
ck_fetch_unpack() {
  [ "$#" -eq 2 ] || ck_die "ck_fetch_unpack: expected 2 arguments, got $#"
  local want=$1 parent=$2 line name version url sha size file archive dest
  line=$(ck_pin "$want") || exit 1
  IFS=$'\t' read -r name version url sha size _ _ file <<<"$line"
  dest="$parent/$name-$version"
  if [ -d "$dest" ] && [ ! -L "$dest" ]; then
    ck_log "reusing $dest"
    printf '%s\n' "$dest"
    return 0
  fi
  archive=$(ck_fetch "$name" "$version" "$url" "$sha" "$size" "$file") || exit 1
  mkdir -p -- "$parent" || ck_die "cannot create $parent"
  ck_unpack "$archive" "$dest"
  printf '%s\n' "$dest"
}
