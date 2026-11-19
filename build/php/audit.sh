# shellcheck shell=bash
# T6, the static audit of a relocated PHP root (an unpacked archive or
# $(ck_php_root <line>)), and the object-file helpers relocation shares.
# Sourced by build.sh after lib/common.sh. It uses only system tools
# (readelf on Linux; otool, lipo and codesign on macOS), so it also runs on a
# fresh runner with no build root and no ck-release.
#
# ck_audit <root> checks, and reports every violation before it fails:
#
# - Layout: only bin/{php,php-fpm,php-cgi}, ext/*.so, lib/*, etc/, licenses/
#   and ck-runtime.json at the top; regular files and folders only (no
#   links, devices or hidden names); the executable bit on every object file
#   and on nothing else.
# - Linux, every ELF: built for this CPU; each DT_NEEDED is in lib/ or in
#   CK_AUDIT_LINUX_SYSTEM; libcrypt is never linked; DT_RPATH is exactly
#   $ORIGIN/../lib (bin, ext) or $ORIGIN (lib) and there is no DT_RUNPATH;
#   a library's soname is its file name; GLIBC_ versions at most
#   CK_AUDIT_GLIBC_MAX, GLIBCXX_ at most CK_AUDIT_GLIBCXX_MAX, and no
#   GLIBC_PRIVATE.
# - macOS, every Mach-O: arm64 only; each linked library is @rpath/<a file in
#   lib/> or in CK_AUDIT_MACOS_SYSTEM; a library's id is @rpath/<its name>;
#   exactly one LC_RPATH, @loader_path/../lib (bin, ext) or @loader_path
#   (lib); minimum macOS at most CK_AUDIT_MACOS_MIN; a valid signature.
# - Every library in lib/ is linked by something.
# - No build path in any object file: see ck_audit_strings.
#
# Call the functions as plain commands, never in an `if`, `&&` or `||`.

if [ -n "${CK_AUDIT_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${CK_COMMON_SH_LOADED:-}" ]; then
  echo "ck-build: error: source lib/common.sh before audit.sh" >&2
  exit 1
fi
CK_AUDIT_SH_LOADED=1

# The Linux baseline: the ubuntu-22.04 runner.
readonly CK_AUDIT_GLIBC_MAX=2.35
readonly CK_AUDIT_GLIBCXX_MAX=3.4.30
# The macOS baseline.
readonly CK_AUDIT_MACOS_MIN=12.0

# System libraries an archive may link on Linux: the glibc family, and
# libstdc++ and libgcc_s for ICU and intl.
readonly CK_AUDIT_LINUX_SYSTEM="libc.so.6 libm.so.6 libpthread.so.0 libdl.so.2
librt.so.1 libresolv.so.2 libutil.so.1 ld-linux-x86-64.so.2 libstdc++.so.6
libgcc_s.so.1"

# System libraries and frameworks an archive may link on macOS.
readonly CK_AUDIT_MACOS_SYSTEM="/usr/lib/libSystem.B.dylib
/usr/lib/libc++.1.dylib
/usr/lib/libiconv.2.dylib
/usr/lib/libresolv.9.dylib
/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation
/System/Library/Frameworks/CoreServices.framework/Versions/A/CoreServices
/System/Library/Frameworks/Security.framework/Versions/A/Security
/System/Library/Frameworks/SystemConfiguration.framework/Versions/A/SystemConfiguration"

# Build paths no object file may contain: home folders, temporary build
# folders, /usr/local and package-manager prefixes. A folder under /tmp is
# refused, but not a file directly in it, which is a runtime default (the
# MySQL socket /tmp/mysql.sock). /opt is checked separately.
readonly CK_AUDIT_BAD_PATHS='/home/[A-Za-z0-9._-]+|/Users/[A-Za-z0-9._-]+|/tmp/[A-Za-z0-9._-]+/|/private/(tmp|var)/|/var/folders/|/usr/local|/nix/store/'

readonly CK_AUDIT_NL=$'\n'

# The number of violations ck_audit found so far, and the names in lib/
# that the files checked so far link.
CK_AUDIT_ERRORS=0
CK_AUDIT_LINKED=

# ck_audit_fail <message...>: records one violation and goes on.
ck_audit_fail() {
  CK_AUDIT_ERRORS=$((CK_AUDIT_ERRORS + 1))
  printf 'ck-build: audit: %s\n' "$*" >&2
}

