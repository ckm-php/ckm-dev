#!/usr/bin/env bash
# Case snippets are single-quoted on purpose: the child bash expands them.
# shellcheck disable=SC2016
# T5: the pinned fetch and unpack rules of lib/fetch.sh, and the pure checks
# of refresh-pins.sh, with no network. A stub `curl` serves local files and
# a stub ck-release prints canned pins. Every case runs in a child bash, so
# `set -e` behaves as it does in the build scripts.
#
# Usage: build/php/tests/fetch-selftest.sh

PHP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
# shellcheck source=../lib/common.sh
. "$PHP_DIR/lib/common.sh"
# shellcheck source=../lib/fetch.sh
. "$PHP_DIR/lib/fetch.sh"

T_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/ck-fetch-selftest.XXXXXX")
T_ROOT=$(cd "$T_ROOT" && pwd -P)
trap 'rm -rf -- "$T_ROOT"' EXIT

T_BIN="$T_ROOT/bin"
T_SERVE="$T_ROOT/serve"
T_LOG="$T_ROOT/curl.log"
mkdir "$T_BIN" "$T_SERVE"
: >"$T_LOG"
export CK_CACHE_DIR="$T_ROOT/cache"
export T_SERVE T_LOG
T_PASSED=0
T_PINS=
export T_PINS

# A curl stand-in: checks the flags ck_download must pass, then copies
# $T_SERVE/<last URL segment> to the -o file, or fails like `curl -f`.
cat >"$T_BIN/curl" <<'EOF'
#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >>"$T_LOG"
[ "$1" = -q ] || { echo "stub curl: -q is not first" >&2; exit 90; }
case " $* " in *" --proto =https "*) ;; *) echo "stub curl: no --proto =https" >&2; exit 90 ;; esac
case " $* " in *" --proto-redir =https "*) ;; *) echo "stub curl: no --proto-redir" >&2; exit 90 ;; esac
out=
while [ "$#" -gt 1 ]; do
  if [ "$1" = -o ]; then out=$2; fi
  shift
done
url=$1
[ -n "$out" ] || { echo "stub curl: no -o" >&2; exit 90; }
[ -f "$T_SERVE/${url##*/}" ] || exit 22
cp "$T_SERVE/${url##*/}" "$out"
EOF
chmod 755 "$T_BIN/curl"

t_fail() {
  printf 'fetch-selftest: FAIL: %s\n' "$*" >&2
  if [ -n "${T_ERR:-}" ]; then
    printf '  stderr of the case:\n%s\n' "$T_ERR" >&2
  fi
  exit 1
}

t_ok() {
  T_PASSED=$((T_PASSED + 1))
  printf 'ok - %s\n' "$*"
}

# t_sh <library> <snippet> [args...]: runs the snippet in a child bash that
# sourced common.sh and <library> (fetch.sh or refresh-pins.sh), with the stub
# curl first on PATH. Sets T_STATUS, T_OUT and T_ERR.
t_sh() {
  local lib=$1 snippet=$2
  shift 2
  T_STATUS=0
  T_OUT=$(PATH="$T_BIN:$PATH" bash -c \
    '. "$0"; . "$1"; CK_PINS=$T_PINS; s=$2; shift 3; eval "$s"' \
    "$PHP_DIR/lib/common.sh" "$lib" "$snippet" _ "$@" 2>"$T_ROOT/stderr") || T_STATUS=$?
  T_ERR=$(cat "$T_ROOT/stderr")
}

t_expect_ok() {
  [ "$T_STATUS" -eq 0 ] || t_fail "$1: exit $T_STATUS, expected 0"
}

# t_expect_fail <case> <text the error must hold>
t_expect_fail() {
  [ "$T_STATUS" -ne 0 ] || t_fail "$1: succeeded, expected a failure"
  case "$T_ERR" in
    *"$2"*) ;;
    *) t_fail "$1: the error does not mention '$2'" ;;
  esac
}

t_curl_calls() {
  wc -l <"$T_LOG" | tr -d ' \t'
}

FETCH="$PHP_DIR/lib/fetch.sh"
REFRESH="$PHP_DIR/refresh-pins.sh"

# Fixtures.
printf 'pinned bytes\n' >"$T_SERVE/demo-1.0.txt"
GOOD_SHA=$(ck_sha256 "$T_SERVE/demo-1.0.txt")
GOOD_SIZE=$(ck_size "$T_SERVE/demo-1.0.txt")
WRONG_SHA=$(printf '%064d' 0)
URL=https://github.com/example/demo/releases/download/v1.0/demo-1.0.txt
CACHED="$CK_CACHE_DIR/demo-1.0/demo-1.0.txt"

