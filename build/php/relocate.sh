# shellcheck shell=bash
# Relocation: turns PHP's install prefix into the archive tree
# $(ck_php_root <line>) that build/php/README.md describes, and keeps the
# library licences the archive ships. Sourced by build.sh after php.sh and
# audit.sh.
#
# - bin/php, bin/php-fpm, bin/php-cgi, and ext/<name>.so from PHP's
#   extension folder.
# - lib/<soname>: exactly the libraries from the stack that bin/ and ext/
#   need, followed transitively, copied as real files under the name they
#   are linked by. A library that is neither in the stack nor an allowed
#   system library stops the build.
# - Linux: strip debug info, then DT_RPATH $ORIGIN/../lib (bin, ext) or
#   $ORIGIN (lib) with patchelf --force-rpath, so LD_LIBRARY_PATH never wins.
# - macOS: strip debug info, then @rpath install names, one LC_RPATH of
#   @loader_path/../lib (bin, ext) or @loader_path (lib), then an ad-hoc
#   signature, since changing a load command breaks the old one.
# - etc/php.ini-{development,production}, etc/ssl/cert.pem, licenses/,
#   ck-runtime.json; mode 0755 for folders and object files, 0644 for the
#   rest.
#
# Call the functions as plain commands, never in an `if`, `&&` or `||`.

if [ -n "${CK_RELOCATE_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${CK_PHP_SH_LOADED:-}" ] || [ -z "${CK_AUDIT_SH_LOADED:-}" ]; then
  echo "ck-build: error: source php.sh and audit.sh before relocate.sh" >&2
  exit 1
fi
CK_RELOCATE_SH_LOADED=1

# Pins that are build tools or data, not linked libraries.
readonly CK_RELOCATE_NOT_LIBRARIES="pkgconf patchelf cacert"

# The module API of the relocated build, from PHP's extension folder name.
CK_PHP_MODULE_API=

# ck_licence_files <folder>: prints the top-level licence and notice files
# of an unpacked source tree, one per line.
ck_licence_files() {
  local path
  for path in "$1"/LICEN[CS]E* "$1"/COPYING* "$1"/COPYRIGHT* "$1"/Copyright* "$1"/NOTICE*; do
    if [ -f "$path" ] && [ ! -L "$path" ]; then printf '%s\n' "$path"; fi
  done
}

# ck_deps_save_licenses: after ck_deps_build, copies each library's licence
# files from its source tree into <prefix>/share/ck-licenses/<name>-<version>/,
# which travels with the prefix to the PHP stage. A prefix restored with the
# folder already there keeps it.
ck_deps_save_licenses() {
  local name version src dest files path prefix=${CK_DEPS_PREFIX:-}
  if [ -z "$prefix" ] || [ -z "${CK_STACK:-}" ]; then
    ck_die "deps licences: run ck_deps_build first"
  fi
  while IFS=$'\t' read -r name version _; do
    if ck_in_list "$name" "$CK_RELOCATE_NOT_LIBRARIES"; then continue; fi
    dest="$prefix/share/ck-licenses/$name-$version"
    src="$(ck_src_dir "$CK_STACK")/$name-$version"
    if [ ! -d "$src" ]; then
      [ -d "$dest" ] || ck_die "deps licences: no source tree for $name $version and no saved licence; rebuild the stack"
      continue
    fi
    files=$(ck_licence_files "$src") || exit 1
    ck_run rm -rf "$dest"
    ck_run mkdir -p "$dest"
    if [ -z "$files" ]; then
      case "$name" in
        sqlite)
          # The autoconf tarball ships no licence file: SQLite is public domain.
          printf '%s\n' "SQLite $version is in the public domain." \
            "See https://www.sqlite.org/copyright.html." >"$dest/NOTICE"
          ;;
        *) ck_die "deps licences: no licence file in $src" ;;
      esac
    fi
    while IFS= read -r path; do
      [ -n "$path" ] || continue
      ck_run cp "$path" "$dest/${path##*/}"
    done <<<"$files"
  done <<<"$CK_PINS"
}

