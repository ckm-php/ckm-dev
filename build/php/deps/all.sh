# shellcheck shell=bash
# Builds a library stack from pinned source into its deps prefix.
#
# Source this file (it sources common.sh, fetch.sh, the platform file and
# every recipe), then call `ck_deps_build <stack>`. Recipes are not meant to
# be sourced alone: they use the ck_dep_* helpers below.
#
# Each deps/<name>.sh defines ck_dep_<name> (no arguments). It builds <name>
# for $CK_STACK into $CK_DEPS_PREFIX, from the stack's pin, and writes the
# stamp <prefix>/.stamps/<name>-<version> last. With the stamp present it
# does nothing, so a restored prefix is reused. Shared libraries for what
# PHP links; static build tools; no docs, man pages, static archives of the
# libraries or libtool .la files.
#
# Recipes run every configure, make and install step through ck_dep_run,
# which dies on failure. Call ck_deps_build and the recipes as plain
# commands, never in an `if`, `&&` or `||`: bash turns `set -e` off inside a
# function called there.

# Each library file returns early when it is already loaded.
# shellcheck source=../lib/common.sh
. "$(dirname "${BASH_SOURCE[0]}")/../lib/common.sh"
# shellcheck source=../lib/fetch.sh
. "$CK_REPO_ROOT/build/php/lib/fetch.sh"
# shellcheck source=/dev/null
. "$CK_REPO_ROOT/build/php/lib/platform-$(ck_os).sh"

# ck_dep_version <name>: prints the stack's pinned version of <name>.
ck_dep_version() {
  local pin name version
  pin=$(ck_pin "$1")
  IFS=$'\t' read -r name version _ <<<"$pin"
  if [ "$name" != "$1" ] || [ -z "$version" ]; then
    ck_die "deps: no usable pin for $1"
  fi
  echo "$version"
}

# ck_dep_is_built <name> <version>: true when the stamp exists.
ck_dep_is_built() {
  if [ -e "$CK_DEPS_PREFIX/.stamps/$1-$2" ]; then
    ck_log "deps: $1 $2 already built for $CK_STACK"
    return 0
  fi
  return 1
}

# ck_dep_unpack <name> <version>: removes any earlier tree of this pin, which
# ck_fetch_unpack would reuse, so every build starts from a fresh source
# tree; then fetches, verifies and unpacks it. Prints the source folder.
ck_dep_unpack() {
  local parent src
  parent=$(ck_src_dir "$CK_STACK")
  ck_log "deps: building $1 $2 for $CK_STACK"
  rm -rf "${parent:?}/$1-$2"
  src=$(ck_fetch_unpack "$1" "$parent")
  [ -d "$src" ] || ck_die "deps: $1: unpacked source is not a folder: $src"
  echo "$src"
}

# ck_dep_run <dir> <argv...>: runs one command in <dir> and dies if it
# fails. The explicit check does not rely on `set -e` stopping at a failed
# subshell, which bash 3.2 does not promise.
ck_dep_run() {
  local dir=$1
  shift
  (cd "$dir" && ck_run "$@") || ck_die "deps: $1 failed in $dir"
}

# ck_dep_need <file>: exits 1 unless the file exists, naming it, so a changed
# archive layout fails clearly.
ck_dep_need() {
  [ -f "$1" ] || ck_die "deps: missing $1 (did the archive layout change?)"
}

# ck_dep_finish <name> <version> <check...>: removes docs, man pages, static
# archives and libtool .la files from the prefix, checks what the recipe
# installed, then writes the stamp. A check with a slash is a path under the
# prefix that must be executable (bin/pkgconf); any other check is a library
# that must exist as lib/<check>.<so|dylib> (libz).
ck_dep_finish() {
  local name=$1 version=$2 prefix=$CK_DEPS_PREFIX ext check
  shift 2
  ext=$(ck_platform_shlib_ext)
  rm -rf "$prefix/share/man" "$prefix/share/doc" "$prefix/share/gtk-doc" "$prefix/share/info"
  rm -f "$prefix"/lib/*.a "$prefix"/lib/*.la
  for check in "$@"; do
    case "$check" in
      */*) [ -x "$prefix/$check" ] || ck_die "deps: $name did not install $check" ;;
      *) [ -e "$prefix/lib/$check.$ext" ] || ck_die "deps: $name did not install lib/$check.$ext" ;;
    esac
  done
  mkdir -p "$prefix/.stamps"
  echo "$name $version" >"$prefix/.stamps/$name-$version"
  ck_log "deps: $name $version installed for $CK_STACK"
}

# shellcheck source=pkgconf.sh
. "$CK_REPO_ROOT/build/php/deps/pkgconf.sh"
# shellcheck source=patchelf.sh
. "$CK_REPO_ROOT/build/php/deps/patchelf.sh"
# shellcheck source=zlib.sh
. "$CK_REPO_ROOT/build/php/deps/zlib.sh"
# shellcheck source=openssl.sh
. "$CK_REPO_ROOT/build/php/deps/openssl.sh"
# shellcheck source=libxml2.sh
. "$CK_REPO_ROOT/build/php/deps/libxml2.sh"
# shellcheck source=sqlite.sh
. "$CK_REPO_ROOT/build/php/deps/sqlite.sh"
# shellcheck source=oniguruma.sh
. "$CK_REPO_ROOT/build/php/deps/oniguruma.sh"
# shellcheck source=libsodium.sh
. "$CK_REPO_ROOT/build/php/deps/libsodium.sh"
# shellcheck source=icu.sh
. "$CK_REPO_ROOT/build/php/deps/icu.sh"
# shellcheck source=curl.sh
. "$CK_REPO_ROOT/build/php/deps/curl.sh"

# ck_deps_build <legacy|current>: sets the hermetic build environment and the
# platform flags (left exported for the PHP stage), loads the stack's pins,
# then builds every library in dependency order: tools first, then zlib and
# OpenSSL, which curl and libxml2 link.
ck_deps_build() {
  local os
  ck_check_stack "${1:-}"
  os=$(ck_os)
  ck_hermetic_env "$1"
  ck_platform_env
  ck_pins_load --stack "$1"
  ck_dep_pkgconf
  if [ "$os" = linux ]; then
    ck_dep_patchelf
  fi
  ck_dep_zlib
  ck_dep_openssl
  ck_dep_libxml2
  ck_dep_sqlite
  ck_dep_oniguruma
  ck_dep_libsodium
  ck_dep_icu
  ck_dep_curl
  ck_log "deps: stack $1 is ready in $CK_DEPS_PREFIX"
}