place_cached() {
  mkdir -p "$CK_CACHE_DIR/demo-1.0"
  cp "$T_SERVE/demo-1.0.txt" "$CACHED"
}

# --- ck_fetch -------------------------------------------------------------

place_cached
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" "$GOOD_SIZE" demo-1.0.txt
t_expect_ok "cached file"
[ "$T_OUT" = "$CACHED" ] || t_fail "cached file: printed '$T_OUT'"
[ "$(t_curl_calls)" = 0 ] || t_fail "cached file: curl was called"
t_ok "a cached file that matches is used without a download"

t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$WRONG_SHA" "$GOOD_SIZE" demo-1.0.txt
t_expect_fail "cached wrong SHA256" "SHA256 mismatch"
[ ! -e "$CACHED" ] || t_fail "cached wrong SHA256: the file was not deleted"
[ "$(t_curl_calls)" = 0 ] || t_fail "cached wrong SHA256: curl was called"
t_ok "a cached file with a wrong SHA256 is deleted and fails, with no new download"

place_cached
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" $((GOOD_SIZE + 1)) demo-1.0.txt
t_expect_fail "cached wrong size" "size mismatch"
[ ! -e "$CACHED" ] || t_fail "cached wrong size: the file was not deleted"
t_ok "a cached file with a wrong size is deleted and fails"

t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" "$GOOD_SIZE" demo-1.0.txt
t_expect_ok "download"
[ "$T_OUT" = "$CACHED" ] || t_fail "download: printed '$T_OUT'"
cmp -s "$CACHED" "$T_SERVE/demo-1.0.txt" || t_fail "download: wrong contents"
[ "$(t_curl_calls)" = 1 ] || t_fail "download: expected one curl call"
[ "$(ls -A "$CK_CACHE_DIR/demo-1.0")" = demo-1.0.txt ] || t_fail "download: a temporary file was left"
t_ok "a download that matches is moved into the cache"

rm -f "$CACHED"
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$WRONG_SHA" "$GOOD_SIZE" demo-1.0.txt
t_expect_fail "download wrong SHA256" "SHA256 mismatch"
[ -z "$(ls -A "$CK_CACHE_DIR/demo-1.0")" ] || t_fail "download wrong SHA256: a file was left"
t_ok "a download with a wrong SHA256 is deleted and fails"

t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" $((GOOD_SIZE - 1)) demo-1.0.txt
t_expect_fail "download wrong size" "size mismatch"
[ -z "$(ls -A "$CK_CACHE_DIR/demo-1.0")" ] || t_fail "download wrong size: a file was left"
t_ok "a download with a wrong size is deleted and fails"

calls=$(t_curl_calls)
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "http://github.com/x/demo-1.0.txt" "$GOOD_SHA" "$GOOD_SIZE" demo-1.0.txt
t_expect_fail "plain HTTP" "not HTTPS"
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" "$GOOD_SIZE" ../demo-1.0.txt
t_expect_fail "unsafe file name" "not a safe file name"
t_sh "$FETCH" 'ck_fetch "$@"' demo 1.0 "$URL" "$GOOD_SHA" "$GOOD_SIZE" other.txt
t_expect_fail "file name not in URL" "does not end in /other.txt"
[ "$(t_curl_calls)" = "$calls" ] || t_fail "bad pin fields: curl was called"
t_ok "plain HTTP, unsafe file names and mismatched URLs are refused before any download"

# --- ck_pins_load and ck_pin ----------------------------------------------

TAB=$'\t'
pin_line() { # <name> <version> <file> <sha> <size>
  printf '%s\t%s\thttps://github.com/example/%s/releases/download/v%s/%s\t%s\t%s\tMIT\tnone\t%s' \
    "$1" "$2" "$1" "$2" "$3" "$4" "$5" "$3"
}
T_RELEASE_LOG="$T_ROOT/release.log"
: >"$T_RELEASE_LOG"
cat >"$T_BIN/ck-release" <<EOF
#!/usr/bin/env bash
case "\$1 \$2" in
  "lock check") exit 0 ;;
  "lock set") printf '%s\n' "\$*" >>"$T_RELEASE_LOG"; exit 0 ;;
