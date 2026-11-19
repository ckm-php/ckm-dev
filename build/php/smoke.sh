#!/usr/bin/env bash
# Usage: smoke.sh <root> [<php version>]
#
# Runs a relocated PHP root the way T4 (crates/ck-net/tests/php_archive.rs)
# and the app do: bin/php from the root, with an empty environment except
# PATH, no ini file (-n), and the relocation contract's settings
# (extension_dir, openssl.cafile, curl.cainfo, include_path). Then:
#
# 1. `php -v` names the expected version, when one is given (the argument,
#    or CK_PHP_EXPECT_VERSION);
# 2. smoke/smoke.php passes with every shared extension loaded, opcache as a
#    Zend extension (CK_PHP_EXPECT_BUILTIN is passed on when set);
# 3. each shared extension loads alone.
#
# Any "PHP Startup" message, such as a library that failed to load, fails.
# Sourced by build.sh and bare-root.sh for ck_smoke and ck_smoke_args; run
# directly for local or CI use. Runs only builds this pipeline made, never a
# PHP from elsewhere.

if [ -n "${CK_SMOKE_SH_LOADED:-}" ]; then
  return 0
fi
# shellcheck source=lib/common.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"
CK_SMOKE_SH_LOADED=1

readonly CK_SMOKE_PHP="$CK_REPO_ROOT/build/php/smoke/smoke.php"

# The settings every run passes, and the arguments that load every shared
# extension, built by ck_smoke_args.
CK_SMOKE_BASE=()
CK_SMOKE_LOAD=()
CK_SMOKE_EXTS=()

# The combined output of the last ck_smoke_php run.
CK_SMOKE_OUT=

# ck_smoke_ext_setting <root as PHP sees it> <name>: prints the -d value that
# loads one shared extension.
ck_smoke_ext_setting() {
  if [ "$2" = opcache ]; then
    printf '%s\n' "zend_extension=$1/ext/opcache.so"
  else
    printf '%s\n' "extension=$2"
  fi
}

# ck_smoke_args <root as PHP sees it> <root on this machine>: fills
# CK_SMOKE_BASE, CK_SMOKE_EXTS (the shared extensions, sorted) and
# CK_SMOKE_LOAD. The roots differ when PHP runs in a container.
ck_smoke_args() {
  local seen=$1 host=$2 path name setting sorted cert="$1/etc/ssl/cert.pem"
  CK_SMOKE_BASE=(
    -n
    -d "extension_dir=$seen/ext"
    -d "openssl.cafile=$cert"
    -d "curl.cainfo=$cert"
    -d include_path=.
  )
  CK_SMOKE_EXTS=()
  CK_SMOKE_LOAD=()
  for path in "$host"/ext/*.so; do
    [ -f "$path" ] || continue
    name=${path##*/}
    CK_SMOKE_EXTS+=("${name%.so}")
  done
  [ "${#CK_SMOKE_EXTS[@]}" -gt 0 ] || ck_die "smoke: $host/ext holds no shared extensions"
  # Sorted by name, as T4 loads them.
  sorted=$(printf '%s\n' "${CK_SMOKE_EXTS[@]}" | LC_ALL=C sort) || ck_die "smoke: sort failed"
  CK_SMOKE_EXTS=()
  while IFS= read -r name; do
    CK_SMOKE_EXTS+=("$name")
  done <<<"$sorted"
  for name in "${CK_SMOKE_EXTS[@]}"; do
    setting=$(ck_smoke_ext_setting "$seen" "$name") || exit 1
    CK_SMOKE_LOAD+=(-d "$setting")
  done
}

# ck_smoke_check <what> <status>: prints the last run's output to stderr,
# then dies when it exited non-zero or printed a startup error.
ck_smoke_check() {
  local what=$1 status=$2
  if [ -n "$CK_SMOKE_OUT" ]; then printf '%s\n' "$CK_SMOKE_OUT" >&2; fi
  [ "$status" -eq 0 ] || ck_die "smoke: $what exited with $status"
  case "$CK_SMOKE_OUT" in
    *'PHP Startup'*) ck_die "smoke: $what printed a startup error" ;;
  esac
}

# ck_smoke_php <root> <php args...>: runs <root>/bin/php from the root with
# an empty environment except PATH and the CK_PHP_EXPECT_* variables, and
# keeps its combined output in CK_SMOKE_OUT. Returns PHP's exit status.
ck_smoke_php() {
  local root=$1 status=0
  shift
  ck_log "+ (in $root) bin/php $*"
  CK_SMOKE_OUT=$(cd "$root" && env -i PATH=/usr/bin:/bin \
    CK_PHP_EXPECT_VERSION="${CK_PHP_EXPECT_VERSION:-}" \
    CK_PHP_EXPECT_BUILTIN="${CK_PHP_EXPECT_BUILTIN:-}" \
    "$root/bin/php" "$@" 2>&1) || status=$?
  return "$status"
}

# ck_smoke <root> [<php version>]: the three runs above.
ck_smoke() {
  local root=${1:-} want=${2:-${CK_PHP_EXPECT_VERSION:-}} status name setting module
  case "$root" in
    /*) ;;
    *) ck_die "smoke: the root must be an absolute path: $root" ;;
  esac
  [ -x "$root/bin/php" ] || ck_die "smoke: no bin/php in $root"
  [ -f "$CK_SMOKE_PHP" ] || ck_die "smoke: missing $CK_SMOKE_PHP"
  CK_PHP_EXPECT_VERSION=$want
  ck_smoke_args "$root" "$root"

  status=0
  ck_smoke_php "$root" "${CK_SMOKE_BASE[@]}" -v || status=$?
  ck_smoke_check "php -v" "$status"
  if [ -n "$want" ]; then
    case "$CK_SMOKE_OUT" in
      "PHP $want "*) ;;
      *) ck_die "smoke: php -v does not name PHP $want" ;;
    esac
  fi

  status=0
  ck_smoke_php "$root" "${CK_SMOKE_BASE[@]}" "${CK_SMOKE_LOAD[@]}" "$CK_SMOKE_PHP" || status=$?
  ck_smoke_check "smoke.php" "$status"

  for name in "${CK_SMOKE_EXTS[@]}"; do
    setting=$(ck_smoke_ext_setting "$root" "$name") || exit 1
    module=$name
    if [ "$name" = opcache ]; then module='Zend OPcache'; fi
    status=0
    ck_smoke_php "$root" "${CK_SMOKE_BASE[@]}" -d "$setting" \
      -r "exit(extension_loaded('$module') ? 0 : 1);" || status=$?
    ck_smoke_check "loading $name alone" "$status"
  done
  ck_log "smoke: $root passed"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: smoke.sh <root> [<php version>]" >&2
    exit 2
  fi
  ck_smoke "$@"
fi
