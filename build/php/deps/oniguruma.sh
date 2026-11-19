# shellcheck shell=bash
# Oniguruma: shared libonig, linked by PHP (mbstring). Sourced by
# deps/all.sh.

# ck_dep_oniguruma: a plain autoconf build; it has no optional dependencies.
ck_dep_oniguruma() {
  local version src jobs
  version=$(ck_dep_version oniguruma)
  if ck_dep_is_built oniguruma "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack oniguruma "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure \
    --prefix="$CK_DEPS_PREFIX" \
    --enable-shared \
    --disable-static \
    --disable-dependency-tracking
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_dep_finish oniguruma "$version" libonig
}