# ck_in_list <word> <list>: true when the word is one of the list's words.
ck_in_list() {
  local word=$1 item
  local -a items
  read -r -d '' -a items <<<"$2" || true
  for item in ${items[@]+"${items[@]}"}; do
    if [ "$item" = "$word" ]; then return 0; fi
  done
  return 1
}

# ck_version_le <a> <b>: true when dotted version a is at most b.
ck_version_le() {
  local a=$1 b=$2 x y
  while [ -n "$a" ] || [ -n "$b" ]; do
    x=${a%%.*}
    y=${b%%.*}
    case "$a" in *.*) a=${a#*.} ;; *) a= ;; esac
    case "$b" in *.*) b=${b#*.} ;; *) b= ;; esac
    case "${x:-0}${y:-0}" in *[!0-9]*) ck_die "not a dotted version: $1 or $2" ;; esac
    if [ "$((10#${x:-0}))" -lt "$((10#${y:-0}))" ]; then return 0; fi
    if [ "$((10#${x:-0}))" -gt "$((10#${y:-0}))" ]; then return 1; fi
  done
  return 0
}

# ck_is_elf <file>: true when the file starts with the ELF magic.
ck_is_elf() {
  local magic
  magic=$(LC_ALL=C head -c 4 "$1" | od -An -tx1 | tr -d ' \n') || return 1
  [ "$magic" = 7f454c46 ]
}

# ck_is_macho <file>: true when the file is a 64-bit Mach-O (or fat) file.
ck_is_macho() {
  local magic
  magic=$(LC_ALL=C head -c 4 "$1" | od -An -tx1 | tr -d ' \n') || return 1
  case "$magic" in
    cffaedfe | feedfacf | cafebabe | bebafeca) return 0 ;;
  esac
  return 1
}

# ck_elf_dynamic <file>: prints "<tag>\t<value>" for each NEEDED, RPATH,
# RUNPATH and SONAME entry, from `readelf -dW`.
ck_elf_dynamic() {
  local out line tag value
  out=$(LC_ALL=C readelf -dW "$1") || ck_die "readelf -d failed on $1"
  while IFS= read -r line; do
    case "$line" in
      *'(NEEDED)'*'['*']') tag=NEEDED ;;
      *'(RPATH)'*'['*']') tag=RPATH ;;
      *'(RUNPATH)'*'['*']') tag=RUNPATH ;;
      *'(SONAME)'*'['*']') tag=SONAME ;;
      *) continue ;;
    esac
    value=${line#*[}
    value=${value%]}
    printf '%s\t%s\n' "$tag" "$value"
  done <<<"$out"
}

# ck_elf_needed <file>: prints the file's DT_NEEDED names, one per line.
ck_elf_needed() {
  local dynamic tag value
  dynamic=$(ck_elf_dynamic "$1") || exit 1
  while IFS=$'\t' read -r tag value; do
    if [ "$tag" = NEEDED ]; then printf '%s\n' "$value"; fi
  done <<<"$dynamic"
}

# ck_elf_symbol_versions <file> <prefix>: prints each required symbol
# version that starts with <prefix>_ (GLIBC, GLIBCXX), one per line.
ck_elf_symbol_versions() {
  local out line word rest
  out=$(LC_ALL=C readelf -VW "$1") || ck_die "readelf -V failed on $1"
  while IFS= read -r line; do
    case "$line" in
      *"Name: $2_"*) ;;
      *) continue ;;
    esac
    rest=${line#*Name: }
    word=${rest%% *}
    printf '%s\n' "$word"
  done <<<"$out"
}

# ck_macho_loads <file>: prints "<command>\t<value>" for the file's
# LC_ID_DYLIB, every LC_*_DYLIB it links, LC_RPATH, and its minimum macOS
# (LC_BUILD_VERSION minos or LC_VERSION_MIN_MACOSX version), from
# `otool -l`.
ck_macho_loads() {
  local out line cmd='' key rest value
  out=$(otool -l "$1") || ck_die "otool -l failed on $1"
  while IFS= read -r line; do
    line=${line#"${line%%[! ]*}"}
    key=${line%% *}
    rest=${line#"$key"}
    rest=${rest#"${rest%%[! ]*}"}
    value=${rest%% (offset *}
    case "$key" in
      cmd) cmd=$value ;;
      name)
        case "$cmd" in
          LC_ID_DYLIB | LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | \
            LC_LAZY_LOAD_DYLIB | LC_LOAD_UPWARD_DYLIB)
            printf '%s\t%s\n' "$cmd" "$value"
            ;;
        esac
        ;;
      path) if [ "$cmd" = LC_RPATH ]; then printf '%s\t%s\n' "$cmd" "$value"; fi ;;
      minos) if [ "$cmd" = LC_BUILD_VERSION ]; then printf 'MINOS\t%s\n' "$value"; fi ;;
      version) if [ "$cmd" = LC_VERSION_MIN_MACOSX ]; then printf 'MINOS\t%s\n' "$value"; fi ;;
    esac
  done <<<"$out"
}

