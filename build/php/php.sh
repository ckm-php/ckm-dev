# shellcheck shell=bash
# The PHP stage: configure, build and install one PHP line into its prefix,
# $(ck_php_prefix <line>), against the line's library stack. Sourced by
# build.sh after deps/all.sh.
#
# The configure flags come from the line's extension classes in
# catalog/php-lines.json (`ck-release lock line`): builtin is compiled in,
# default and optional are built `=shared`. --disable-all starts from
# nothing, so no extension is built that the manifest does not name, and
# --enable-option-checking=fatal stops on a flag the line's configure does
# not know.
#
# Call the functions as plain commands, never in an `if`, `&&` or `||`.

if [ -n "${CK_PHP_SH_LOADED:-}" ]; then
  return 0
fi
if [ -z "${CK_FETCH_SH_LOADED:-}" ] || [ -z "${CK_PLATFORM_SH_LOADED:-}" ]; then
  echo "ck-build: error: source deps/all.sh before php.sh" >&2
  exit 1
fi
CK_PHP_SH_LOADED=1

# What `lock line` says about the loaded line.
CK_PHP_LINE=
CK_PHP_VERSION=
CK_PHP_RELEASE=
CK_PHP_REVISION=
CK_PHP_STACK=
CK_PHP_LICENSE=
CK_PHP_BUILTIN=
CK_PHP_DEFAULT=
CK_PHP_OPTIONAL=
# The stack's pins, kept because ck_pins_load replaces CK_PINS.
CK_PHP_STACK_PINS=
# The verified CA bundle in the download cache, and its pin.
CK_PHP_CACERT=
CK_PHP_CACERT_VERSION=
CK_PHP_CACERT_LICENSE=
# The PHP source pin.
CK_PHP_SOURCE_SHA256=
# Patch file names applied to the source, space-separated.
CK_PHP_PATCHES=
# macOS: the SDK's usr folder, where iconv.h is.
CK_PHP_ICONV_DIR=
# The configure arguments, built by ck_php_configure_args.
CK_PHP_CONFIGURE=()

# ck_php_run <dir> <argv...>: runs one command in <dir> and dies if it fails.
ck_php_run() {
  local dir=$1
  shift
  (cd "$dir" && ck_run "$@") || ck_die "php: $1 failed in $dir"
}

# ck_php_line_load <line>: reads `ck-release lock line <line>` into the
# CK_PHP_* variables above.
ck_php_line_load() {
  local line=${1:-} info key value
  ck_check_line "$line"
  info=$(ck_release lock line "$line") || ck_die "ck-release lock line $line failed"
  [ -n "$info" ] || ck_die "ck-release lock line $line printed nothing"
  CK_PHP_LINE= CK_PHP_VERSION= CK_PHP_RELEASE= CK_PHP_REVISION= CK_PHP_STACK=
  CK_PHP_LICENSE= CK_PHP_BUILTIN= CK_PHP_DEFAULT= CK_PHP_OPTIONAL=
  while IFS=$'\t' read -r key value; do
    case "$key" in
      line) CK_PHP_LINE=$value ;;
      php) CK_PHP_VERSION=$value ;;
      version) CK_PHP_RELEASE=$value ;;
      revision) CK_PHP_REVISION=$value ;;
      stack) CK_PHP_STACK=$value ;;
      license) CK_PHP_LICENSE=$value ;;
      builtin) CK_PHP_BUILTIN=$value ;;
      default) CK_PHP_DEFAULT=$value ;;
      optional) CK_PHP_OPTIONAL=$value ;;
      *) ck_die "lock line $line: unknown key: $key" ;;
    esac
  done <<<"$info"
  [ "$CK_PHP_LINE" = "$line" ] || ck_die "lock line $line: printed line '$CK_PHP_LINE'"
  case "$CK_PHP_VERSION" in
    "$line".*) ;;
    *) ck_die "lock line $line: the php pin $CK_PHP_VERSION is not in the line" ;;
  esac
  [ "$CK_PHP_RELEASE" = "$CK_PHP_VERSION-ck$CK_PHP_REVISION" ] ||
    ck_die "lock line $line: unexpected version $CK_PHP_RELEASE"
  ck_check_stack "$CK_PHP_STACK"
  [ -n "$CK_PHP_LICENSE" ] || ck_die "lock line $line: no licence"
  [ -n "$CK_PHP_BUILTIN" ] || ck_die "lock line $line: no built-in extensions"
}

