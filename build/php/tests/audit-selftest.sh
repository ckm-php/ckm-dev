#!/usr/bin/env bash
# T5: the string rules and Linux glibc boundary of audit.sh.
# ck_audit_strings allows under /opt only the baked strings it names, each
# matched whole, and refuses build paths
# (fileinfo.so's libmagic exception stays scoped to that file);
# ck_audit_null_dir requires the start-up defaults in the null folder. The
# object files are synthetic: NUL-separated strings, as a binary holds them.
#
# Usage: build/php/tests/audit-selftest.sh

PHP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
# shellcheck source=../lib/common.sh
. "$PHP_DIR/lib/common.sh"
# shellcheck source=/dev/null
. "$PHP_DIR/lib/platform-$(ck_os).sh"
# shellcheck source=../audit.sh
. "$PHP_DIR/audit.sh"

T_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/ck-audit-selftest.XXXXXX")
T_ROOT=$(cd "$T_ROOT" && pwd -P)
trap 'rm -rf -- "$T_ROOT"' EXIT

T_PASSED=0
T_NULL=$(ck_platform_null_dir)
T_B=/opt/ck-dev-build

t_fail() {
  printf 'audit-selftest: FAIL: %s\n' "$*" >&2
  if [ -s "$T_ROOT/stderr" ]; then
    printf '  audit output:\n%s\n' "$(cat "$T_ROOT/stderr")" >&2
  fi
  exit 1
}

t_ok() {
  T_PASSED=$((T_PASSED + 1))
  printf 'ok - %s\n' "$*"
}

# t_object <path> <string...>: writes a synthetic object file holding each
# string between NUL bytes, among other bytes.
t_object() {
  local path=$1 text
  shift
  mkdir -p "$(dirname "$path")"
  printf '\177ELF\002\001\001\000' >"$path"
  for text in "$@"; do
    printf '\000\001%s\000' "$text" >>"$path"
  done
}

# t_expect <what> <violations> <message part> <function> <args...>: runs one
# audit function and checks the number of violations it recorded and, when
# the count is not zero, that its output names <message part>.
t_expect() {
  local what=$1 want=$2 part=$3
  shift 3
  CK_AUDIT_ERRORS=0
  "$@" 2>"$T_ROOT/stderr"
  [ "$CK_AUDIT_ERRORS" -eq "$want" ] || t_fail "$what: $CK_AUDIT_ERRORS violations, expected $want"
  if [ "$want" -gt 0 ]; then
    grep -q -F -- "$part" "$T_ROOT/stderr" || t_fail "$what: the output does not name '$part'"
  fi
  t_ok "$what"
}

# t_strings <what> <violations> <message part> <file name> <string...>
t_strings() {
  local what=$1 want=$2 part=$3 file="$T_ROOT/files/$4"
  shift 4
  rm -f "$file"
  t_object "$file" "$@"
  t_expect "$what" "$want" "$part" ck_audit_strings "$file"
}

# The baked strings as today's four archives hold them, in their contexts.
t_strings "allowed: every named baked string" 0 '' lib/libcrypto.so.3 \
  "compiler: gcc -ffile-prefix-map=$T_B/= -I$T_B/deps/current/include" \
  "-DENGINESDIR=\"\\\"$T_B/deps/current/lib/engines-3\\\"\"" \
  "$T_B/deps/current/lib/ossl-modules" \
  "$T_B/deps/legacy/lib/engines-1.1" \
  "file://$T_B/deps/legacy/etc/xml/catalog" \
  "'PKG_CONFIG=$T_B/deps/current/bin/pkgconf'" \
  "'PKG_CONFIG_LIBDIR=$T_B/deps/current/lib/pkgconfig:$T_B/deps/current/share/pkgconfig'" \
  "'--prefix=$T_B/php/8.5/prefix'" \
  "$T_B/php/7.4/prefix/bin" "$T_B/php/7.4/prefix/sbin" "$T_B/php/8.5/prefix/etc" \
  "$T_B/php/8.5/prefix/var" "$T_B/php/8.5/prefix/lib/php" "$T_B/php/8.5/prefix/php/man" \
  ".:$T_B/php/8.5/prefix/share/php" \
  "$T_B/php/8.5/prefix/lib/php/extensions/no-debug-non-zts-20250925" \
  "$T_B/php/7.4/prefix/lib/php/extensions/no-debug-non-zts-20190902"
t_strings "allowed: the null-folder strings" 0 '' bin/php \
  "$T_NULL/ssl" "$T_NULL/ssl/certs" "'--with-config-file-path=$T_NULL/php'" "$T_NULL/php.d" \
  /proc/ck-dev-none/ssl /System/ck-dev-none/php
t_strings "refused: OpenSSL's config folder in the deps prefix" 1 'start-up defaults' \
  lib/libcrypto.so.3 "OPENSSLDIR: \"$T_B/deps/current/ssl\""
t_strings "refused: a file in OpenSSL's old config folder" 1 "$T_B/deps/legacy/ssl/cert.pem" \
  lib/libcrypto.so.1.1 "$T_B/deps/legacy/ssl/cert.pem"