# ck_object_folder <root> <file>: prints bin, ext or lib.
ck_object_folder() {
  local rel=${2#"$1"/}
  case "$rel" in
    bin/* | ext/* | lib/*) echo "${rel%%/*}" ;;
    *) ck_die "not in bin/, ext/ or lib/: $2" ;;
  esac
}

# ck_audit_strings <file>: records a violation for each build path in the
# file. /opt/ck-dev-build is the only /opt folder allowed: only root can
# create it on a user's machine, and the libraries keep paths under it
# (OPENSSLDIR, ICU's data folder, PHP's own prefix).
ck_audit_strings() {
  local file=$1 hits status opt line
  status=0
  hits=$(LC_ALL=C grep -a -o -E "$CK_AUDIT_BAD_PATHS" "$file" | LC_ALL=C sort -u) || status=$?
  case "$status" in
    0) ;;
    1) hits= ;;
    *) ck_die "grep failed on $file" ;;
  esac
  case "$file" in
    */ext/fileinfo.so)
      # libmagic's built-in database describes these paths as text to
      # match; they are data, not where anything was built.
      hits=$(printf '%s\n' "$hits" | LC_ALL=C grep -v -x -E '/nix/store/|/usr/local') || true
      ;;
  esac
  if [ -n "$hits" ]; then
    ck_audit_fail "$file holds build paths: ${hits//"$CK_AUDIT_NL"/ }"
  fi
  status=0
  opt=$(LC_ALL=C grep -a -o -E '/opt/[A-Za-z0-9._-]*' "$file" | LC_ALL=C sort -u) || status=$?
  case "$status" in
    0) ;;
    1) opt= ;;
    *) ck_die "grep failed on $file" ;;
  esac
  hits=
  while IFS= read -r line; do
    case "$line" in
      '' | /opt/ck-dev-build) ;;
      *) hits="$hits $line" ;;
    esac
  done <<<"$opt"
  if [ -n "$hits" ]; then
    ck_audit_fail "$file holds paths under /opt other than /opt/ck-dev-build:$hits"
  fi
}

# ck_audit_layout <root>: the folder and file rules.
ck_audit_layout() {
  local root=$1 path name found
  for path in "$root"/* "$root"/.*; do
    name=${path##*/}
    case "$name" in
      . | .. | '*' | '.*') [ -e "$path" ] || continue ;;
    esac
    case "$name" in
      bin | ext | lib | etc | licenses)
        if [ -L "$path" ] || [ ! -d "$path" ]; then ck_audit_fail "$name is not a folder"; fi
        ;;
      ck-runtime.json)
        if [ -L "$path" ] || [ ! -f "$path" ]; then ck_audit_fail "$name is not a file"; fi
        ;;
      . | ..) ;;
      *) ck_audit_fail "unexpected top-level entry: $name" ;;
    esac
  done
  for name in bin/php bin/php-fpm bin/php-cgi etc/php.ini-development etc/php.ini-production \
    etc/ssl/cert.pem licenses/php/LICENSE ck-runtime.json; do
    [ -f "$root/$name" ] || ck_audit_fail "missing $name"
  done
  found=$(find "$root" ! -type f ! -type d -print) || ck_die "find failed in $root"
  [ -z "$found" ] || ck_audit_fail "links or special files: ${found//"$CK_AUDIT_NL"/, }"
  found=$(find "$root" -name '.*' -print) || ck_die "find failed in $root"
  [ -z "$found" ] || ck_audit_fail "hidden names: ${found//"$CK_AUDIT_NL"/, }"
  for path in "$root"/bin/*; do
    [ -e "$path" ] || continue
    case "${path##*/}" in
      php | php-fpm | php-cgi) ;;
      *) ck_audit_fail "unexpected file in bin/: ${path##*/}" ;;
    esac
  done
  for path in "$root"/ext/*; do
    [ -e "$path" ] || continue
    case "${path##*/}" in
      *.so) ;;
      *) ck_audit_fail "unexpected file in ext/: ${path##*/}" ;;
    esac
  done
  found=$(find "$root/etc" "$root/licenses" "$root/ck-runtime.json" -type f -perm -0100 -print 2>/dev/null) || true
  [ -z "$found" ] || ck_audit_fail "executable files outside bin/, ext/ and lib/: ${found//"$CK_AUDIT_NL"/, }"
}