# ck_relocate_copy_lib <root> <name>: copies <deps>/lib/<name>, following
# links, to <root>/lib/<name>. Dies when the stack has no such library.
ck_relocate_copy_lib() {
  local root=$1 name=$2
  case "$name" in
    '' | */* | .*) ck_die "relocate: not a library name: $name" ;;
  esac
  [ -f "$CK_DEPS_PREFIX/lib/$name" ] ||
    ck_die "relocate: $name is needed but is neither in the stack ($CK_DEPS_PREFIX/lib) nor an allowed system library"
  ck_run cp "$CK_DEPS_PREFIX/lib/$name" "$root/lib/$name"
  ck_run chmod 0755 "$root/lib/$name"
}

# ck_relocate_linux <root>: copies the closure, strips, sets DT_RPATH.
ck_relocate_linux() {
  local root=$1 i=0 file needed name folder patchelf="$CK_DEPS_PREFIX/bin/patchelf"
  local -a queue
  [ -x "$patchelf" ] || ck_die "relocate: no patchelf in the stack: $patchelf"
  queue=("$root"/bin/* "$root"/ext/*.so)
  while [ "$i" -lt "${#queue[@]}" ]; do
    file=${queue[$i]}
    i=$((i + 1))
    needed=$(ck_elf_needed "$file") || exit 1
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      case "$name" in
        libcrypt.so*) ck_die "relocate: $file links $name; PHP's bundled crypt must be used" ;;
      esac
      if ck_in_list "$name" "$CK_AUDIT_LINUX_SYSTEM"; then continue; fi
      if [ -e "$root/lib/$name" ]; then continue; fi
      ck_relocate_copy_lib "$root" "$name"
      queue+=("$root/lib/$name")
    done <<<"$needed"
  done
  for file in "${queue[@]}"; do
    folder=$(ck_object_folder "$root" "$file") || exit 1
    # Strip before patchelf: strip can break a file patchelf has grown.
    ck_run strip --strip-debug "$file"
    ck_run "$patchelf" --force-rpath --set-rpath "$(ck_platform_rpath "$folder")" "$file"
  done
}

# ck_macos_bundled_name <file> <install name>: prints the lib/ name of a
# library the file links from the stack, nothing for an allowed system
# library, and dies for anything else.
ck_macos_bundled_name() {
  local file=$1 value=$2 name
  case "$value" in
    "$CK_DEPS_PREFIX/lib/"*) name=${value#"$CK_DEPS_PREFIX/lib/"} ;;
    @rpath/*) name=${value#@rpath/} ;;
    *)
      if ck_in_list "$value" "$CK_AUDIT_MACOS_SYSTEM"; then return 0; fi
      ck_die "relocate: $file links $value, which is neither in the stack nor an allowed system library"
      ;;
  esac
  case "$name" in
    '' | */* | .*) ck_die "relocate: $file links $value" ;;
  esac
  printf '%s\n' "$name"
}

