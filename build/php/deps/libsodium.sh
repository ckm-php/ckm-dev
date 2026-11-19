# shellcheck shell=bash
# libsodium: shared, linked by PHP (sodium). Sourced by deps/all.sh.

# ck_dep_libsodium: a plain autoconf build with the default, portable
# optimisation; --enable-opt, which optimises for the build machine's CPU,
# stays off.
ck_dep_libsodium() {
  local version src jobs
  version=$(ck_dep_version libsodium)
  if ck_dep_is_built libsodium "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack libsodium "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure \
    --prefix="$CK_DEPS_PREFIX" \
    --enable-shared \
    --disable-static \
    --disable-dependency-tracking
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_dep_finish libsodium "$version" libsodium
}