esac
case "\$CK_TEST_RELEASE" in
  two) printf '%s\n%s\n' "$(pin_line demo 1.0 demo-1.0.txt "$GOOD_SHA" "$GOOD_SIZE")" "$(pin_line other 2.0 other-2.0.tar.gz "$GOOD_SHA" 1)" ;;
  empty) ;;
  fail) echo "ck-release: no pins" >&2; exit 1 ;;
esac
EOF
chmod 755 "$T_BIN/ck-release"
export CK_RELEASE_BIN="$T_BIN/ck-release"

CK_TEST_RELEASE=two t_sh "$FETCH" 'ck_pins_load --stack current && ck_pin demo'
t_expect_ok "load and pick a pin"
[ "$T_OUT" = "$(pin_line demo 1.0 demo-1.0.txt "$GOOD_SHA" "$GOOD_SIZE")" ] || t_fail "ck_pin printed '$T_OUT'"
CK_TEST_RELEASE=empty t_sh "$FETCH" 'ck_pins_load --stack current'
t_expect_fail "empty export" "printed no pins"
CK_TEST_RELEASE=fail t_sh "$FETCH" 'ck_pins_load --stack current'
t_expect_fail "failed export" "lock export --stack current failed"
CK_TEST_RELEASE=two t_sh "$FETCH" 'ck_pins_load --stack current && ck_pin missing'
t_expect_fail "missing pin" "no pin for missing"
T_PINS="$(pin_line demo 1.0 a "$GOOD_SHA" 1)"$'\n'"$(pin_line demo 1.1 b "$GOOD_SHA" 1)"
t_sh "$FETCH" 'ck_pin demo'
t_expect_fail "duplicate pin" "2 pins for demo"
T_PINS="demo${TAB}1.0${TAB}https://github.com/x/a${TAB}${GOOD_SHA}${TAB}1${TAB}MIT${TAB}none"
t_sh "$FETCH" 'ck_pin demo'
t_expect_fail "seven fields" "has 7 fields"
T_PINS="demo${TAB}1.0${TAB}${TAB}https://github.com/x/a${TAB}${GOOD_SHA}${TAB}1${TAB}MIT${TAB}a"
t_sh "$FETCH" 'ck_pin demo'
t_expect_fail "empty field" "empty field"
T_PINS=
t_ok "ck_pins_load and ck_pin take one well-formed line and refuse the rest"

# --- ck_unpack ------------------------------------------------------------

SRC="$T_ROOT/src"
OUT="$T_ROOT/out"
mkdir -p "$SRC/top/sub" "$OUT"
printf 'a\n' >"$SRC/top/a.txt"
printf 'b\n' >"$SRC/top/sub/b.txt"
ln -s a.txt "$SRC/top/link-in"
ln -s ../a.txt "$SRC/top/sub/link-up"
printf 'outside\n' >"$SRC/esc"
tar -czf "$T_ROOT/good.tar.gz" -C "$SRC" top

t_sh "$FETCH" 'ck_unpack "$@"' "$T_ROOT/good.tar.gz" "$OUT/good"
t_expect_ok "good archive"
if [ ! -f "$OUT/good/a.txt" ] || [ ! -f "$OUT/good/sub/b.txt" ] || [ ! -L "$OUT/good/sub/link-up" ]; then
  t_fail "good archive: tree not unpacked as expected"
fi
[ -z "$(find "$OUT" -name '.ck-unpack.*')" ] || t_fail "good archive: staging folder left"
t_ok "a well-formed archive unpacks with its top folder as the destination"

t_sh "$FETCH" 'ck_unpack "$@"' "$T_ROOT/good.tar.gz" "$OUT/good"
t_expect_fail "existing destination" "already exists"
t_ok "an existing destination is refused"

# expect_refused <case> <archive> <error text>
expect_refused() {
  t_sh "$FETCH" 'ck_unpack "$@"' "$2" "$OUT/refused"
  t_expect_fail "$1" "$3"
  [ ! -e "$OUT/refused" ] || t_fail "$1: the destination was created"
  [ -z "$(find "$OUT" -name '.ck-unpack.*')" ] || t_fail "$1: staging folder left"
  if [ -e "$OUT/esc" ] || [ -e "$T_ROOT/esc" ]; then
    t_fail "$1: a file escaped"
  fi
}

tar -czPf "$T_ROOT/dotdot.tar.gz" -C "$SRC" top/a.txt top/../esc 2>/dev/null
case "$(tar -tzf "$T_ROOT/dotdot.tar.gz" 2>/dev/null)" in
  *../esc*) ;;
  *) t_fail "fixture: this tar did not store a '..' member" ;;
