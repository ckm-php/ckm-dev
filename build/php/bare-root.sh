# shellcheck shell=bash
# T7: runs a packed PHP archive where no library but its own and the allowed
# system ones can be found. Sourced by build.sh after audit.sh and smoke.sh.
# CI only: it never runs on a developer's machine.
#
# - Linux: a root file system holding only the unpacked archive, the
#   allow-listed system libraries copied from the host at the paths the host
#   loads them from, the program interpreter, glibc's iconv codecs, an empty /tmp and the smoke
#   script. It is imported into the local Docker image store (no registry)
#   and run with no network as an unprivileged user.
# - macOS: the workflow moves /opt/homebrew and the /usr/local library
#   folders away first (it has sudo; this script does not). The archive is unpacked into a folder
#   with a space and run with an empty environment, so no DYLD_* variable is
#   set.
#
# Both audit the unpacked archive, check `php -v`, and run the smoke script
# with every shared extension loaded.
#
# Call the functions as plain commands, never in an `if`, `&&` or `||`.

if [ -n "${CK_BARE_ROOT_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${CK_AUDIT_SH_LOADED:-}" ] || [ -z "${CK_SMOKE_SH_LOADED:-}" ]; then
  echo "ck-build: error: source audit.sh and smoke.sh before bare-root.sh" >&2
  exit 1
fi
CK_BARE_ROOT_SH_LOADED=1

# The temporary folder and image of the current run, removed on exit.
CK_BARE_WORK=
CK_BARE_IMAGE=

# ck_bare_cleanup: removes what the run created. Runs from the EXIT trap.
ck_bare_cleanup() {
  if [ -n "$CK_BARE_IMAGE" ]; then
    docker image rm -f "$CK_BARE_IMAGE" >/dev/null 2>&1 || true
  fi
  case "$CK_BARE_WORK" in
    */ck-bare.*) rm -rf -- "$CK_BARE_WORK" ;;
  esac
}

# ck_bare_unpack <archive> <dest>: unpacks a packed archive into <dest>,
# which must exist, after checking its listing has no absolute or `..` path.
ck_bare_unpack() {
  local archive=$1 dest=$2 listing member
  listing=$(tar -tzf "$archive") || ck_die "bare root: cannot list $archive"
  [ -n "$listing" ] || ck_die "bare root: empty archive: $archive"
  while IFS= read -r member; do
    case "$member" in
      /*) ck_die "bare root: absolute member path in $archive: $member" ;;
    esac
    case "/$member/" in
      */../*) ck_die "bare root: member path with '..' in $archive: $member" ;;
    esac
  done <<<"$listing"
  ck_run tar -xzf "$archive" -C "$dest"
}

# ck_bare_version <archive>: prints the PHP version its file name gives:
# php-<X.Y.Z>-ck<n>-<os>-<cpu>.tar.gz.
ck_bare_version() {
  local base=${1##*/} os cpu rest
  os=$(ck_os)
  cpu=$(ck_cpu)
  case "$base" in
    php-*-ck*-"$os-$cpu".tar.gz) ;;
    *) ck_die "bare root: not a PHP archive for $os-$cpu: $base" ;;
  esac
  rest=${base#php-}
  rest=${rest%-"$os-$cpu".tar.gz}
  printf '%s\n' "${rest%-ck*}"
}

