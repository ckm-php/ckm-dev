# PHP build pipeline

ck-dev builds its own PHP for macOS and Linux (D7, D14). This folder holds the build scripts. They run in GitHub Actions on `ubuntu-22.04` (glibc 2.35) and `macos-15` (deployment target 12.0). Windows uses the official NTS zips and is not built here.

This file is the contract between the scripts, `crates/ck-release` and the app. Everything under [Frozen interfaces](#frozen-interfaces) changes only through a plan.

## Rules for the scripts

- Bash 3.2 features only (macOS ships 3.2). No associative arrays, `mapfile`, namerefs or `${var,,}`. Arguments are kept in arrays and expanded as `"${args[@]}"`. An array that may be empty is expanded as `${args[@]+"${args[@]}"}`, because bash before 4.4 fails on it under `set -u`.
- Every script sources `lib/common.sh` first. It sets `set -euo pipefail` and `umask 022`.
- No `sudo` anywhere. The workflow creates the build root on the CI runner. Nothing creates it on a developer's machine.
- Every download is pinned in `catalog/sources.lock.json` and checked against its SHA256 before use. A mismatch deletes the file and fails. The build never refreshes a pin.
- Compilers and tools run in the hermetic environment (`ck_hermetic_env`). Its `PATH` is the stack's `bin/` plus `/usr/bin:/bin:/usr/sbin:/sbin`, never `/usr/local` or Homebrew. `pkg-config` lookups use `PKG_CONFIG_LIBDIR` only, never `PKG_CONFIG_PATH`.

## Library stacks

Every library PHP links is built from pinned source, as a shared library, into one prefix per stack. Everything that links libssl uses the same libssl.

| Stack     | PHP lines   | OpenSSL |
| --------- | ----------- | ------- |
| `legacy`  | 7.4 and 8.0 | 1.1.1   |
| `current` | 8.1 and up  | 3       |

`catalog/php-lines.json` gives each line its stack. For a library whose two stacks need different versions, it names a version series per stack (`"openssl": "1.1.1"`), and the lock holds one pin in each series. Every other library has a single pin that both stacks share. `ck-release lock export --stack <stack>` prints the stack's pins, one per library: every pin except `php`, so it includes the build tools and the CA bundle, and on macOS also `patchelf`. Recipes pick the entries they need by name.

The build tools `pkgconf` and `patchelf` (Linux) are built from pins into each stack's `bin/`. They are never shipped.

## Build root layout

The build root is fixed: `/opt/ck-dev-build`. Only root can create it on a user's machine, so no path baked into a binary can point to a folder a user controls. It cannot be overridden. The download cache can be, with `CK_CACHE_DIR`, for local self-tests.

```text
/opt/ck-dev-build/
  cache/<name>-<version>/<file>   verified downloads, one folder per pin
  src/<stack>/<name>-<version>/   unpacked library sources
  deps/<stack>/                   the stack's install prefix:
    bin/                            pkgconf, patchelf and library tools
    include/
    lib/                            shared libraries
    lib/pkgconfig/
    share/
  php/<line>/
    src/                          unpacked PHP source tree
    prefix/                       PHP's --prefix (`make install` writes here)
    root/                         the relocated archive tree that `pack` packs
  out/                            archives and their descriptors
```

`<file>` is the last segment of the pin's URL (the eighth field of `lock export`). Stages: `deps` (library stacks), `php` (configure, make, install into `prefix/`), `relocate` (copy into `root/`, fix library paths, sign on macOS), `audit` (static checks) and `smoke` (run tests).

## Archive layout

One `tar.gz` per PHP version, OS and CPU, named `php-<version>-<os>-<cpu>.tar.gz`, for example `php-8.5.12-ck1-linux-x86_64.tar.gz`. It unpacks as is into `runtimes/php/<version>/`, with no wrapping folder and no links.

```text
bin/php  bin/php-fpm  bin/php-cgi
ext/<name>.so            shared extensions, in one flat folder
lib/<soname>             bundled libraries, under their soname, no symlinks
etc/php.ini-development  etc/php.ini-production  etc/ssl/cert.pem
licenses/<component>/    the licence and notice of PHP and of each library
ck-runtime.json          version, line, build revision, OS, CPU, thread safety,
                         module API, extension classes, each library's version
                         and SHA256, baseline, toolchain
```

- **Version:** `<php version>-ck<build revision>`, such as `8.5.12-ck1`. A rebuild of the same PHP raises the revision. A version folder is never overwritten.
- **Thread safety:** NTS only.
- **Linux:** `DT_RPATH` is `$ORIGIN/../lib` for `bin/` and `ext/`, and `$ORIGIN` for `lib/`. The only system libraries allowed are the glibc family, `libstdc++.so.6` and `libgcc_s.so.1`. PHP's bundled crypt is forced, so `libcrypt.so.1` is never linked.
- **macOS:** `@rpath` install names with `@loader_path/../lib`, then an ad-hoc `codesign`. Only an explicit allow-list of `/usr/lib` libraries and system frameworks may be linked.
- **Packing:** `ck-release pack` writes every folder and regular file sorted by path, with mtime 0, uid and gid 0, empty owner names, and mode 0755 (folders and executables) or 0644. It refuses links, special files and any name `ck_core::extract` would refuse. It runs on Linux and macOS only.

## Relocation contract

Nothing about the install location is baked in. The app (M3) passes it at run time, and the smoke test passes exactly these:

| Setting                         | How it is passed                                                    |
| ------------------------------- | ------------------------------------------------------------------- |
| The ini file                    | `-c <ini>` or `PHPRC`                                               |
| The ini scan folder             | `PHP_INI_SCAN_DIR`                                                  |
| The extension folder            | `extension_dir=<root>/ext` in the ini                               |
| FPM prefix, pid, log and socket | `php-fpm -p <dir> -y <conf> -c <ini> -F`                            |
| CA bundle                       | `openssl.cafile` and `curl.cainfo` set to `<root>/etc/ssl/cert.pem` |
| `include_path`                  | `include_path="."`; no PEAR, `php-config`, `phpize` or headers ship |

## Frozen interfaces

### Tracked files

- **`catalog/sources.lock.json`:** `{"schema": 1, "entries": [...]}`. Each entry is `{name, version, url, sha256, size, license, signature}`, in that key order. Only `ck-release lock set` writes it, and `ck-release` refuses a file that is not byte for byte its canonical form (entries sorted by name, then version; two-space JSON; one trailing newline). Prettier ignores it.
  - `name`: `[a-z0-9][a-z0-9-]{0,31}`. The PHP source is `php`.
  - `version`: `ck-core`'s runtime version rule. A `php` version is `X.Y.Z`, and its line `X.Y` must be in `php-lines.json`, with at most one pin per line.
  - `url`: HTTPS, written in normal form, with no user, port, query or fragment. Its host is exactly one of `www.php.net`, `github.com`, `download.gnome.org`, `www.sqlite.org` or `curl.se`. Its last segment is a safe file name: `[A-Za-z0-9][A-Za-z0-9._+-]*`, at most 128 bytes, no `..`.
  - `sha256`: 64 lower-case hex characters. `size`: 1 byte to 2 GiB.
  - `license`: SPDX, 1 to 128 bytes, no control characters.
  - `signature`: what the pin refresh checked upstream besides the hash: `gpg`, `minisig`, `sha256-file` or `none`.
  - No two entries share a name and version (versions compare ignoring case). Unknown fields are refused.
- **`catalog/php-lines.json`:** hand-edited, holds no pins. `{"schema": 1, "stacks": {<stack>: {<library>: <series>}}, "lines": [...]}`. Each line is `{line, stack, build_revision, license, extensions: {builtin, default, optional}}`.
  - `build_revision` starts at 1. `license` is the catalog licence, PHP's own (`PHP-3.01`); library licences ship inside the archive.
  - Extension names are lower case, as configure and the `.so` files spell them. `builtin` is compiled in, `default` is shared and on, `optional` is shared and off. A name is in one class only.
  - A series matches a version that equals it, or starts with it followed by anything but a digit: `3.5` matches `3.5.9` but not `3.50.1`, and `1.1.1` matches `1.1.1w`.

### `ck-release` CLI

The tool is built from this repository and never shipped. Scripts call it through `ck_release`, which runs `$CK_RELEASE_BIN` (an absolute path set before the build). Exit codes: 0 ok, 1 any failure (one `ck-release: <message>` line on stderr), 2 usage. Data goes to stdout only.

```text
ck-release lock [--lock <file>] [--manifest <file>] check
ck-release lock [...] export [--stack <legacy|current> | --line <X.Y>]
ck-release lock [...] line <X.Y>
ck-release lock [...] set <name> <version> --url <url> --sha256 <hex> --size <bytes>
                          --license <spdx> --signature <gpg|minisig|sha256-file|none>
ck-release pack <root> --out <file.tar.gz> --descriptor <file.json>
                --name <name> --version <version> --os <linux|macos>
                --cpu <x86_64|aarch64> --license <spdx>
ck-release payload build --descriptors <dir> --base-url <url> --sequence <n>
                         --expires <rfc3339> --out <file.json>
```

- `--lock` and `--manifest` default to `catalog/sources.lock.json` and `catalog/php-lines.json` in the repository the tool was built from. Every `lock` command first loads and checks both files.
- `check` prints `ok: <n> pins, <m> PHP lines`. A lock that does not pin everything yet passes.
- `export` with `--stack` fails when a library the stack names a series for has no pin in it. With `--line` it fails when the line has no `php` pin.
- `set` adds or replaces the entry with that name and version, checks both files, then replaces the lock atomically. It prints nothing.
- `pack` requires the archive's file name to be `<name>-<version>-<os>-<cpu>.tar.gz`, and writes the descriptor.
- `payload build` reads every `*.json` in `--descriptors`, and writes the unsigned payload in `ck-core`'s catalog schema 1 as two-space JSON. Each URL is `<base url>/<file>` and must pass the app's download policy (HTTPS on `github.com`).

### `lock export` lines

One line per entry, eight fields separated by a single tab, in this order. No field is empty or holds a tab or newline, so no quoting is needed.

```text
name  version  url  sha256  size  license  signature  file
```

Capture the output first, so a failed export stops the script. A process substitution (`done < <(...)`) would hide the exit status.

```bash
pins=$(ck_release lock export --stack current)
[ -n "$pins" ] || ck_die "no pins for stack current"
while IFS=$'\t' read -r name version url sha256 size license signature file; do
  ...
done <<<"$pins"
```

### `lock line` output

`key<TAB>value` lines in this order. List values are separated by single spaces and may be empty. Read it the same way: capture it, then loop over `<<<"$info"` with `IFS=$'\t' read -r key value`.

```text
line      8.5
php       8.5.12
version   8.5.12-ck1
revision  1
stack     current
license   PHP-3.01
builtin   bcmath calendar core ...
default   curl exif fileinfo ...
optional  ftp shmop soap ...
```

### Descriptor

`pack` writes one per archive and `payload build` reads them. Unknown fields are refused. `os`, `cpu` and `format` take `ck-core`'s catalog values; `cpu` becomes the catalog's `arch`.

```json
{
  "schema": 1,
  "name": "php",
  "version": "8.5.12-ck1",
  "license": "PHP-3.01",
  "os": "linux",
  "cpu": "x86_64",
  "file": "php-8.5.12-ck1-linux-x86_64.tar.gz",
  "sha256": "<64 lower-case hex>",
  "size": 31457280,
  "format": "tar.gz"
}
```

### `lib/common.sh`

| Function                      | Does                                                                                                                                                                                                                           |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ck_log <message...>`         | Prints `ck-build: <message>` to stderr                                                                                                                                                                                         |
| `ck_die <message...>`         | Prints `ck-build: error: <message>` to stderr and exits 1                                                                                                                                                                      |
| `ck_require_cmd <command...>` | Exits 1 unless every command is on `PATH`                                                                                                                                                                                      |
| `ck_run <argv...>`            | Prints the command, shell-quoted, to stderr, then runs it                                                                                                                                                                      |
| `ck_os`                       | Prints `linux` or `macos`                                                                                                                                                                                                      |
| `ck_cpu`                      | Prints `x86_64` or `aarch64`                                                                                                                                                                                                   |
| `ck_jobs`                     | Prints the job count: `CK_JOBS`, or the CPU count                                                                                                                                                                              |
| `ck_check_stack <stack>`      | Exits 1 unless `legacy` or `current`                                                                                                                                                                                           |
| `ck_check_line <line>`        | Exits 1 unless the line looks like `8.5`                                                                                                                                                                                       |
| `ck_build_root`               | Prints `/opt/ck-dev-build`                                                                                                                                                                                                     |
| `ck_cache_dir`                | Prints `CK_CACHE_DIR` (absolute) or `<root>/cache`                                                                                                                                                                             |
| `ck_deps_prefix <stack>`      | Prints `<root>/deps/<stack>`                                                                                                                                                                                                   |
| `ck_src_dir <stack>`          | Prints `<root>/src/<stack>`                                                                                                                                                                                                    |
| `ck_php_dir <line>`           | Prints `<root>/php/<line>`                                                                                                                                                                                                     |
| `ck_php_src_dir <line>`       | Prints `<root>/php/<line>/src`                                                                                                                                                                                                 |
| `ck_php_prefix <line>`        | Prints `<root>/php/<line>/prefix`                                                                                                                                                                                              |
| `ck_php_root <line>`          | Prints `<root>/php/<line>/root`                                                                                                                                                                                                |
| `ck_out_dir`                  | Prints `<root>/out`                                                                                                                                                                                                            |
| `ck_hermetic_env <stack>`     | Clears compiler, linker, loader and Homebrew variables, then exports `CK_STACK`, `CK_DEPS_PREFIX`, `PATH`, `PKG_CONFIG`, `PKG_CONFIG_LIBDIR`, `CPPFLAGS=-I<prefix>/include`, `LDFLAGS=-L<prefix>/lib`, `LC_ALL=C` and `TZ=UTC` |
| `ck_release <args...>`        | Runs `$CK_RELEASE_BIN`                                                                                                                                                                                                         |

Sourcing also defines the read-only `CK_BUILD_ROOT` and `CK_REPO_ROOT`. It creates no folder. Path helpers only print. Per-OS flags (compilers, rpath, deployment target) are added by `lib/platform-linux.sh` and `lib/platform-macos.sh` on top of `ck_hermetic_env`.

### `build.sh` stages

`/bin/bash build/php/build.sh <stage> <arg>`. Exit codes: 0 ok, 1 failure, 2 usage. `deps` and `php` need an absolute `CK_RELEASE_BIN` and a writable `/opt/ck-dev-build`. `audit` and `bare-root` need neither, so they run on a fresh runner.

| Stage                    | Does                                                                                                                                                                                    |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `deps <legacy\|current>` | Builds the stack into `<root>/deps/<stack>`. It also saves each library's licence files in `share/ck-licenses/<name>-<version>/`, which the cached prefix carries to the `php` stage.   |
| `php <X.Y>`              | Builds PHP for the line, then relocates, audits, smoke-tests and packs it. Stdout holds only two lines: the archive path, then the descriptor path, both in `<root>/out`.               |
| `audit <root>`           | The static audit (T6) of an unpacked root                                                                                                                                               |
| `bare-root <archive>`    | T7. Linux runs the archive in a bare root holding only glibc, its iconv codecs and the allow-listed system libraries (`docker import`, no network). macOS runs it with Homebrew hidden. |

### `lib/fetch.sh`

| Function                                                 | Does                                                                                                                                                                                      |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ck_pins_load <lock export args...>`                     | Loads `ck-release lock export` output into `CK_PINS`. It does this once per process.                                                                                                      |
| `ck_pin <name>`                                          | Prints that name's single line from `CK_PINS`                                                                                                                                             |
| `ck_fetch <name> <version> <url> <sha256> <size> <file>` | Ensures `<cache>/<name>-<version>/<file>` exists and is verified (size, then SHA256), then prints its path. A mismatch deletes the file and fails, and it never downloads again silently. |
| `ck_unpack <archive> <dest>`                             | Unpacks a tarball into a new `dest`. It refuses absolute paths, `..`, more than one top-level entry, special files, and links that leave the tree.                                        |
| `ck_fetch_unpack <name> <parent>`                        | Runs `ck_pin`, then `ck_fetch`, then `ck_unpack` into `<parent>/<name>-<version>/`                                                                                                        |

### Platform and library recipes

- **`lib/platform-{linux,macos}.sh`:**
  - `ck_platform_env` exports the compilers and flags for that OS: `-O2 -fPIC`; on macOS also the 12.0 deployment target and `-headerpad_max_install_names`.
  - `ck_platform_shlib_ext` prints `so` or `dylib`.
  - `ck_platform_rpath <bin|ext|lib>` prints the rpath for each folder.
- **`deps/all.sh`:** `ck_deps_build <stack>` builds every library in order: pkgconf, patchelf (Linux only), zlib, openssl, libxml2, sqlite, oniguruma, libsodium, icu, curl.
- **`deps/<name>.sh`:** each defines `ck_dep_<name>`. It skips the build when the stamp `<prefix>/.stamps/<name>-<version>` already exists.

### Keys

`keys/<component>.gpg` and `.fingerprints` hold the keyring and the allowed signers that `refresh-pins.sh` verifies against. `keys/<component>.unsigned` records why a component has no signature check (zlib: its key is on no approved host). `keys/README.md` lists where each key came from.