esac
expect_refused "'..' member" "$T_ROOT/dotdot.tar.gz" "member path with '..'"
t_ok "an archive with a '..' member is refused before anything is written"

mkdir "$T_ROOT/abs"
printf 'x\n' >"$T_ROOT/abs/file"
tar -czPf "$T_ROOT/absolute.tar.gz" "$T_ROOT/abs/file" 2>/dev/null
case "$(tar -tzf "$T_ROOT/absolute.tar.gz" 2>/dev/null)" in
  /*) ;;
  *) t_fail "fixture: this tar did not store an absolute member" ;;
esac
expect_refused "absolute member" "$T_ROOT/absolute.tar.gz" "absolute member path"
t_ok "an archive with an absolute member is refused"

mkdir "$SRC/second"
tar -czf "$T_ROOT/two-tops.tar.gz" -C "$SRC" top second
expect_refused "two top folders" "$T_ROOT/two-tops.tar.gz" "more than one top-level entry"
t_ok "an archive with two top-level entries is refused"

mkdir -p "$T_ROOT/evil/top"
ln -s ../../esc "$T_ROOT/evil/top/out"
tar -czf "$T_ROOT/link-out.tar.gz" -C "$T_ROOT/evil" top
expect_refused "link outside" "$T_ROOT/link-out.tar.gz" "dangling or points outside"
mkdir -p "$T_ROOT/evil2/top"
ln -s / "$T_ROOT/evil2/top/root"
tar -czf "$T_ROOT/link-abs.tar.gz" -C "$T_ROOT/evil2" top
expect_refused "absolute link" "$T_ROOT/link-abs.tar.gz" "absolute link"
mkdir -p "$T_ROOT/evil3/top"
ln -s missing "$T_ROOT/evil3/top/dangling"
tar -czf "$T_ROOT/link-dangling.tar.gz" -C "$T_ROOT/evil3" top
expect_refused "dangling link" "$T_ROOT/link-dangling.tar.gz" "dangling or points outside"
t_ok "links that are absolute, dangling or point outside the tree are refused"

# --- ck_fetch_unpack --------------------------------------------------------

cp "$T_ROOT/good.tar.gz" "$T_SERVE/pkg-3.0.tar.gz"
PKG_SHA=$(ck_sha256 "$T_SERVE/pkg-3.0.tar.gz")
PKG_SIZE=$(ck_size "$T_SERVE/pkg-3.0.tar.gz")
T_PINS=$(pin_line pkg 3.0 pkg-3.0.tar.gz "$PKG_SHA" "$PKG_SIZE")
t_sh "$FETCH" 'ck_fetch_unpack pkg "$1"' "$OUT/stack"
t_expect_ok "fetch and unpack"
if [ "$T_OUT" != "$OUT/stack/pkg-3.0" ] || [ ! -f "$OUT/stack/pkg-3.0/sub/b.txt" ]; then
  t_fail "fetch and unpack: printed '$T_OUT'"
fi
calls=$(t_curl_calls)
t_sh "$FETCH" 'ck_fetch_unpack pkg "$1"' "$OUT/stack"
t_expect_ok "reuse"
if [ "$T_OUT" != "$OUT/stack/pkg-3.0" ] || [ "$(t_curl_calls)" != "$calls" ]; then
  t_fail "reuse: downloaded again"
fi
T_PINS=
t_ok "ck_fetch_unpack fetches, unpacks as <name>-<version> and reuses the folder"

# --- refresh-pins.sh checks -----------------------------------------------

UPPER=$(printf '%s' "$GOOD_SHA" | tr 'a-f' 'A-F')
printf '%s\n' "$GOOD_SHA" >"$T_ROOT/sum-bare"
printf '%s  other.tar.xz\n%s  demo-1.0.txt\n' "$WRONG_SHA" "$UPPER" >"$T_ROOT/sum-multi"
printf '%s *demo-1.0.txt\r\n' "$GOOD_SHA" >"$T_ROOT/sum-star"
printf 'SHA256 (demo-1.0.txt) = %s\n' "$GOOD_SHA" >"$T_ROOT/sum-bsd"
# OpenSSL 1.1.1's .sha256 assets: one bare hash after a space.
printf ' %s\n' "$GOOD_SHA" >"$T_ROOT/sum-lead"
for sum in sum-bare sum-multi sum-star sum-bsd sum-lead; do
  t_sh "$REFRESH" 'ck_sumfile_hash "$@"' "$T_ROOT/$sum" demo-1.0.txt
  t_expect_ok "checksum file $sum"
  [ "$T_OUT" = "$GOOD_SHA" ] || t_fail "checksum file $sum: printed '$T_OUT'"
done
printf '%s  other.tar.xz\n' "$GOOD_SHA" >"$T_ROOT/sum-none"
t_sh "$REFRESH" 'ck_sumfile_hash "$@"' "$T_ROOT/sum-none" demo-1.0.txt
t_expect_fail "checksum file without the file" "no SHA256 for demo-1.0.txt"
printf '%s  demo-1.0.txt\n%s  demo-1.0.txt\n' "$GOOD_SHA" "$WRONG_SHA" >"$T_ROOT/sum-two"
t_sh "$REFRESH" 'ck_sumfile_hash "$@"' "$T_ROOT/sum-two" demo-1.0.txt
t_expect_fail "checksum file with two hashes" "expected one"
t_ok "checksum files: one hash per file name, in each upstream format"

SIGNER=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
PRIMARY=0123456789ABCDEF0123456789ABCDEF01234567
printf '# Only comments: no fingerprint listed yet.\n\n' >"$T_ROOT/placeholder.fingerprints"
t_sh "$REFRESH" 'ck_allowlist_load "$@"' "$T_ROOT/placeholder.fingerprints"
t_expect_fail "placeholder allow-list" "lists no fingerprint"
printf '# owner\n0123 4567 89ab cdef 0123  4567 89AB CDEF 0123 4567  # someone\n' >"$T_ROOT/allow"
printf 'not-a-fingerprint\n' >"$T_ROOT/allow-bad"
t_sh "$REFRESH" 'ck_allowlist_load "$@"' "$T_ROOT/allow-bad"
t_expect_fail "malformed allow-list" "not a 40-hex-digit fingerprint"
t_sh "$REFRESH" 'ck_allowlist_load "$@"' "$T_ROOT/allow"
t_expect_ok "allow-list"
[ "$T_OUT" = "$PRIMARY" ] || t_fail "allow-list: printed '$T_OUT'"

validsig() { # <signer> [primary]
  printf '[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG %s someone\n' "${1: -16}"
  printf '[GNUPG:] VALIDSIG %s 2026-11-01 1793000000 0 4 0 22 10 00 %s\n' "$1" "${2:-}"
}
validsig "$SIGNER" "$PRIMARY" >"$T_ROOT/status-sub"
t_sh "$REFRESH" 'ck_gpg_check_status "$@"' "$T_ROOT/status-sub" "$T_ROOT/allow"
t_expect_ok "subkey signature with an allowed primary"
[ "$T_OUT" = "$PRIMARY" ] || t_fail "status: printed '$T_OUT'"
validsig "$SIGNER" "$SIGNER" >"$T_ROOT/status-other"
t_sh "$REFRESH" 'ck_gpg_check_status "$@"' "$T_ROOT/status-other" "$T_ROOT/allow"
t_expect_fail "signature by a key not on the list" "no valid signature from a key on the allow-list"
{
  validsig "$SIGNER" "$PRIMARY"
  printf '[GNUPG:] BADSIG %s someone\n' "${SIGNER: -16}"
} >"$T_ROOT/status-bad"
t_sh "$REFRESH" 'ck_gpg_check_status "$@"' "$T_ROOT/status-bad" "$T_ROOT/allow"
t_expect_fail "a bad signature next to a good one" "gpgv reported: BADSIG"
{
  printf '[GNUPG:] PLAINTEXT 62 1793000000 old.txt\n'
  validsig "$SIGNER" "$PRIMARY"
} >"$T_ROOT/status-inline"
t_sh "$REFRESH" 'ck_gpg_check_status "$@"' "$T_ROOT/status-inline" "$T_ROOT/allow"
t_expect_fail "an inline-signed message" "expected a detached signature"
t_ok "allow-lists and gpgv status: an allowed key must sign a detached signature, and any bad signature fails"

printf '# why\nNo key on an approved host;  ask the owner.  # note\n' >"$T_ROOT/c.unsigned"
t_sh "$REFRESH" 'ck_unsigned_reason "$@"' "$T_ROOT/c.unsigned" "$T_ROOT/c.gpg"
t_expect_ok "unsigned note"
[ "$T_OUT" = "No key on an approved host; ask the owner." ] || t_fail "unsigned note: printed '$T_OUT'"
printf '# only a comment\n' >"$T_ROOT/empty.unsigned"
t_sh "$REFRESH" 'ck_unsigned_reason "$@"' "$T_ROOT/empty.unsigned" "$T_ROOT/c.gpg"
t_expect_fail "unsigned note without a reason" "gives no reason"
: >"$T_ROOT/c.gpg"
t_sh "$REFRESH" 'ck_unsigned_reason "$@"' "$T_ROOT/c.unsigned" "$T_ROOT/c.gpg"
t_expect_fail "unsigned note next to a keyring" "remove one"
t_ok "an unsigned note needs a reason and no keyring beside it"

if command -v jq >/dev/null 2>&1; then
  printf '{"version":"8.5.12","source":[{"filename":"php-8.5.12.tar.gz","sha256":"%s"},{"filename":"php-8.5.12.tar.xz","sha256":"%s"}]}' \
    "$WRONG_SHA" "$GOOD_SHA" >"$T_ROOT/release.json"
  t_sh "$REFRESH" 'ck_php_json_hash "$@"' "$T_ROOT/release.json" php-8.5.12.tar.xz
  t_expect_ok "php.net JSON"
  [ "$T_OUT" = "$GOOD_SHA" ] || t_fail "php.net JSON: printed '$T_OUT'"
  t_sh "$REFRESH" 'ck_php_json_hash "$@"' "$T_ROOT/release.json" php-8.5.12.tar.bz2
  t_expect_fail "php.net JSON without the file" "no single sha256"
  t_ok "php.net's release JSON gives the hash of the named file"
else
  printf 'skip - php.net JSON check (jq is not installed)\n'
fi

T_STATUS=0
T_ERR=$(CI=true bash "$REFRESH" php 8.5.12 2>&1 >/dev/null) || T_STATUS=$?
t_expect_fail "refresh in CI" "does not run in CI"
t_ok "refresh-pins.sh refuses to run in CI"

# One whole refresh with the stubs: a matching .sha256 asset reaches
# `lock set` as sha256-file; a mismatch never calls it. CI is unset for the
# child only, since every tool it calls here is a stub.
ONIG=onig-6.9.10.tar.gz
ONIG_URL=https://github.com/kkos/oniguruma/releases/download/v6.9.10/$ONIG
cp "$T_ROOT/good.tar.gz" "$T_SERVE/$ONIG"
ONIG_SHA=$(ck_sha256 "$T_SERVE/$ONIG")
ONIG_SIZE=$(ck_size "$T_SERVE/$ONIG")
refresh_onig() {
  T_STATUS=0
  T_OUT=$(env -u CI -u GITHUB_ACTIONS PATH="$T_BIN:$PATH" TMPDIR="$T_ROOT" \
    bash "$REFRESH" oniguruma 6.9.10 2>"$T_ROOT/stderr") || T_STATUS=$?
  T_ERR=$(cat "$T_ROOT/stderr")
}
printf '%s  %s\n' "$ONIG_SHA" "$ONIG" >"$T_SERVE/$ONIG.sha256"
refresh_onig
t_expect_ok "refresh oniguruma"
[ "$(cat "$T_RELEASE_LOG")" = "lock set oniguruma 6.9.10 --url $ONIG_URL --sha256 $ONIG_SHA --size $ONIG_SIZE --license BSD-2-Clause --signature sha256-file" ] ||
  t_fail "refresh oniguruma: lock set was called as '$(cat "$T_RELEASE_LOG")'"
case "$T_OUT" in
  *"checked: sha256 matches $ONIG_URL.sha256"*) ;;
  *) t_fail "refresh oniguruma: the report does not name the checksum file" ;;
esac
: >"$T_RELEASE_LOG"
printf '%s  %s\n' "$WRONG_SHA" "$ONIG" >"$T_SERVE/$ONIG.sha256"
refresh_onig
t_expect_fail "refresh with a wrong .sha256" ".sha256 gives $WRONG_SHA"
[ ! -s "$T_RELEASE_LOG" ] || t_fail "refresh with a wrong .sha256: lock set was called"
[ -z "$(find "$T_ROOT" -maxdepth 1 -name 'ck-refresh.*')" ] || t_fail "refresh: temporary folder left"
t_ok "a refresh records a pin only after its second channel matches"

printf 'fetch-selftest: %d checks passed\n' "$T_PASSED"
