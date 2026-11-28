#!/usr/bin/env bash
# Usage: refresh-pins.sh <name> <version> [--url <url>]
#
# Records one pin in catalog/sources.lock.json, and only when a person asks
# for it: the build never refreshes pins, and this script refuses to run in
# CI. It downloads the file into a new empty temporary folder, measures its
# size and SHA256, checks every upstream channel the component has (a second
# published hash, a GPG or minisign signature), prints what it checked, and
# writes the pin with `ck-release lock set`. A check that exists for the
# component and fails, or cannot be fetched, stops the script.
#
# GPG signatures are checked with gpgv in a temporary home against the
# keyring and fingerprint allow-list in build/php/keys/, so the user's own
# keyring is never read or changed. A GPG component whose key is not
# published on an approved host has a tracked keys/<component>.unsigned note
# with the reason instead of a keyring: its signature is skipped with a
# warning, and the pin records only what was checked.
#
# Needs: CK_RELEASE_BIN (the absolute path of ck-release), curl, gpgv, gpg
# (only to de-armour a keyring), jq (only for php), minisign (optional, for
# libsodium).

CK_REFRESH_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib/common.sh
. "$CK_REFRESH_DIR/lib/common.sh"
# shellcheck source=lib/fetch.sh
. "$CK_REFRESH_DIR/lib/fetch.sh"

readonly CK_KEYS_DIR="$CK_REFRESH_DIR/keys"
# The lock's own cap, 2 GiB.
readonly CK_REFRESH_MAX_BYTES=2147483648

ck_refresh_usage() {
  cat >&2 <<'EOF'
usage: refresh-pins.sh <name> <version> [--url <url>]

names: php openssl icu curl libxml2 sqlite zlib oniguruma libsodium cacert
       pkgconf patchelf
--url is required for sqlite and pkgconf, and replaces the known URL for
any other name.
EOF
  exit 2
}

# ck_refresh_url <name> <version>: prints the known upstream URL. These are
# templates read from the upstream release pages; a wrong one fails on the
# download, and --url replaces it.
ck_refresh_url() {
  local name=$1 v=$2 gh=https://github.com
  case "$name" in
    php) echo "https://www.php.net/distributions/php-$v.tar.xz" ;;
    openssl)
      case "$v" in
        1.1.1*) echo "$gh/openssl/openssl/releases/download/OpenSSL_${v//./_}/openssl-$v.tar.gz" ;;
        *) echo "$gh/openssl/openssl/releases/download/openssl-$v/openssl-$v.tar.gz" ;;
      esac
      ;;
    icu)
      # ICU renamed its release tags and source asset in 78.
      if [ "${v%%.*}" -ge 78 ] 2>/dev/null; then
        echo "$gh/unicode-org/icu/releases/download/release-$v/icu4c-$v-sources.tgz"
      else
        echo "$gh/unicode-org/icu/releases/download/release-${v//./-}/icu4c-${v//./_}-src.tgz"
      fi
      ;;
    curl) echo "$gh/curl/curl/releases/download/curl-${v//./_}/curl-$v.tar.xz" ;;
    libxml2) echo "https://download.gnome.org/sources/libxml2/${v%.*}/libxml2-$v.tar.xz" ;;
    zlib) echo "$gh/madler/zlib/releases/download/v$v/zlib-$v.tar.gz" ;;
    oniguruma) echo "$gh/kkos/oniguruma/releases/download/v$v/onig-$v.tar.gz" ;;
    libsodium) echo "$gh/jedisct1/libsodium/releases/download/$v-RELEASE/libsodium-$v.tar.gz" ;;
    cacert) echo "https://curl.se/ca/cacert-$v.pem" ;;
    patchelf) echo "$gh/NixOS/patchelf/releases/download/$v/patchelf-$v.tar.gz" ;;
    sqlite | pkgconf) ck_die "$name has no known URL; pass --url" ;;
    *) ck_die "unknown component: $name" ;;
  esac
}