t_strings "refused: PHP's old ini folders" 1 "$T_B/php/8.5/no-ini.d" bin/php \
  "$T_B/php/8.5/no-ini" "$T_B/php/8.5/no-ini.d"
t_strings "refused: a home folder" 1 /home/runner bin/php "/home/runner/work/ckm-dev/src/main.c"
t_strings "refused: a build-root path not on the list" 1 "$T_B/deps/current/lib" \
  lib/libcurl.so.4 "-L$T_B/deps/current/lib"
t_strings "refused: a named string with more after it" 1 "$T_B/deps/current/include/openssl" \
  lib/libcrypto.so.3 "$T_B/deps/current/include/openssl"
t_strings "refused: an unknown stack" 1 "$T_B/deps/other/include" lib/libcrypto.so.3 \
  "$T_B/deps/other/include"
t_strings "refused: the build root without its slash" 1 "$T_B" lib/libcrypto.so.3 "$T_B"
t_strings "refused: another folder under /opt" 1 /opt/homebrew lib/libz.so.1 /opt/homebrew/lib
t_strings "allowed: libmagic's text paths in fileinfo.so" 0 '' ext/fileinfo.so \
  /usr/local /nix/store/
t_strings "refused: libmagic's text paths in another extension" 1 /usr/local ext/curl.so /usr/local
t_strings "refused: a home folder in fileinfo.so" 1 /home/runner ext/fileinfo.so \
  /usr/local "/home/runner/work"
t_strings "refused: an old default in fileinfo.so" 1 "$T_B/deps/current/ssl" ext/fileinfo.so \
  "$T_B/deps/current/ssl"

# ck_audit_null_dir on a root: one libcrypto with <null>/ssl, and bin/php
# with both ini strings, each whole.
T_GOOD="$T_ROOT/good"
t_object "$T_GOOD/lib/libcrypto.so.3" "OPENSSLDIR: \"$T_NULL/ssl\"" "$T_NULL/ssl/certs"
t_object "$T_GOOD/bin/php" "'--with-config-file-path=$T_NULL/php'" "$T_NULL/php.d"
t_expect "null folder: present" 0 '' ck_audit_null_dir "$T_GOOD"

T_OLD="$T_ROOT/old"
t_object "$T_OLD/lib/libcrypto.so.3" "OPENSSLDIR: \"$T_B/deps/current/ssl\""
t_object "$T_OLD/bin/php" "$T_B/php/8.5/no-ini" "$T_B/php/8.5/no-ini.d"
t_expect "null folder: missing everywhere" 3 "does not hold $T_NULL/ssl" ck_audit_null_dir "$T_OLD"

T_PART="$T_ROOT/part"
t_object "$T_PART/lib/libcrypto.so.3" "$T_NULL/ssl"
t_object "$T_PART/bin/php" "$T_NULL/php.d" "$T_NULL/php/extra"
t_expect "null folder: the ini path only as part of a longer string" 1 "does not hold $T_NULL/php" \
  ck_audit_null_dir "$T_PART"

T_NOSSL="$T_ROOT/nossl"
t_object "$T_NOSSL/bin/php" "$T_NULL/php" "$T_NULL/php.d"
mkdir -p "$T_NOSSL/lib"
t_expect "null folder: no libcrypto" 1 'expected one libcrypto' ck_audit_null_dir "$T_NOSSL"

if [ "$(ck_os)" = linux ]; then
  # Feed readelf-style output through the real ELF checks without a compiler
  # or a dependency on the self-test host's glibc version.
  t_linux_elf() (
    local version=$1 CK_AUDIT_ERRORS=0
    readelf() {
      case "$1" in
        -hW) printf '%s\n' '  Machine: Advanced Micro Devices X86-64' '  Type: DYN (Position-Independent Executable file)' ;;
        -dW)
          # shellcheck disable=SC2016 # $ORIGIN is literal readelf output
          printf '%s\n' '  0x1 (NEEDED) Shared library: [libc.so.6]' '  0xf (RPATH) Library rpath: [$ORIGIN/../lib]'
          ;;
        -lW) printf '%s\n' '  INTERP' '  GNU_RELRO' '  GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10' ;;
        -VW) printf '  0x0010: Name: GLIBC_%s  Flags: none  Version: 2\n' "$version" ;;
        *) return 1 ;;
      esac
    }
    ck_audit_linux_file "$T_ROOT/elf" "$T_ROOT/elf/bin/php" x86_64
    printf '%s\n' "$CK_AUDIT_ERRORS"
  )

  t_object "$T_ROOT/elf/bin/php"
  [ "$(t_linux_elf 2.39 2>"$T_ROOT/stderr")" = 0 ] || t_fail "GLIBC_2.39 must pass"
  t_ok "allowed: GLIBC_2.39"
  [ "$(t_linux_elf 2.40 2>"$T_ROOT/stderr")" = 1 ] || t_fail "GLIBC_2.40 must fail"
  grep -qF 'needs GLIBC_2.40, above the baseline 2.39' "$T_ROOT/stderr" ||
    t_fail "GLIBC_2.40: missing baseline diagnostic"
  t_ok "refused: GLIBC_2.40"
fi

printf 'audit-selftest: %s checks passed\n' "$T_PASSED"