# ck_relocate_macos <root>: copies the closure, strips, rewrites install
# names and rpaths, signs.
ck_relocate_macos() {
  local root=$1 i=0 file loads cmd value name folder
  local -a queue args
  queue=("$root"/bin/* "$root"/ext/*.so)
  while [ "$i" -lt "${#queue[@]}" ]; do
    file=${queue[$i]}
    i=$((i + 1))
    loads=$(ck_macho_loads "$file") || exit 1
    while IFS=$'\t' read -r cmd value; do
      case "$cmd" in
        LC_ID_DYLIB | LC_RPATH | MINOS | '') continue ;;
      esac
      name=$(ck_macos_bundled_name "$file" "$value") || exit 1
      [ -n "$name" ] || continue
      if [ -e "$root/lib/$name" ]; then continue; fi
      ck_relocate_copy_lib "$root" "$name"
      queue+=("$root/lib/$name")
    done <<<"$loads"
  done
  for file in "${queue[@]}"; do
    folder=$(ck_object_folder "$root" "$file") || exit 1
    ck_run strip -S "$file"
    loads=$(ck_macho_loads "$file") || exit 1
    args=()
    if [ "$folder" = lib ]; then args+=(-id "@rpath/${file##*/}"); fi
    while IFS=$'\t' read -r cmd value; do
      case "$cmd" in
        LC_RPATH) args+=(-delete_rpath "$value") ;;
        LC_ID_DYLIB | MINOS | '') ;;
        *)
          name=$(ck_macos_bundled_name "$file" "$value") || exit 1
          if [ -n "$name" ] && [ "$value" != "@rpath/$name" ]; then
            args+=(-change "$value" "@rpath/$name")
          fi
          ;;
      esac
    done <<<"$loads"
    if [ "${#args[@]}" -gt 0 ]; then ck_run install_name_tool "${args[@]}" "$file"; fi
    # Separate call: one call may not delete and add the same rpath.
    ck_run install_name_tool -add_rpath "$(ck_platform_rpath "$folder")" "$file"
    ck_run codesign --force --sign - "$file"
  done
}

# ck_php_licenses <root>: licenses/php/ (PHP's LICENSE and the licences of
# the code PHP bundles, flattened to one folder), licenses/<library>/ for
# each linked library, licenses/cacert/NOTICE.
ck_php_licenses() {
  local root=$1 src list rel name version dest saved
  src=$(ck_php_src_dir "$CK_PHP_LINE")
  dest="$root/licenses/php"
  ck_run mkdir -p "$dest"
  [ -f "$src/LICENSE" ] || ck_die "relocate: no LICENSE in $src"
  ck_run cp "$src/LICENSE" "$dest/LICENSE"
  list=$(cd "$src" && find . \( -name tests -o -name test \) -prune -o -type f \
    \( -name 'LICENSE*' -o -name 'LICENCE*' -o -name 'COPYING*' \) -print | LC_ALL=C sort) ||
    ck_die "relocate: cannot list licence files in $src"
  while IFS= read -r rel; do
    rel=${rel#./}
    case "$rel" in '' | LICENSE) continue ;; esac
    name=${rel//\//-}
    case "$name" in
      *[!A-Za-z0-9._-]*) ck_die "relocate: unexpected licence file name: $rel" ;;
    esac
    ck_run cp "$src/$rel" "$dest/$name"
  done <<<"$list"

  while IFS=$'\t' read -r name version _; do
    if ck_in_list "$name" "$CK_RELOCATE_NOT_LIBRARIES"; then continue; fi
    saved="$CK_DEPS_PREFIX/share/ck-licenses/$name-$version"
    [ -d "$saved" ] || ck_die "relocate: no saved licence for $name $version in $saved; rebuild the stack"
    ck_run mkdir -p "$root/licenses/$name"
    for rel in "$saved"/*; do
      if [ -f "$rel" ] && [ ! -L "$rel" ]; then ck_run cp "$rel" "$root/licenses/$name/${rel##*/}"; fi
    done
    [ -n "$(ls -A "$root/licenses/$name")" ] || ck_die "relocate: $saved is empty"
  done <<<"$CK_PHP_STACK_PINS"

  ck_run mkdir -p "$root/licenses/cacert"
  printf '%s\n' \
    "etc/ssl/cert.pem is the CA certificate bundle that the curl project" \
    "extracts from Mozilla's NSS (https://curl.se/ca/), version $CK_PHP_CACERT_VERSION," \
    "unmodified. Licence: $CK_PHP_CACERT_LICENSE (https://mozilla.org/MPL/2.0/). The file" \
    "is its own source form." >"$root/licenses/cacert/NOTICE"
}

