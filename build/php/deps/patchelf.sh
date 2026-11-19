# shellcheck shell=bash
# patchelf: rewrites ELF library search paths during relocation. A Linux-only
# build tool that is never shipped. Sourced by deps/all.sh.

# ck_dep_patchelf: installs <prefix>/bin/patchelf. Only the binary is copied,
# so no docs, man page or shell completions land in the prefix.
ck_dep_patchelf() {
  local version src jobs os
  os=$(ck_os)
  [ "$os" = linux ] || ck_die "deps: patchelf is built on Linux only"
  version=$(ck_dep_version patchelf)
  if ck_dep_is_built patchelf "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack patchelf "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure --prefix="$CK_DEPS_PREFIX" --disable-dependency-tracking
  ck_dep_run "$src" make -j"$jobs"
  ck_run mkdir -p "$CK_DEPS_PREFIX/bin"
  ck_run install -m 0755 "$src/src/patchelf" "$CK_DEPS_PREFIX/bin/patchelf"
  ck_dep_finish patchelf "$version" bin/patchelf
}