# ck_refresh_license <name> <version>: prints the component's SPDX licence.
ck_refresh_license() {
  case "$1" in
    php) echo PHP-3.01 ;;
    openssl)
      case "$2" in
        1.*) echo OpenSSL ;;
        3.*) echo Apache-2.0 ;;
        *) ck_die "no licence known for openssl $2" ;;
      esac
      ;;
    icu) echo Unicode-3.0 ;;
    curl) echo curl ;;
    libxml2) echo MIT ;;
    sqlite) echo blessing ;;
    zlib) echo Zlib ;;
    oniguruma) echo BSD-2-Clause ;;
    libsodium) echo ISC ;;
    cacert) echo MPL-2.0 ;;
    pkgconf) echo ISC ;;
    patchelf) echo GPL-3.0-or-later ;;
    *) ck_die "unknown component: $1" ;;
  esac
}

# ck_refresh_check_host <url>: dies unless the URL is HTTPS on an approved
# host (the list ck-release enforces for pins), with no user or port.
ck_refresh_check_host() {
  local url=$1 rest host
  case "$url" in
    https://*) rest=${url#https://} ;;
    *) ck_die "not an HTTPS URL: $url" ;;
  esac
  host=${rest%%/*}
  case "$host" in
    www.php.net | github.com | download.gnome.org | www.sqlite.org | curl.se | \
      downloads.php.net | git.almalinux.org | repo.almalinux.org | \
      security.ubuntu.com | archive.ubuntu.com) ;;
    *) ck_die "host not approved for pins: $host ($url)" ;;
  esac
  [ "$rest" != "$host" ] || ck_die "the URL has no path: $url"
}

# ck_sumfile_hash <sum file> <file name>: prints the one SHA256 the checksum
# file gives for <file name>, in lower case. Reads `<hex>` alone,
# `<hex>  <name>`, `<hex> *<name>` and `SHA256 (<name>) = <hex>` lines,
# after any leading blanks. Dies unless exactly one hash matches.
ck_sumfile_hash() {
  local sumfile=$1 want=$2 line hash name found='' count=0 bare=0 ws=$' \t'
  local hex_re='^[0-9A-Fa-f]{64}$'
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%$'\r'}
    # shellcheck disable=SC2295 # $ws is meant as a character class
    line=${line#"${line%%[!$ws]*}"}
    [ -n "$line" ] || continue
    hash='' name=''
    case "$line" in
      'SHA256 ('*') = '*)
        name=${line#"SHA256 ("}
        name=${name%%") = "*}
        hash=${line##*") = "}
        ;;
      *)
        # shellcheck disable=SC2295 # $ws is meant as a character class
        hash=${line%%[$ws]*}
        if [ "$hash" != "$line" ]; then
          name=${line#"$hash"}
          # shellcheck disable=SC2295 # $ws is meant as a character class
          name=${name#"${name%%[!$ws]*}"}
          name=${name#\*}
        fi
        ;;
    esac
    [[ $hash =~ $hex_re ]] || continue
    if [ -z "$name" ]; then
      bare=$((bare + 1))
      found=$hash
      count=$((count + 1))
    elif [ "$name" = "$want" ]; then
      found=$hash
      count=$((count + 1))
    fi
  done <"$sumfile"
  [ "$bare" -eq 0 ] || [ "$count" -eq 1 ] || ck_die "$sumfile: more than one hash without a file name"
  [ "$count" -gt 0 ] || ck_die "$sumfile: no SHA256 for $want"
  [ "$count" -eq 1 ] || ck_die "$sumfile: $count SHA256 lines for $want; expected one"
  printf '%s\n' "$found" | tr 'A-F' 'a-f'
}

# ck_allowlist_load <file>: prints the allow-list's fingerprints, one per
# line in upper case. A line is a 40-hex-digit fingerprint, spaces allowed
# inside it; `#` starts a comment. Dies on any other line or an empty list.
ck_allowlist_load() {
  local file=$1 line fpr out='' re='^[0-9A-F]{40}$'
  [ -f "$file" ] || ck_die "missing fingerprint allow-list: $file"
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%%#*}
    fpr=$(printf '%s' "$line" | tr -d ' \t\r' | tr 'a-f' 'A-F')
    [ -n "$fpr" ] || continue
    [[ $fpr =~ $re ]] || ck_die "$file: not a 40-hex-digit fingerprint: $line"
    out="$out$fpr"$'\n'
  done <"$file"
  [ -n "$out" ] || ck_die "$file lists no fingerprint; fill it from the component's upstream key page"
  printf '%s' "$out"
}

# ck_in_list <word> <newline-separated list>: true when the word is a line.
ck_in_list() {
  local nl=$'\n'
  case "$nl$2$nl" in
    *"$nl$1$nl"*) return 0 ;;
  esac
  return 1
}

# ck_gpg_check_status <status file> <allow-list file>: reads gpgv's
# --status-fd output and prints the allowed fingerprint that made a valid
# signature. Per GnuPG's DETAILS, VALIDSIG's first argument is the signing
# (sub)key's fingerprint and its tenth, when present, the primary key's;
# either may be on the allow-list. Dies on BADSIG, ERRSIG, EXPSIG or
# REVKEYSIG, or when no valid signature comes from an allowed key. Also dies
# on PLAINTEXT: a detached signature has no embedded data, so that line means
# the .asc held its own signed message and the download was never covered.
ck_gpg_check_status() {
  local status=$1 allow signer primary line matched=
  allow=$(ck_allowlist_load "$2") || exit 1
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      '[GNUPG:] BADSIG '* | '[GNUPG:] ERRSIG '* | '[GNUPG:] EXPSIG '* | '[GNUPG:] REVKEYSIG '*)
        ck_die "gpgv reported: ${line#\[GNUPG:\] }"
        ;;
      '[GNUPG:] PLAINTEXT '* | '[GNUPG:] PLAINTEXT_LENGTH '*)
        ck_die "the signature file embeds signed data; expected a detached signature"
        ;;
      '[GNUPG:] VALIDSIG '*)
        primary=
        read -r _ _ signer _ _ _ _ _ _ _ _ primary _ <<<"$line"
        signer=$(printf '%s' "$signer" | tr 'a-f' 'A-F')
        primary=$(printf '%s' "$primary" | tr 'a-f' 'A-F')
        if [ -n "$signer" ] && ck_in_list "$signer" "$allow"; then
          matched=$signer
        elif [ -n "$primary" ] && ck_in_list "$primary" "$allow"; then
          matched=$primary
        else
          ck_log "valid signature by a key not on the allow-list: $signer${primary:+ (primary $primary)}"
        fi
        ;;
    esac
  done <"$status"
  [ -n "$matched" ] || ck_die "no valid signature from a key on the allow-list $2"
  printf '%s\n' "$matched"
}

# ck_gpg_verify <component> <signature> <file> <work dir>: checks a detached
# signature with gpgv in a fresh home under <work dir>, against
# keys/<component>.gpg and keys/<component>.fingerprints. Prints the allowed
# fingerprint that signed.
ck_gpg_verify() {
  local component=$1 sig=$2 file=$3 work=$4 keyring home status
  keyring="$CK_KEYS_DIR/$component.gpg"
  [ -f "$keyring" ] || ck_die "missing keyring $keyring; fetch it from the component's upstream key page"
  ck_allowlist_load "$CK_KEYS_DIR/$component.fingerprints" >/dev/null || exit 1
  home="$work/gnupg"
  mkdir -m 700 "$home" || ck_die "cannot create $home"
  # gpgv reads binary keyrings only.
  if LC_ALL=C grep -q -a -- '-----BEGIN PGP PUBLIC KEY BLOCK-----' "$keyring"; then
    ck_require_cmd gpg
    gpg --homedir "$home" --batch --no-autostart --dearmor <"$keyring" >"$work/keyring.gpg" ||
      ck_die "cannot de-armour $keyring"
    keyring="$work/keyring.gpg"
  fi
  status="$work/gpgv.status"
  if ! gpgv --homedir "$home" --keyring "$keyring" --status-fd 3 -- "$sig" "$file" \
    3>"$status" 2>"$work/gpgv.log"; then
    cat "$work/gpgv.log" >&2
    ck_die "gpgv rejected the signature on $(basename -- "$file")"
  fi
  ck_gpg_check_status "$status" "$CK_KEYS_DIR/$component.fingerprints"
}

# ck_unsigned_reason <note> <keyring>: prints the reason a .unsigned note
# gives, its lines without comments joined by spaces. Dies when the note
# gives no reason, or when the component also has a keyring.
ck_unsigned_reason() {
  local note=$1 keyring=$2 line reason=
  [ ! -e "$keyring" ] || ck_die "both $keyring and $note exist; remove one"
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%%#*}
    line=$(printf '%s' "$line" | tr -s ' \t\r' '  ')
    line=${line# }
    line=${line% }
    [ -n "$line" ] || continue
    reason="${reason:+$reason }$line"
  done <"$note"
  [ -n "$reason" ] || ck_die "$note gives no reason"
  printf '%s\n' "$reason"
}

# ck_php_json_hash <json> <file name>: prints the sha256 that php.net's
# release JSON gives for the file.
ck_php_json_hash() {
  local hash
  hash=$(jq -r --arg f "$2" \
    '[(.source // .[].source // empty)[] | select(.filename == $f) | .sha256] | if length == 1 then .[0] else empty end' \
    "$1") || ck_die "cannot read php.net's release JSON"
  ck_is_sha256 "$hash" || ck_die "php.net's release JSON has no single sha256 for $2"
  printf '%s\n' "$hash"
}

ck_refresh_main() {
  if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ]; then
    ck_die "refresh-pins.sh does not run in CI: a person refreshes pins and reviews the lock diff"
  fi
  [ "$#" -ge 2 ] || ck_refresh_usage
  local name=$1 version=$2 url='' work file sha size license kind=none
  local checked=() fpr expected
  shift 2
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --url)
        [ "$#" -ge 2 ] || ck_refresh_usage
        url=$2
        shift 2
        ;;
      *) ck_refresh_usage ;;
    esac
  done
  local name_re='^[a-z0-9][a-z0-9-]{0,31}$' version_re='^[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}$'
  [[ $name =~ $name_re ]] || ck_die "not a component name: $name"
  [[ $version =~ $version_re ]] || ck_die "not a version: $version"
  case "$version" in *..*) ck_die "not a version: $version" ;; esac
  license=$(ck_refresh_license "$name" "$version") || exit 1
  if [ -z "$url" ]; then
    url=$(ck_refresh_url "$name" "$version") || exit 1
  fi
  ck_refresh_check_host "$url"
  ck_require_cmd curl
  ck_release lock check >/dev/null || ck_die "the lock or manifest does not pass ck-release lock check"

  work=$(mktemp -d "${TMPDIR:-/tmp}/ck-refresh.XXXXXX") || ck_die "cannot create a temporary folder"
  # Expand now: the path is fixed.
  # shellcheck disable=SC2064
  trap "rm -rf -- '$work'" EXIT
  file=${url##*/}
  mkdir "$work/dl" || ck_die "cannot create $work/dl"
  ck_log "downloading $url"
  ck_download "$url" "$work/dl/$file" "$CK_REFRESH_MAX_BYTES"
  size=$(ck_size "$work/dl/$file") || exit 1
  sha=$(ck_sha256 "$work/dl/$file") || exit 1
  checked+=("size $size bytes, sha256 $sha")

  # A second published hash.
  case "$name" in
    php)
      ck_require_cmd jq
      local json_url="https://www.php.net/releases/index.php?json&version=$version"
      ck_download "$json_url" "$work/release.json" 1048576
      expected=$(ck_php_json_hash "$work/release.json" "$file") || exit 1
      [ "$expected" = "$sha" ] || ck_die "php.net's release JSON gives $expected for $file, the download is $sha"
      checked+=("sha256 matches php.net's release JSON")
      ;;
    openssl | oniguruma | cacert)
      ck_refresh_check_host "$url.sha256"
      ck_download "$url.sha256" "$work/sum" 65536
      expected=$(ck_sumfile_hash "$work/sum" "$file") || exit 1
      [ "$expected" = "$sha" ] || ck_die "$file.sha256 gives $expected, the download is $sha"
      checked+=("sha256 matches $url.sha256")
      kind=sha256-file
      ;;
    libxml2)
      local sum_url="${url%/*}/libxml2-$version.sha256sum"
      ck_refresh_check_host "$sum_url"
      ck_download "$sum_url" "$work/sum" 65536
      expected=$(ck_sumfile_hash "$work/sum" "$file") || exit 1
      [ "$expected" = "$sha" ] || ck_die "${sum_url##*/} gives $expected, the download is $sha"
      checked+=("sha256 matches $sum_url")
      kind=sha256-file
      ;;
  esac

  # A signature.
  case "$name" in
    php | openssl | curl | icu | zlib)
      if [ -f "$CK_KEYS_DIR/$name.unsigned" ]; then
        local reason
        reason=$(ck_unsigned_reason "$CK_KEYS_DIR/$name.unsigned" "$CK_KEYS_DIR/$name.gpg") || exit 1
        ck_log "WARNING: the $name signature was NOT verified: $reason"
        checked+=("no signature check (keys/$name.unsigned: $reason)")
      else
        ck_require_cmd gpgv
        ck_refresh_check_host "$url.asc"
        ck_download "$url.asc" "$work/sig.asc" 1048576
        fpr=$(ck_gpg_verify "$name" "$work/sig.asc" "$work/dl/$file" "$work") || exit 1
        checked+=("gpg signature $url.asc by allowed key $fpr")
        kind=gpg
      fi
      ;;
    libsodium)
      if command -v minisign >/dev/null 2>&1; then
        local pub="$CK_KEYS_DIR/libsodium.minisign.pub"
        [ -f "$pub" ] || ck_die "missing $pub; fetch it from libsodium's documentation"
        ck_refresh_check_host "$url.minisig"
        ck_download "$url.minisig" "$work/sig.minisig" 65536
        minisign -V -p "$pub" -m "$work/dl/$file" -x "$work/sig.minisig" >&2 ||
          ck_die "minisign rejected the signature on $file"
        checked+=("minisign signature $url.minisig against $pub")
        kind=minisig
      else
        ck_log "WARNING: minisign is not installed, so the libsodium signature was NOT verified."
        ck_log "WARNING: the pin is recorded with signature 'none'. Install minisign and run again to verify it."
      fi
      ;;
  esac
  if [ "$kind" = none ]; then
    ck_log "WARNING: nothing upstream was checked for $name $version besides this download; review the hash by hand"
  fi

  ck_release lock set "$name" "$version" --url "$url" --sha256 "$sha" --size "$size" \
    --license "$license" --signature "$kind" || ck_die "ck-release lock set failed"

  printf 'pinned %s %s (%s, signature: %s)\n' "$name" "$version" "$license" "$kind"
  printf '  url: %s\n' "$url"
  local item
  for item in "${checked[@]}"; do
    printf '  checked: %s\n' "$item"
  done
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  ck_refresh_main "$@"
fi