# ck_json_str <text>: prints the text as a JSON string. Dies on control
# characters, which no field here should hold.
ck_json_str() {
  local text=$1
  case "$text" in
    *[[:cntrl:]]*) ck_die "relocate: control character in a ck-runtime.json value: $text" ;;
  esac
  text=${text//\\/\\\\}
  text=${text//\"/\\\"}
  printf '"%s"' "$text"
}

# ck_json_list <space-separated words>: prints a JSON array of strings.
ck_json_list() {
  local word sep= out
  local -a words
  read -r -a words <<<"$1"
  printf '['
  for word in ${words[@]+"${words[@]}"}; do
    out=$(ck_json_str "$word") || exit 1
    printf '%s%s' "$sep" "$out"
    sep=', '
  done
  printf ']'
}

# ck_php_runtime_json <root>: writes <root>/ck-runtime.json.
ck_php_runtime_json() {
  local root=$1 os cpu toolchain baseline name version sha license sep= cacert_sha
  local s_release s_php s_line s_os s_cpu s_api s_tool s_src l_builtin l_default l_optional l_patches
  local s_name s_version s_license s_sha libs= s_ca_version s_ca_sha s_ca_license
  os=$(ck_os)
  cpu=$(ck_cpu)
  toolchain=$("$CC" --version) || ck_die "relocate: $CC --version failed"
  toolchain=${toolchain%%"$CK_AUDIT_NL"*}
  case "$os" in
    linux) baseline="{\"glibc\": \"$CK_AUDIT_GLIBC_MAX\"}" ;;
    macos) baseline="{\"macos\": \"$CK_AUDIT_MACOS_MIN\"}" ;;
  esac
  cacert_sha=$(ck_sha256 "$root/etc/ssl/cert.pem") || exit 1
  s_release=$(ck_json_str "$CK_PHP_RELEASE") || exit 1
  s_php=$(ck_json_str "$CK_PHP_VERSION") || exit 1
  s_line=$(ck_json_str "$CK_PHP_LINE") || exit 1
  s_os=$(ck_json_str "$os") || exit 1
  s_cpu=$(ck_json_str "$cpu") || exit 1
  s_api=$(ck_json_str "$CK_PHP_MODULE_API") || exit 1
  s_tool=$(ck_json_str "$toolchain") || exit 1
  s_src=$(ck_json_str "$CK_PHP_SOURCE_SHA256") || exit 1
  l_builtin=$(ck_json_list "$CK_PHP_BUILTIN") || exit 1
  l_default=$(ck_json_list "$CK_PHP_DEFAULT") || exit 1
  l_optional=$(ck_json_list "$CK_PHP_OPTIONAL") || exit 1
  l_patches=$(ck_json_list "$CK_PHP_PATCHES") || exit 1
  s_ca_version=$(ck_json_str "$CK_PHP_CACERT_VERSION") || exit 1
  s_ca_sha=$(ck_json_str "$cacert_sha") || exit 1
  s_ca_license=$(ck_json_str "$CK_PHP_CACERT_LICENSE") || exit 1
  while IFS=$'\t' read -r name version _ sha _ license _ _; do
    if ck_in_list "$name" "$CK_RELOCATE_NOT_LIBRARIES"; then continue; fi
    s_name=$(ck_json_str "$name") || exit 1
    s_version=$(ck_json_str "$version") || exit 1
    s_license=$(ck_json_str "$license") || exit 1
    s_sha=$(ck_json_str "$sha") || exit 1
    libs="$libs$sep
    {\"name\": $s_name, \"version\": $s_version, \"license\": $s_license, \"source_sha256\": $s_sha}"
    sep=,
  done <<<"$CK_PHP_STACK_PINS"
  cat >"$root/ck-runtime.json" <<EOF
{
  "schema": 1,
  "name": "php",
  "version": $s_release,
  "php_version": $s_php,
  "line": $s_line,
  "build_revision": $CK_PHP_REVISION,
  "os": $s_os,
  "cpu": $s_cpu,
  "thread_safety": "nts",
  "module_api": $s_api,
  "php_source_sha256": $s_src,
  "patches": $l_patches,
  "extensions": {
    "builtin": $l_builtin,
    "default": $l_default,
    "optional": $l_optional
  },
  "libraries": [$libs
  ],
  "ca_bundle": {"version": $s_ca_version, "license": $s_ca_license, "sha256": $s_ca_sha},
  "baseline": $baseline,
  "toolchain": $s_tool
}
EOF
}

