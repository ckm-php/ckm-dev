# shellcheck shell=bash
# Helpers every PHP build script sources first. The function names and the
# folder layout are frozen: see build/php/README.md.
#
# Bash 3.2 only: no associative arrays, mapfile, namerefs or ${var,,}. Under
# `set -u` bash before 4.4 fails on "${empty[@]}", so expand an array that may
# be empty as ${arr[@]+"${arr[@]}"}. Assign a command's output on its own line
# (`x=$(f)`, not `local x=$(f)`), or a failure in f is lost.
#
# Sourcing sets strict mode and umask and defines functions and read-only
# constants. It creates nothing and changes no other variable.

if [ -n "${CK_COMMON_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${BASH_VERSION:-}" ]; then
  echo "ck-build: error: run the build scripts with bash" >&2
  exit 1
fi
CK_COMMON_SH_LOADED=1

set -euo pipefail
umask 022

# The fixed build root. Only root can create it on a user's machine, so no
# path baked into a build can point to a folder a user controls. Never
# overridable.
readonly CK_BUILD_ROOT=/opt/ck-dev-build

# The repository root, found from this file's location.
CK_REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd -P)
readonly CK_REPO_ROOT

# ck_log <message...>: prints one line to stderr.
ck_log() {
  printf 'ck-build: %s\n' "$*" >&2
}

# ck_die <message...>: prints an error line to stderr and exits 1.
ck_die() {
  printf 'ck-build: error: %s\n' "$*" >&2
  exit 1
}

# ck_require_cmd <command...>: exits 1 unless every command is on PATH.
ck_require_cmd() {
  local cmd
  for cmd in "$@"; do
    command -v "$cmd" >/dev/null 2>&1 || ck_die "missing command: $cmd"
  done
}

# ck_run <argv...>: prints the command, shell-quoted, to stderr, then runs it.
ck_run() {
  [ "$#" -gt 0 ] || ck_die "ck_run: no command"
  local quoted
  quoted=$(printf '%q ' "$@")
  ck_log "+ ${quoted% }"
  "$@"
}

# ck_os: prints linux or macos (ck-core's OS names); exits 1 elsewhere.
ck_os() {
  local kernel
  kernel=$(uname -s)
  case "$kernel" in
    Linux) echo linux ;;
    Darwin) echo macos ;;
    *) ck_die "unsupported OS: $kernel" ;;
  esac
}

# ck_cpu: prints x86_64 or aarch64 (ck-core's CPU names); exits 1 elsewhere.
ck_cpu() {
  local machine
  machine=$(uname -m)
  case "$machine" in
    x86_64 | amd64) echo x86_64 ;;
    arm64 | aarch64) echo aarch64 ;;
    *) ck_die "unsupported CPU: $machine" ;;
  esac
}

# ck_jobs: prints the parallel job count: CK_JOBS if set, else the CPU count.
ck_jobs() {
  local jobs
  if [ -n "${CK_JOBS:-}" ]; then
    jobs=$CK_JOBS
  else
    jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)
  fi
  case "$jobs" in
    '' | *[!0-9]* | 0*) ck_die "not a job count: $jobs" ;;
  esac
  echo "$jobs"
}

# ck_check_stack <stack>: exits 1 unless the stack is legacy or current.
ck_check_stack() {
  case "${1:-}" in
    legacy | current) ;;
    *) ck_die "not a library stack (legacy, current): ${1:-}" ;;
  esac
}

# ck_check_line <line>: exits 1 unless the PHP line looks like 8.5.
ck_check_line() {
  local re='^[0-9]+\.[0-9]+$'
  [[ ${1:-} =~ $re ]] || ck_die "not a PHP line such as 8.5: ${1:-}"
}

# ck_build_root: prints the fixed build root.
ck_build_root() {
  echo "$CK_BUILD_ROOT"
}