# ck_php_stack_load: loads the line's stack pins, checks the stack was built
# from exactly these pins, and fetches the CA bundle. Needs ck_hermetic_env.
ck_php_stack_load() {
  local os pin name version url sha size license file
  [ "${CK_DEPS_PREFIX:-}" = "$(ck_deps_prefix "$CK_PHP_STACK")" ] ||
    ck_die "php: run ck_hermetic_env $CK_PHP_STACK first"
  os=$(ck_os)
  ck_pins_load --stack "$CK_PHP_STACK"
  CK_PHP_STACK_PINS=$CK_PINS
  while IFS=$'\t' read -r name version _ _ _ _ _ _; do
    case "$name" in
      cacert) continue ;;
      patchelf) [ "$os" = linux ] || continue ;;
    esac
    [ -f "$CK_DEPS_PREFIX/.stamps/$name-$version" ] ||
      ck_die "php: stack $CK_PHP_STACK has no $name $version; run build.sh deps $CK_PHP_STACK first"
  done <<<"$CK_PHP_STACK_PINS"
  pin=$(ck_pin cacert) || exit 1
  IFS=$'\t' read -r name version url sha size license _ file <<<"$pin"
  CK_PHP_CACERT=$(ck_fetch "$name" "$version" "$url" "$sha" "$size" "$file") || exit 1
  CK_PHP_CACERT_VERSION=$version
  CK_PHP_CACERT_LICENSE=$license
}

# ck_php_ext_flag <name> <builtin|shared>: prints the configure flag that
# builds the extension that way, or nothing when the line always builds it
# in. Dies for a name or class it does not know.
ck_php_ext_flag() {
  local name=$1 class=$2 shared= opt=${1//_/-} major=${CK_PHP_LINE%%.*}
  case "$class" in
    builtin) ;;
    shared) shared=shared ;;
    *) ck_die "php: unknown extension class: $class" ;;
  esac
  case "$name" in
    core | date | hash | pcre | random | reflection | spl | standard)
      [ -z "$shared" ] || ck_die "php: $name is always built in"
      ;;
    json)
      [ -z "$shared" ] || ck_die "php: json cannot be shared"
      # json is always built in from 8.0.
      if [ "$major" -lt 8 ]; then echo --enable-json; fi
      ;;
    opcache)
      # 7.4 builds opcache.so whatever the flag says; 8.5 always builds it in
      # and has no flag.
      if [ -n "$shared" ]; then echo --enable-opcache; fi
      ;;
    libxml)
      [ -z "$shared" ] || ck_die "php: libxml cannot be shared"
      echo --with-libxml
      ;;
    iconv)
      if [ -n "$CK_PHP_ICONV_DIR" ]; then
        echo "--with-iconv=${shared:+shared,}$CK_PHP_ICONV_DIR"
      else
        echo "--with-iconv${shared:+=shared}"
      fi
      ;;
    mysqli | pdo_mysql)
      [ -n "$shared" ] || ck_die "php: $name is built shared only"
      echo "--with-$opt=shared,mysqlnd"
      ;;
    curl | openssl | pdo_sqlite | sodium | sqlite3 | zlib)
      echo "--with-$opt${shared:+=shared}"
      ;;
    bcmath | calendar | ctype | dom | exif | fileinfo | filter | ftp | intl | \
      mbstring | mysqlnd | pcntl | pdo | phar | posix | session | shmop | \
      simplexml | soap | sockets | sysvmsg | sysvsem | sysvshm | tokenizer | \
      xml | xmlreader | xmlwriter)
      echo "--enable-$opt${shared:+=shared}"
      ;;
    *) ck_die "php: no configure flag known for the extension $name; add it to ck_php_ext_flag" ;;
  esac
}

# ck_php_line_cflags <line>: prints extra CFLAGS a line needs on today's
# compilers. Only lines that are built get an entry.
ck_php_line_cflags() {
  local os
  os=$(ck_os)
  case "$1" in
    7.4)
      # 7.4 predates C23 and the compilers that made these warnings errors;
      # without the flags configure's own probes also fail silently.
      printf '%s' "-std=gnu17 -Wno-error=implicit-function-declaration"
      printf ' %s' -Wno-error=incompatible-pointer-types -Wno-error=int-conversion
      if [ "$os" = macos ]; then
        printf ' %s' -Wno-error=incompatible-function-pointer-types
      fi
      ;;
  esac
}