# ck_php_relocate <line>: builds $(ck_php_root <line>) from the prefix that
# ck_php_build installed, in the same process.
ck_php_relocate() {
  local line=${1:-} os prefix src root dirs dir path name have want
  local -a names
  if [ -z "$line" ] || [ "$line" != "$CK_PHP_LINE" ]; then
    ck_die "relocate: run ck_php_build $line first"
  fi
  [ "${CK_DEPS_PREFIX:-}" = "$(ck_deps_prefix "$CK_PHP_STACK")" ] || ck_die "relocate: run ck_php_build $line first"
  os=$(ck_os)
  prefix=$(ck_php_prefix "$line")
  src=$(ck_php_src_dir "$line")
  root=$(ck_php_root "$line")
  ck_run rm -rf "$root"
  for dir in bin ext lib etc/ssl licenses; do
    ck_run mkdir -p "$root/$dir"
  done
  ck_run cp "$prefix/bin/php" "$root/bin/php"
  ck_run cp "$prefix/bin/php-cgi" "$root/bin/php-cgi"
  ck_run cp "$prefix/sbin/php-fpm" "$root/bin/php-fpm"

  dirs=
  for path in "$prefix"/lib/php/extensions/no-debug-non-zts-*; do
    if [ -d "$path" ]; then dirs="$dirs $path"; fi
  done
  dirs=${dirs# }
  case "$dirs" in
    '' | *' '*) ck_die "relocate: expected one no-debug-non-zts-* extension folder in $prefix/lib/php/extensions, found '$dirs'" ;;
  esac
  CK_PHP_MODULE_API=${dirs##*-}
  case "$CK_PHP_MODULE_API" in
    '' | *[!0-9]*) ck_die "relocate: no module API in $dirs" ;;
  esac
  for path in "$dirs"/*.so; do
    [ -f "$path" ] || continue
    ck_run cp "$path" "$root/ext/${path##*/}"
  done
  # Autoconf only warns about an extension it skips, so compare.
  have=
  for path in "$root"/ext/*.so; do
    [ -f "$path" ] || continue
    name=${path##*/}
    have="$have${name%.so}$CK_AUDIT_NL"
  done
  have=$(printf '%s' "$have" | LC_ALL=C sort)
  read -r -a names <<<"$CK_PHP_DEFAULT $CK_PHP_OPTIONAL"
  want=$(printf '%s\n' ${names[@]+"${names[@]}"} | LC_ALL=C sort)
  if [ "$have" != "$want" ]; then
    ck_die "relocate: ext/ holds [${have//"$CK_AUDIT_NL"/ }], the line's default and optional extensions are [${want//"$CK_AUDIT_NL"/ }]"
  fi

  ck_run cp "$src/php.ini-development" "$root/etc/php.ini-development"
  ck_run cp "$src/php.ini-production" "$root/etc/php.ini-production"
  ck_run cp "$CK_PHP_CACERT" "$root/etc/ssl/cert.pem"

  ck_run chmod 0755 "$root"/bin/* "$root"/ext/*.so
  case "$os" in
    linux) ck_relocate_linux "$root" ;;
    macos) ck_relocate_macos "$root" ;;
  esac
  ck_php_licenses "$root"
  ck_php_runtime_json "$root"

  ck_run find "$root" -type d -exec chmod 0755 {} +
  ck_run find "$root/etc" "$root/licenses" "$root/ck-runtime.json" -type f -exec chmod 0644 {} +
  ck_run chmod 0755 "$root"/bin/* "$root"/ext/*.so "$root"/lib/*
  ck_log "relocate: $root is ready"
}
