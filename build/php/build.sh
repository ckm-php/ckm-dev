#!/usr/bin/env bash
# The PHP build pipeline's entry point (build/php/README.md). CI only.
#
#   build.sh deps <legacy|current>
#       Builds the library stack into $(ck_deps_prefix <stack>), from pinned
#       source, and saves each library's licence under
#       <prefix>/share/ck-licenses/<name>-<version>/.
#
#   build.sh php <X.Y>
#       Needs the line's stack, already built (or restored) at its prefix.
#       Builds PHP into $(ck_php_prefix <X.Y>), assembles and relocates
#       $(ck_php_root <X.Y>) with ck-runtime.json and licences, audits it
#       (T6), runs the smoke test on it, then packs it with `ck-release pack`
#       into $(ck_out_dir):
#         php-<version>-<os>-<cpu>.tar.gz   the archive
#         php-<version>-<os>-<cpu>.json     its descriptor
#       Prints the archive path, then the descriptor path, one per line.
#
#   build.sh audit <root>
#       T6, the static audit, on an unpacked or relocated root.
#
#   build.sh bare-root <archive.tar.gz>
#       T7: Linux runs the archive in a bare root file system under Docker;
#       macOS runs it with Homebrew and the /usr/local library folders
#       hidden by the workflow.
#
# Only the paths that `php` prints go to stdout; every log and every tool's
# output goes to stderr. Exit 1 on any failure, 2 on bad usage.
#
# Environment:
#   CK_RELEASE_BIN  absolute path of ck-release; deps and php only
#   CK_JOBS         optional parallel job count
#   CK_CACHE_DIR    optional absolute download cache folder
#
# deps and php need /opt/ck-dev-build to exist and be writable. The workflow
# creates it with sudo on the runner; no script here runs sudo. audit and
# bare-root use system tools only and need neither the build root nor
# ck-release.

CK_BUILD_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=deps/all.sh
. "$CK_BUILD_DIR/deps/all.sh"
# shellcheck source=php.sh
. "$CK_BUILD_DIR/php.sh"
# shellcheck source=audit.sh
. "$CK_BUILD_DIR/audit.sh"
# shellcheck source=relocate.sh
. "$CK_BUILD_DIR/relocate.sh"
# shellcheck source=smoke.sh
. "$CK_BUILD_DIR/smoke.sh"
# shellcheck source=bare-root.sh
. "$CK_BUILD_DIR/bare-root.sh"

ck_build_usage() {
  cat >&2 <<'EOF'
usage: build.sh deps <legacy|current>
       build.sh php <X.Y>
       build.sh audit <root>
       build.sh bare-root <archive.tar.gz>
EOF
  exit 2
}

# ck_build_needs_root: dies unless the build root is a writable folder.
ck_build_needs_root() {
  local root
  root=$(ck_build_root)
  if [ -L "$root" ] || [ ! -d "$root" ] || [ ! -w "$root" ]; then
    ck_die "$root must be a writable folder; the workflow creates it on the runner"
  fi
}

# ck_build_needs_release: dies unless CK_RELEASE_BIN names an executable by
# its absolute path, before any work starts.
ck_build_needs_release() {
  case "${CK_RELEASE_BIN:-}" in
    /*) ;;
    *) ck_die "set CK_RELEASE_BIN to the absolute path of ck-release" ;;
  esac
  [ -x "$CK_RELEASE_BIN" ] || ck_die "not executable: $CK_RELEASE_BIN"
}

# ck_php_pack <line>: packs the relocated root and prints the archive and
# descriptor paths on fd 3, the original stdout.
ck_php_pack() {
  local line=$1 os cpu asset out root
  os=$(ck_os)
  cpu=$(ck_cpu)
  root=$(ck_php_root "$line")
  out=$(ck_out_dir)
  asset="php-$CK_PHP_RELEASE-$os-$cpu"
  ck_run mkdir -p "$out"
  ck_run ck_release pack "$root" --out "$out/$asset.tar.gz" --descriptor "$out/$asset.json" \
    --name php --version "$CK_PHP_RELEASE" --os "$os" --cpu "$cpu" --license "$CK_PHP_LICENSE"
  printf '%s\n' "$out/$asset.tar.gz" "$out/$asset.json" >&3
}

ck_build_main() {
  local command=${1:-} arg=${2:-} root
  [ "$#" -eq 2 ] || ck_build_usage
  case "$command" in
    deps)
      ck_check_stack "$arg"
      ck_build_needs_release
      ck_build_needs_root
      ck_deps_build "$arg"
      ck_deps_save_licenses
      ;;
    php)
      ck_check_line "$arg"
      ck_build_needs_release
      ck_build_needs_root
      ck_php_build "$arg"
      ck_php_relocate "$arg"
      root=$(ck_php_root "$arg")
      ck_audit "$root"
      CK_PHP_EXPECT_BUILTIN=$CK_PHP_BUILTIN
      ck_smoke "$root" "$CK_PHP_VERSION"
      ck_php_pack "$arg"
      ;;
    audit)
      [ -d "$arg" ] || ck_die "not a folder: $arg"
      root=$(cd "$arg" && pwd -P) || ck_die "cannot resolve $arg"
      ck_audit "$root"
      ;;
    bare-root)
      ck_bare_root "$arg"
      ;;
    *) ck_build_usage ;;
  esac
}

# Tools print to stdout; keep it for the paths `php` prints.
exec 3>&1 1>&2
ck_build_main "$@"