# ck_php_configure_args <line>: fills CK_PHP_CONFIGURE.
ck_php_configure_args() {
  local line=$1 dir prefix name flag major=${1%%.*}
  local -a names
  dir=$(ck_php_dir "$line")
  prefix=$(ck_php_prefix "$line")
  # The ini folders: the app always passes -c or PHPRC, and these never
  # exist on a user's machine, where only root could create them.
  # --disable-rpath: relocation sets every library search path, so PHP adds
  # none of its own.
  CK_PHP_CONFIGURE=(
    --enable-option-checking=fatal
    --prefix="$prefix"
    --with-config-file-path="$dir/no-ini"
    --with-config-file-scan-dir="$dir/no-ini.d"
    --disable-all
    --enable-cli
    --enable-fpm
    --enable-cgi
    --disable-phpdbg
    --disable-debug
    --disable-rpath
    --with-pcre-jit
  )
  read -r -a names <<<"$CK_PHP_BUILTIN"
  for name in ${names[@]+"${names[@]}"}; do
    flag=$(ck_php_ext_flag "$name" builtin) || exit 1
    if [ -n "$flag" ]; then CK_PHP_CONFIGURE+=("$flag"); fi
  done
  read -r -a names <<<"$CK_PHP_DEFAULT $CK_PHP_OPTIONAL"
  for name in ${names[@]+"${names[@]}"}; do
    flag=$(ck_php_ext_flag "$name" shared) || exit 1
    if [ -n "$flag" ]; then CK_PHP_CONFIGURE+=("$flag"); fi
  done
  if [ "$major" -lt 8 ]; then
    # 7.x uses the system's crypt when it supports every algorithm; these
    # answers force PHP's bundled crypt, so libcrypt.so.1 (missing on Fedora
    # and Arch) is never needed. 8.x bundles it unless told otherwise.
    CK_PHP_CONFIGURE+=(
      ac_cv_crypt_des=no
      ac_cv_crypt_ext_des=no
      ac_cv_crypt_md5=no
      ac_cv_crypt_blowfish=no
      ac_cv_crypt_sha256=no
      ac_cv_crypt_sha512=no
    )
  fi
}

# ck_php_apply_patches <src> <line>: applies build/php/patches/<line>/*.patch
# in name order, with no fuzz, and records their names.
ck_php_apply_patches() {
  local src=$1 line=$2 patch
  CK_PHP_PATCHES=
  for patch in "$CK_REPO_ROOT/build/php/patches/$line"/*.patch; do
    [ -e "$patch" ] || continue
    if [ -L "$patch" ] || [ ! -f "$patch" ]; then
      ck_die "php: not a regular file: $patch"
    fi
    ck_php_run "$src" patch -p1 -N -t -F 0 -i "$patch"
    CK_PHP_PATCHES="${CK_PHP_PATCHES:+$CK_PHP_PATCHES }${patch##*/}"
  done
}

# ck_php_build <line>: builds the line from a fresh source tree into a fresh
# prefix. Loads the line, sets the hermetic environment and platform flags
# (left exported for relocation), and checks the stack is built.
ck_php_build() {
  local line=${1:-} os dir src prefix pin name version url sha size file archive
  local jobs cflags ldflags sdk
  ck_check_line "$line"
  ck_php_line_load "$line"
  os=$(ck_os)
  ck_hermetic_env "$CK_PHP_STACK"
  ck_platform_env
  ck_php_stack_load

  CK_PHP_ICONV_DIR=
  if [ "$os" = macos ]; then
    # macOS has no /usr/include; iconv.h is only in the SDK, and libiconv
    # resolves to /usr/lib/libiconv.2.dylib through the SDK's stub.
    sdk=$(xcrun --show-sdk-path) || ck_die "php: xcrun --show-sdk-path failed"
    [ -f "$sdk/usr/include/iconv.h" ] || ck_die "php: no iconv.h in the SDK at $sdk"
    CK_PHP_ICONV_DIR="$sdk/usr"
  fi

  ck_pins_load --line "$line"
  pin=$(ck_pin php) || exit 1
  IFS=$'\t' read -r name version url sha size _ _ file <<<"$pin"
  [ "$version" = "$CK_PHP_VERSION" ] || ck_die "php: the line's pin is $version, lock line said $CK_PHP_VERSION"
  CK_PHP_SOURCE_SHA256=$sha
  archive=$(ck_fetch "$name" "$version" "$url" "$sha" "$size" "$file") || exit 1

  dir=$(ck_php_dir "$line")
  src=$(ck_php_src_dir "$line")
  prefix=$(ck_php_prefix "$line")
  ck_run mkdir -p "$dir"
  ck_run rm -rf "$src" "$prefix" "$(ck_php_root "$line")"
  ck_unpack "$archive" "$src"
  [ -f "$src/configure" ] || ck_die "php: no configure script in $src"
  ck_php_apply_patches "$src" "$line"

  ck_php_configure_args "$line"
  cflags=$(ck_php_line_cflags "$line")
  ldflags=$LDFLAGS
  if [ "$os" = linux ]; then
    # Drops any library nothing uses, such as a libcrypt that configure
    # found but the bundled crypt never calls.
    ldflags="$ldflags -Wl,--as-needed"
  fi
  jobs=$(ck_jobs)
  # configure records CFLAGS and LDFLAGS in the Makefile.
  ck_php_run "$src" env CFLAGS="$CFLAGS${cflags:+ $cflags}" LDFLAGS="$ldflags" \
    ./configure "${CK_PHP_CONFIGURE[@]}"
  ck_php_run "$src" make -j"$jobs"
  ck_php_run "$src" make install
  for file in bin/php bin/php-cgi sbin/php-fpm; do
    [ -f "$prefix/$file" ] || ck_die "php: make install wrote no $prefix/$file"
  done
  ck_log "php: $CK_PHP_VERSION built into $prefix"
}
