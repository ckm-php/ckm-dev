# shellcheck shell=bash
# curl: shared libcurl, linked by PHP (curl), built against the stack's own
# OpenSSL and zlib only. Sourced by deps/all.sh.

# ck_dep_curl: every optional library curl would detect on the runner is
# named off, so libcurl links nothing outside the stack and the system. The
# list covers the curl versions both stacks may pin; autoconf only warns
# about an option a version does not have.
#
# - No CA bundle or CA path is baked in; PHP passes curl.cainfo.
# - LDAP is off, so neither the runner's libldap nor macOS's is linked.
# - No docs or built-in manual.
# - The curl and wcurl tools are removed again: <prefix>/bin comes first on
#   the hermetic PATH, and a curl with no CA store would break every later
#   download (lib/fetch.sh runs `curl`).
ck_dep_curl() {
  local version src jobs prefix=$CK_DEPS_PREFIX
  version=$(ck_dep_version curl)
  if ck_dep_is_built curl "$version"; then return 0; fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack curl "$version")
  ck_dep_need "$src/configure"
  ck_dep_run "$src" ./configure \
    --prefix="$prefix" \
    --enable-shared \
    --disable-static \
    --disable-dependency-tracking \
    --with-openssl="$prefix" \
    --with-zlib="$prefix" \
    --without-ca-bundle \
    --without-ca-path \
    --without-ca-fallback \
    --without-libpsl \
    --without-brotli \
    --without-zstd \
    --without-libidn2 \
    --without-nghttp2 \
    --without-nghttp3 \
    --without-ngtcp2 \
    --without-libssh \
    --without-libssh2 \
    --without-librtmp \
    --without-libgsasl \
    --without-gssapi \
    --disable-ldap \
    --disable-ldaps \
    --disable-docs \
    --disable-manual
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_run rm -f "$prefix/bin/curl" "$prefix/bin/wcurl"
  ck_dep_finish curl "$version" libcurl
}
