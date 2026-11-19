# shellcheck shell=bash
# pkgconf: the stack's pkg-config, a static build tool that is never shipped.
# Sourced by deps/all.sh.

# ck_dep_pkgconf: installs <prefix>/bin/pkgconf (ck_hermetic_env's
# PKG_CONFIG) and a pkg-config alias, for configure scripts that look the
# tool up by name. It reads only PKG_CONFIG_LIBDIR, and its compiled-in
# search path is the prefix's own. The system folders are the real system
# ones, so pkgconf never drops the prefix's -I and -L flags from its output.
ck_dep_pkgconf() {
  local version src jobs prefix=$CK_DEPS_PREFIX
  version=$(ck_dep_version pkgconf)
  if ck_dep_is_built pkgconf "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack pkgconf "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure \
    --prefix="$prefix" \
    --disable-shared \
    --enable-static \
    --disable-dependency-tracking \
    --with-pkg-config-dir="$prefix/lib/pkgconfig:$prefix/share/pkgconfig" \
    --with-system-libdir=/usr/lib \
    --with-system-includedir=/usr/include
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_run ln -sf pkgconf "$prefix/bin/pkg-config"
  ck_dep_finish pkgconf "$version" bin/pkgconf bin/pkg-config
}
