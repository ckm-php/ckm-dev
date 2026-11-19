# shellcheck shell=bash
# libxml2: shared, linked by PHP (libxml, dom, xml, SimpleXML, xmlreader,
# xmlwriter, soap). The legacy stack pins the 2.11 series, the current stack
# 2.15 (catalog/php-lines.json); the soname differs (libxml2.so.2 before
# 2.14, libxml2.so.16 after). Sourced by deps/all.sh.

# ck_dep_libxml2: the flags cover both series; autoconf only warns about an
# option a series no longer has (lzma and ftp are gone in 2.15).
#
# - zlib: ours, named explicitly; 2.13 and later leave it off by default.
# - iconv: the system's, which is glibc on Linux and /usr/lib/libiconv on
#   macOS; the hermetic environment keeps Homebrew's out.
# - Off: Python bindings, lzma, ICU, readline and history (xmllint only),
#   and the HTTP and FTP clients.
ck_dep_libxml2() {
  local version src jobs
  version=$(ck_dep_version libxml2)
  if ck_dep_is_built libxml2 "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack libxml2 "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure \
    --prefix="$CK_DEPS_PREFIX" \
    --enable-shared \
    --disable-static \
    --disable-dependency-tracking \
    --with-zlib="$CK_DEPS_PREFIX" \
    --without-python \
    --without-lzma \
    --without-icu \
    --without-readline \
    --without-history \
    --without-http \
    --without-ftp
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_dep_finish libxml2 "$version" libxml2
}
