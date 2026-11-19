# shellcheck shell=bash
# macOS build flags, sourced on macOS only. lib/platform-linux.sh defines the
# same functions for Linux, so callers source lib/platform-$(ck_os).sh and
# never branch on the OS for these:
#
#   ck_platform_env              compilers and flags, after ck_hermetic_env
#   ck_platform_shlib_ext        prints the shared library suffix: dylib
#   ck_platform_rpath <folder>   prints the relocated library search path for
#                                a file in the archive's bin/, ext/ or lib/
#
# Also defines the read-only CK_MACOS_DEPLOYMENT_TARGET.
#
# Build-time linking: every dylib in the deps prefix carries its absolute
# install name, <deps prefix>/lib/<name>, so configure run-tests and the php
# binary the PHP build runs load the stack's libraries. Relocation rewrites
# them to @rpath/<name>, adds ck_platform_rpath's value as LC_RPATH, and
# signs again. -headerpad_max_install_names leaves room for that.

if [ -n "${CK_PLATFORM_SH_LOADED:-}" ]; then
  return 0
fi
# shellcheck source=common.sh
. "$(dirname "${BASH_SOURCE[0]}")/common.sh"
[ "$(ck_os)" = macos ] || ck_die "platform-macos.sh sourced on $(uname -s)"
CK_PLATFORM_SH_LOADED=1

# The oldest macOS the builds run on.
readonly CK_MACOS_DEPLOYMENT_TARGET=12.0

# ck_platform_env: exports CC, CXX, CFLAGS, CXXFLAGS and
# MACOSX_DEPLOYMENT_TARGET, and appends to LDFLAGS. Call it once, right
# after ck_hermetic_env.
ck_platform_env() {
  [ -n "${CK_DEPS_PREFIX:-}" ] || ck_die "ck_platform_env: run ck_hermetic_env first"
  # Apple's compilers from the active Xcode; never Homebrew's.
  local cc=/usr/bin/clang cxx=/usr/bin/clang++
  local min="-mmacosx-version-min=$CK_MACOS_DEPLOYMENT_TARGET"
  [ -x "$cc" ] || ck_die "missing compiler: $cc"
  [ -x "$cxx" ] || ck_die "missing compiler: $cxx"
  export CC="$cc"
  export CXX="$cxx"
  # Covers every compiler and linker call, including those that skip CFLAGS.
  export MACOSX_DEPLOYMENT_TARGET="$CK_MACOS_DEPLOYMENT_TARGET"
  # The prefix map keeps build paths out of __FILE__ strings.
  export CFLAGS="-O2 -fPIC $min -ffile-prefix-map=$CK_BUILD_ROOT/="
  export CXXFLAGS="$CFLAGS"
  export LDFLAGS="$LDFLAGS $min -Wl,-headerpad_max_install_names"
}

# ck_platform_shlib_ext: prints the shared library suffix.
ck_platform_shlib_ext() {
  echo dylib
}

# ck_platform_rpath <bin|ext|lib>: prints the library search path a file in
# that archive folder gets when relocated; libraries sit in lib/.
ck_platform_rpath() {
  case "${1:-}" in
    bin | ext) echo '@loader_path/../lib' ;;
    lib) echo '@loader_path' ;;
    *) ck_die "ck_platform_rpath: not bin, ext or lib: ${1:-}" ;;
  esac
}
