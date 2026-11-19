# shellcheck shell=bash
# OpenSSL: shared libssl and libcrypto, linked by PHP (openssl) and curl.
# The legacy stack pins the 1.1.1 series, the current stack 3.x
# (catalog/php-lines.json). Sourced by deps/all.sh.

# ck_dep_openssl: Configure takes CC, CFLAGS and LDFLAGS from the environment
# (ck_platform_env) only while its command line holds no compiler or linker
# flag, so only the target and options go there.
#
# - OPENSSLDIR, the default config file and CA store that libcrypto reads at
#   every start, is <null folder>/ssl (ck_platform_null_dir), which no account
#   can create, so the "no config file" default always holds. install_sw
#   writes nothing there; PHP gets its CA bundle from openssl.cafile.
# - 3.x: no-module builds the legacy provider into libcrypto, so loading it
#   needs no module path, and no engine or provider files are built.
#   no-docs skips the man pages.
# - 1.1.1: no-dynamic-engine builds the bundled engines into libcrypto, and
#   its default target builds no docs.
# - install_sw installs only libraries, headers, pkg-config files and the
#   openssl tool: no docs and nothing under OPENSSLDIR. The tool and
#   c_rehash are removed again, so they never shadow the system's on the
#   hermetic PATH, where <prefix>/bin comes first.
ck_dep_openssl() {
  local version src jobs os cpu target null
  local -a args
  version=$(ck_dep_version openssl)
  if ck_dep_is_built openssl "$version"; then return 0; fi
  os=$(ck_os)
  cpu=$(ck_cpu)
  case "$os-$cpu" in
    linux-x86_64) target=linux-x86_64 ;;
    linux-aarch64) target=linux-aarch64 ;;
    macos-aarch64) target=darwin64-arm64-cc ;;
    macos-x86_64) target=darwin64-x86_64-cc ;;
    *) ck_die "deps: no OpenSSL target for $os-$cpu" ;;
  esac
  null=$(ck_platform_null_dir)
  args=(
    "$target"
    --prefix="$CK_DEPS_PREFIX"
    --openssldir="$null/ssl"
    --libdir=lib
    shared
    no-tests
  )
  case "$version" in
    1.1.1*) args+=(no-dynamic-engine) ;;
    *) args+=(no-module no-docs) ;;
  esac
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack openssl "$version")
  ck_dep_need "$src/Configure"
  ck_dep_run "$src" ./Configure "${args[@]}"
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install_sw
  ck_run rm -f "$CK_DEPS_PREFIX/bin/openssl" "$CK_DEPS_PREFIX/bin/c_rehash"
  ck_dep_finish openssl "$version" libssl libcrypto
}
