# shellcheck shell=bash
# zlib: shared, linked by PHP (zlib), libxml2 and curl. Sourced by
# deps/all.sh.

# ck_dep_zlib: zlib's own configure script, not autoconf: it takes CC,
# CFLAGS and LDFLAGS from the environment and refuses unknown options.
ck_dep_zlib() {
  local version src jobs
  version=$(ck_dep_version zlib)
  if ck_dep_is_built zlib "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack zlib "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure --prefix="$CK_DEPS_PREFIX" --libdir="$CK_DEPS_PREFIX/lib"
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_dep_finish zlib "$version" libz
}
