# shellcheck shell=bash
# Linux build flags, sourced on Linux only. lib/platform-macos.sh defines the
# same functions for macOS, so callers source lib/platform-$(ck_os).sh and
# never branch on the OS for these:
#
#   ck_platform_env              compilers and flags, after ck_hermetic_env
#   ck_platform_shlib_ext        prints the shared library suffix: so
#   ck_platform_rpath <folder>   prints the relocated library search path for
#                                a file in the archive's bin/, ext/ or lib/
#
# Build-time linking: every object gets a DT_RUNPATH to <deps prefix>/lib, so
# configure run-tests and the php binary the PHP build runs load the stack's
# libraries, never the runner's (LD_LIBRARY_PATH is unset). Relocation
# replaces it with ck_platform_rpath's value as DT_RPATH
# (`patchelf --force-rpath --set-rpath`).

if [ -n "${CK_PLATFORM_SH_LOADED:-}" ]; then
  return 0
fi
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[ "$(ck_os)" = linux ] || ck_die "platform-linux.sh sourced on $(uname -s)"
CK_PLATFORM_SH_LOADED=1

# ck_platform_env: exports CC, CXX, CFLAGS and CXXFLAGS, and appends the
# runpath to LDFLAGS. Call it once, right after ck_hermetic_env.
ck_platform_env() {
  [ -n "${CK_DEPS_PREFIX:-}" ] || ck_die "ck_platform_env: run ck_hermetic_env first"
  # The runner's default GCC; the glibc floor comes from the runner image.
  local cc=/usr/bin/gcc cxx=/usr/bin/g++
  [ -x "$cc" ] || ck_die "missing compiler: $cc"
  [ -x "$cxx" ] || ck_die "missing compiler: $cxx"
  export CC="$cc"
  export CXX="$cxx"
  # The prefix map keeps build paths out of __FILE__ strings.
  export CFLAGS="-O2 -fPIC -ffile-prefix-map=$CK_BUILD_ROOT/="
  export CXXFLAGS="$CFLAGS"
  # New dtags give DT_RUNPATH, which LD_LIBRARY_PATH overrides: ICU's and
  # libtool's in-tree tools then load their fresh libraries, not an older
  # copy already in the prefix.
  export LDFLAGS="$LDFLAGS -Wl,-rpath,$CK_DEPS_PREFIX/lib -Wl,--enable-new-dtags"
}

# ck_platform_shlib_ext: prints the shared library suffix.
ck_platform_shlib_ext() {
  echo so
}

# ck_platform_rpath <bin|ext|lib>: prints the library search path a file in
# that archive folder gets when relocated; libraries sit in lib/.
ck_platform_rpath() {
  # shellcheck disable=SC2016 # $ORIGIN is for the loader, not the shell
  case "${1:-}" in
    bin | ext) echo '$ORIGIN/../lib' ;;
    lib) echo '$ORIGIN' ;;
    *) ck_die "ck_platform_rpath: not bin, ext or lib: ${1:-}" ;;
  esac
}