# ck_bare_host_lib <soname>: prints the path the host's loader resolves the
# soname to, or nothing when the host does not have it.
ck_bare_host_lib() {
  local cache line name rest
  cache=$(/sbin/ldconfig -p) || ck_die "bare root: ldconfig -p failed"
  while IFS= read -r line; do
    line=${line#"${line%%[![:space:]]*}"}
    name=${line%% *}
    [ "$name" = "$1" ] || continue
    case "$line" in
      *x86-64*' => '/*)
        rest=${line##* => }
        printf '%s\n' "$rest"
        return 0
        ;;
    esac
  done <<<"$cache"
}

# ck_bare_copy_host <rootfs> <path>: copies a host file, following links, to
# the same path inside the root file system.
ck_bare_copy_host() {
  local rootfs=$1 path=$2
  ck_run mkdir -p "$rootfs$(dirname "$path")"
  ck_run cp -L "$path" "$rootfs$path"
}

# ck_bare_root_linux <archive> <version>
ck_bare_root_linux() {
  local archive=$1 version=$2 rootfs php_dir="/ck dev/php" interp header so path gconv host_glibc status=0
  local -a run sonames
  ck_require_cmd docker tar readelf getconf
  # The root borrows the runner's libc; a newer host would not prove the floor.
  host_glibc=$(LC_ALL=C getconf GNU_LIBC_VERSION) || ck_die "bare root: cannot read the host glibc version"
  [ "$host_glibc" = "glibc $CK_AUDIT_GLIBC_MAX" ] ||
    ck_die "bare root: expected host glibc $CK_AUDIT_GLIBC_MAX, got $host_glibc"
  rootfs="$CK_BARE_WORK/rootfs"
  ck_run mkdir -p "$rootfs$php_dir" "$rootfs/tmp"
  ck_run chmod 1777 "$rootfs/tmp"
  ck_bare_unpack "$archive" "$rootfs$php_dir"
  ck_audit "$rootfs$php_dir"

  header=$(LC_ALL=C readelf -lW "$rootfs$php_dir/bin/php") || ck_die "bare root: readelf -l failed"
  interp=${header#*Requesting program interpreter: }
  interp=${interp%%]*}
  case "$interp" in
    /*) ;;
    *) ck_die "bare root: bin/php names no program interpreter" ;;
  esac
  ck_bare_copy_host "$rootfs" "$interp"
  read -r -d '' -a sonames <<<"$CK_AUDIT_LINUX_SYSTEM" || true
  for so in "${sonames[@]}"; do
    path=$(ck_bare_host_lib "$so") || exit 1
    if [ -z "$path" ]; then
      ck_log "bare root: the host has no $so"
      continue
    fi
    ck_bare_copy_host "$rootfs" "$path"
  done
  # glibc's iconv codecs and their config ship with glibc on every
  # distribution; PHP's iconv extension needs them.
  gconv=/usr/lib/x86_64-linux-gnu/gconv
  [ -d "$gconv" ] || ck_die "bare root: the host has no $gconv"
  ck_run mkdir -p "$rootfs${gconv%/*}"
  ck_run cp -RL "$gconv" "$rootfs$gconv"
  ck_run mkdir -p "$rootfs/ck"
  ck_run cp "$CK_SMOKE_PHP" "$rootfs/ck/smoke.php"

  ck_smoke_args "$php_dir" "$rootfs$php_dir"
  CK_BARE_IMAGE="ck-php-bare:t$$"
  ck_run tar -C "$rootfs" --numeric-owner --owner=0 --group=0 -cf "$CK_BARE_WORK/rootfs.tar" .
  ck_run docker import "$CK_BARE_WORK/rootfs.tar" "$CK_BARE_IMAGE"
  run=(docker run --rm --network none --user 65534:65534 --workdir "$php_dir"
    --env "CK_PHP_EXPECT_VERSION=$version" "$CK_BARE_IMAGE" "$php_dir/bin/php")

  ck_log "+ ${run[*]} ${CK_SMOKE_BASE[*]} -v"
  CK_SMOKE_OUT=$("${run[@]}" "${CK_SMOKE_BASE[@]}" -v 2>&1) || status=$?
  ck_smoke_check "php -v in the bare root" "$status"
  case "$CK_SMOKE_OUT" in
    "PHP $version "*) ;;
    *) ck_die "bare root: php -v does not name PHP $version" ;;
  esac
  ck_log "+ ${run[*]} ${CK_SMOKE_BASE[*]} ${CK_SMOKE_LOAD[*]} /ck/smoke.php"
  status=0
  CK_SMOKE_OUT=$("${run[@]}" "${CK_SMOKE_BASE[@]}" "${CK_SMOKE_LOAD[@]}" /ck/smoke.php 2>&1) || status=$?
  ck_smoke_check "smoke.php in the bare root" "$status"
}

# ck_bare_root_macos <archive> <version>
ck_bare_root_macos() {
  local archive=$1 version=$2 root dir
  # /usr/local itself is on the sealed system volume, so the workflow moves
  # only Homebrew and the /usr/local folders a link or dyld can search.
  for dir in /opt/homebrew /usr/local/lib /usr/local/include /usr/local/opt \
    /usr/local/Cellar /usr/local/Frameworks; do
    if [ -e "$dir" ] || [ -L "$dir" ]; then
      ck_die "bare root: $dir exists; the workflow must move it away before this step"
    fi
  done
  root="$CK_BARE_WORK/ck dev/php"
  ck_run mkdir -p "$root"
  ck_bare_unpack "$archive" "$root"
  ck_audit "$root"
  ck_smoke "$root" "$version"
}

# ck_bare_root <archive.tar.gz>: T7 for this OS.
ck_bare_root() {
  local archive=${1:-} version os
  [ -f "$archive" ] || ck_die "bare root: not a file: $archive"
  archive=$(cd "$(dirname "$archive")" && pwd -P)/${archive##*/}
  version=$(ck_bare_version "$archive") || exit 1
  os=$(ck_os)
  CK_BARE_WORK=$(mktemp -d "${TMPDIR:-/tmp}/ck-bare.XXXXXX") || ck_die "bare root: mktemp failed"
  trap ck_bare_cleanup EXIT
  case "$os" in
    linux) ck_bare_root_linux "$archive" "$version" ;;
    macos) ck_bare_root_macos "$archive" "$version" ;;
  esac
  ck_log "bare root: $archive runs on its own"
}