# ck_cache_dir: prints the download cache: CK_CACHE_DIR (absolute) if set,
# else <root>/cache. Each pin gets its own folder, <cache>/<name>-<version>/.
ck_cache_dir() {
  if [ -n "${CK_CACHE_DIR:-}" ]; then
    case "$CK_CACHE_DIR" in
      /*) echo "$CK_CACHE_DIR" ;;
      *) ck_die "CK_CACHE_DIR must be an absolute path: $CK_CACHE_DIR" ;;
    esac
  else
    echo "$CK_BUILD_ROOT/cache"
  fi
}

# ck_deps_prefix <stack>: prints the stack's install prefix, with bin, lib,
# include, lib/pkgconfig and share under it.
ck_deps_prefix() {
  ck_check_stack "${1:-}"
  echo "$CK_BUILD_ROOT/deps/$1"
}

# ck_src_dir <stack>: prints the folder that holds the stack's unpacked
# library sources, one <name>-<version>/ folder each.
ck_src_dir() {
  ck_check_stack "${1:-}"
  echo "$CK_BUILD_ROOT/src/$1"
}

# ck_php_dir <line>: prints the line's working folder.
ck_php_dir() {
  ck_check_line "${1:-}"
  echo "$CK_BUILD_ROOT/php/$1"
}

# ck_php_src_dir <line>: prints the line's unpacked PHP source tree.
ck_php_src_dir() {
  ck_check_line "${1:-}"
  echo "$CK_BUILD_ROOT/php/$1/src"
}

# ck_php_prefix <line>: prints the --prefix PHP is configured and installed with.
ck_php_prefix() {
  ck_check_line "${1:-}"
  echo "$CK_BUILD_ROOT/php/$1/prefix"
}

# ck_php_root <line>: prints the relocated archive tree that `ck-release pack` packs.
ck_php_root() {
  ck_check_line "${1:-}"
  echo "$CK_BUILD_ROOT/php/$1/root"
}

# ck_out_dir: prints the folder for archives and their descriptors.
ck_out_dir() {
  echo "$CK_BUILD_ROOT/out"
}

# ck_hermetic_env <stack>: resets the build environment to the stack's
# prefix and the system's base tools, so configure never finds Homebrew,
# /usr/local or a runner's development libraries.
ck_hermetic_env() {
  ck_check_stack "${1:-}"
  local prefix
  prefix=$(ck_deps_prefix "$1")
  unset PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR \
    CPATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH OBJC_INCLUDE_PATH LIBRARY_PATH \
    LD_LIBRARY_PATH LD_RUN_PATH LD_PRELOAD \
    DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_FRAMEWORK_PATH \
    DYLD_FALLBACK_FRAMEWORK_PATH DYLD_INSERT_LIBRARIES \
    CC CXX CPP CFLAGS CXXFLAGS OBJCFLAGS ACLOCAL_PATH CONFIG_SITE \
    HOMEBREW_PREFIX HOMEBREW_CELLAR HOMEBREW_REPOSITORY
  export CK_STACK="$1"
  export CK_DEPS_PREFIX="$prefix"
  export PATH="$prefix/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  export PKG_CONFIG="$prefix/bin/pkgconf"
  export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig:$prefix/share/pkgconfig"
  export CPPFLAGS="-I$prefix/include"
  export LDFLAGS="-L$prefix/lib"
  export LC_ALL=C
  export TZ=UTC
}

# ck_release <args...>: runs ck-release from CK_RELEASE_BIN, an absolute
# path set before the build, since the hermetic PATH has no cargo.
ck_release() {
  case "${CK_RELEASE_BIN:-}" in
    /*) ;;
    *) ck_die "CK_RELEASE_BIN must be the absolute path of ck-release" ;;
  esac
  [ -x "$CK_RELEASE_BIN" ] || ck_die "not executable: $CK_RELEASE_BIN"
  "$CK_RELEASE_BIN" "$@"
}
