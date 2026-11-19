# shellcheck shell=bash
# SQLite: shared libsqlite3, linked by PHP (sqlite3, pdo_sqlite), built from
# the sqlite-autoconf tarball. Sourced by deps/all.sh.

# ck_dep_sqlite: the tarball's configure is autosetup, not autoconf. It
# refuses unknown options, so only options it documents are passed.
#
# - COLUMN_METADATA: pdo_sqlite's getColumnMeta() reports the table only
#   when sqlite3_column_table_name exists. FTS4, FTS5 and R*Tree are common
#   in applications; JSON and math functions are on by default.
# - The shell gets no readline, so it never links the system's.
# - Linux: --soname=legacy gives libsqlite3.so.0, the usual soname; the
#   default is none.
ck_dep_sqlite() {
  local version src jobs os
  local -a args
  version=$(ck_dep_version sqlite)
  if ck_dep_is_built sqlite "$version"; then return 0; fi
  os=$(ck_os)
  args=(
    --prefix="$CK_DEPS_PREFIX"
    --disable-static
    --disable-readline
    --enable-fts4
    --enable-fts5
    --enable-rtree
  )
  if [ "$os" = linux ]; then
    args+=(--soname=legacy)
  fi
  jobs=$(ck_jobs)
  src=$(ck_dep_unpack sqlite "$version")
  ck_dep_need "$src/configure"
  # configure records CFLAGS in the Makefile, so make needs no override.
  ck_dep_run "$src" env CFLAGS="$CFLAGS -DSQLITE_ENABLE_COLUMN_METADATA=1" \
    ./configure "${args[@]}"
  ck_dep_run "$src" make -j"$jobs"
  ck_dep_run "$src" make install
  ck_dep_finish sqlite "$version" libsqlite3
}
