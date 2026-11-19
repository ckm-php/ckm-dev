# shellcheck shell=bash
# ICU4C: shared libicuuc, libicui18n, libicuio and libicudata, linked by PHP
# (intl). The legacy stack pins the 74 series, the current stack 79
# (catalog/php-lines.json). Sourced by deps/all.sh.

# ck_dep_icu: built out of tree with ICU's configure, which takes CC, CXX,
# CFLAGS, CXXFLAGS and LDFLAGS from the environment, so the macOS deployment
# target from ck_platform_env reaches every object.
#
# - C++17, which current ICU requires; the older series builds with it too.
#   configure records CXXFLAGS in the Makefiles, so make needs no override.
# - The data is one shared library, libicudata.
# - Tools stay on: the data build needs them. Samples, tests, extras (uconv)
#   and the layout library are off. libicuio stays: PHP's intl needs it.
# - macOS: --enable-rpath gives the libraries absolute install names, like
#   every other dylib in the prefix; without it ICU uses bare names. Linux
#   gets its runpath from LDFLAGS.
ck_dep_icu() {
  local version src jobs os icu_src build candidate
  local -a args
  version=$(ck_dep_version icu)
  if ck_dep_is_built icu "$version"; then return 0; fi
  os=$(ck_os)
  args=(
    --prefix="$CK_DEPS_PREFIX"
    --enable-shared
    --disable-static
    --disable-samples
    --disable-tests
    --disable-extras
    --disable-layoutex
    --with-data-packaging=library
  )
  if [ "$os" = macos ]; then
    args+=(--enable-rpath)
  fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack icu "$version")
  # The archives hold icu/source/, and ck_fetch_unpack drops the top folder.
  # Also allow the repository's layout, icu4c/source/.
  icu_src=
  for candidate in "$src/source" "$src/icu4c/source"; do
    if [ -f "$candidate/configure" ]; then
      icu_src=$candidate
      break
    fi
  done
  [ -n "$icu_src" ] || ck_die "deps: icu: no source/configure under $src (did the archive layout change?)"
  build="$src/ck-build"
  ck_run rm -rf "$build"
  ck_run mkdir -p "$build"
  ck_dep_run "$build" env CXXFLAGS="$CXXFLAGS -std=c++17" \
    "$icu_src/configure" "${args[@]}"
  ck_dep_run "$build" make -j"$jobs"
  ck_dep_run "$build" make install
  ck_dep_finish icu "$version" libicuuc libicui18n libicuio libicudata
}