# ck_audit_linux_file <root> <file> <cpu>: the ELF rules for one file;
# adds the names in lib/ it links to CK_AUDIT_LINKED.
ck_audit_linux_file() {
  local root=$1 file=$2 cpu=$3 folder want header dynamic tag value rpaths='' runpath='' soname=''
  local needed=0 v versions max=
  folder=$(ck_object_folder "$root" "$file") || exit 1
  if ! ck_is_elf "$file"; then
    ck_audit_fail "$file is not an ELF file"
    return 0
  fi
  header=$(LC_ALL=C readelf -hW "$file") || ck_die "readelf -h failed on $file"
  case "$cpu:$header" in
    x86_64:*'Machine:'*'X86-64'*) ;;
    aarch64:*'Machine:'*'AArch64'*) ;;
    *) ck_audit_fail "$file is not built for $cpu" ;;
  esac
  dynamic=$(ck_elf_dynamic "$file") || exit 1
  while IFS=$'\t' read -r tag value; do
    case "$tag" in
      NEEDED)
        needed=$((needed + 1))
        case "$value" in
          libcrypt.so*) ck_audit_fail "$file links $value; PHP's bundled crypt must be used" ;;
          */*) ck_audit_fail "$file links a path: $value" ;;
          *)
            if [ -f "$root/lib/$value" ]; then
              CK_AUDIT_LINKED="$CK_AUDIT_LINKED $value"
            elif ! ck_in_list "$value" "$CK_AUDIT_LINUX_SYSTEM"; then
              ck_audit_fail "$file links $value, which is neither in lib/ nor an allowed system library"
            fi
            ;;
        esac
        ;;
      RPATH) rpaths="$rpaths|$value" ;;
      RUNPATH) runpath=$value ;;
      SONAME) soname=$value ;;
    esac
  done <<<"$dynamic"
  # Guards against a parse that silently finds nothing; libraries such as
  # ICU's data library may need nothing at all.
  if [ "$needed" -eq 0 ] && [ "$folder" = bin ]; then
    ck_audit_fail "$file has no DT_NEEDED entries; is readelf's output readable?"
  fi
  want=$(ck_platform_rpath "$folder")
  [ "$rpaths" = "|$want" ] || ck_audit_fail "$file has RPATH '${rpaths#|}', expected '$want'"
  [ -z "$runpath" ] || ck_audit_fail "$file has a RUNPATH ($runpath)"
  if [ "$folder" = lib ] && [ "$soname" != "${file##*/}" ]; then
    ck_audit_fail "$file has soname '$soname'; it must be its file name"
  fi
  versions=$(ck_elf_symbol_versions "$file" GLIBC) || exit 1
  while IFS= read -r v; do
    [ -n "$v" ] || continue
    case "$v" in
      GLIBC_PRIVATE) ck_audit_fail "$file needs GLIBC_PRIVATE" ;;
      GLIBC_[0-9]*)
        v=${v#GLIBC_}
        if ! ck_version_le "$v" "$CK_AUDIT_GLIBC_MAX"; then
          ck_audit_fail "$file needs GLIBC_$v, above the baseline $CK_AUDIT_GLIBC_MAX"
        fi
        max=$v
        ;;
      *) ck_audit_fail "$file needs an unknown glibc version $v" ;;
    esac
  done <<<"$versions"
  if [ -z "$max" ] && [ "${file##*/}" = php ]; then
    ck_audit_fail "$file needs no GLIBC_ versions; is readelf's output readable?"
  fi
  versions=$(ck_elf_symbol_versions "$file" GLIBCXX) || exit 1
  while IFS= read -r v; do
    [ -n "$v" ] || continue
    v=${v#GLIBCXX_}
    case "$v" in
      *[!0-9.]* | '') ck_audit_fail "$file needs an unknown libstdc++ version $v" ;;
      *)
        if ! ck_version_le "$v" "$CK_AUDIT_GLIBCXX_MAX"; then
          ck_audit_fail "$file needs GLIBCXX_$v, above the baseline $CK_AUDIT_GLIBCXX_MAX"
        fi
        ;;
    esac
  done <<<"$versions"
}

# ck_audit_macos_file <root> <file>: the Mach-O rules for one file; adds
# the names in lib/ it links to CK_AUDIT_LINKED.
ck_audit_macos_file() {
  local root=$1 file=$2 folder want loads cmd value name archs rpaths='' id='' minos=''
  folder=$(ck_object_folder "$root" "$file") || exit 1
  if ! ck_is_macho "$file"; then
    ck_audit_fail "$file is not a Mach-O file"
    return 0
  fi
  archs=$(lipo -archs "$file") || ck_die "lipo -archs failed on $file"
  [ "$archs" = arm64 ] || ck_audit_fail "$file is built for '$archs', expected arm64 only"
  loads=$(ck_macho_loads "$file") || exit 1
  while IFS=$'\t' read -r cmd value; do
    case "$cmd" in
      LC_ID_DYLIB) id=$value ;;
      LC_RPATH) rpaths="$rpaths|$value" ;;
      MINOS) minos=$value ;;
      LC_*DYLIB)
        case "$value" in
          @rpath/*)
            name=${value#@rpath/}
            case "$name" in
              */* | '') ck_audit_fail "$file links $value" ;;
              *)
                if [ -f "$root/lib/$name" ]; then
                  CK_AUDIT_LINKED="$CK_AUDIT_LINKED $name"
                else
                  ck_audit_fail "$file links $value, which is not in lib/"
                fi
                ;;
            esac
            ;;
          *)
            if ! ck_in_list "$value" "$CK_AUDIT_MACOS_SYSTEM"; then
              ck_audit_fail "$file links $value, which is neither @rpath/<lib> nor an allowed system library"
            fi
            ;;
        esac
        ;;
    esac
  done <<<"$loads"
  want=$(ck_platform_rpath "$folder")
  [ "$rpaths" = "|$want" ] || ck_audit_fail "$file has LC_RPATH '${rpaths#|}', expected '$want'"
  if [ "$folder" = lib ]; then
    [ "$id" = "@rpath/${file##*/}" ] || ck_audit_fail "$file has id '$id', expected @rpath/${file##*/}"
  fi
  if [ -z "$minos" ]; then
    ck_audit_fail "$file states no minimum macOS"
  elif ! ck_version_le "$minos" "$CK_AUDIT_MACOS_MIN"; then
    ck_audit_fail "$file needs macOS $minos, above the baseline $CK_AUDIT_MACOS_MIN"
  fi
  if ! codesign --verify --strict "$file"; then
    ck_audit_fail "$file has no valid signature"
  fi
}

# ck_audit <root>: runs every check on the root and dies if any failed.
ck_audit() {
  local root=${1:-} os cpu file name
  case "$root" in
    /*) ;;
    *) ck_die "ck_audit: the root must be an absolute path: $root" ;;
  esac
  if [ -L "$root" ] || [ ! -d "$root" ]; then
    ck_die "ck_audit: not a folder: $root"
  fi
  os=$(ck_os)
  cpu=$(ck_cpu)
  case "$os" in
    linux) ck_require_cmd readelf od grep find ;;
    macos) ck_require_cmd otool lipo codesign od grep find ;;
  esac
  CK_AUDIT_ERRORS=0
  CK_AUDIT_LINKED=
  ck_log "audit: $root"
  ck_audit_layout "$root"
  for file in "$root"/bin/* "$root"/ext/* "$root"/lib/*; do
    # Links and other non-files were reported by the layout check.
    if [ -L "$file" ] || [ ! -f "$file" ]; then continue; fi
    [ -x "$file" ] || ck_audit_fail "$file is not executable"
    case "$os" in
      linux) ck_audit_linux_file "$root" "$file" "$cpu" ;;
      macos) ck_audit_macos_file "$root" "$file" ;;
    esac
    ck_audit_strings "$file"
  done
  for file in "$root"/lib/*; do
    [ -e "$file" ] || continue
    name=${file##*/}
    ck_in_list "$name" "$CK_AUDIT_LINKED" || ck_audit_fail "lib/$name is linked by nothing"
  done
  [ "$CK_AUDIT_ERRORS" -eq 0 ] || ck_die "audit: $CK_AUDIT_ERRORS violations in $root"
  ck_log "audit: $root passed"
}
