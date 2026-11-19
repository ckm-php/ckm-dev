# Expected module lists

The extension lists of each PHP line on each OS, recorded from CI runs of `.github/workflows/php-build.yml`. They change only to match what a CI run showed.

| File                      | Holds                                        |
| ------------------------- | -------------------------------------------- |
| `<line>-<os>.builtin.txt` | The extensions compiled into `bin/php`       |
| `<line>-<os>.default.txt` | The shared extensions that are on by default |

- **Format:** one name per line, sorted with `LC_ALL=C sort`, LF line endings. Names are spelled as `catalog/php-lines.json` and the `.so` files spell them: lower case, and `opcache` rather than `Zend OPcache`.
- **No code reads these files yet.** The build checks the manifest's own lists: `smoke.sh` requires `php -n -m` to list exactly the manifest's `builtin` class, and `smoke.php` and `ck-runtime.json` use the same lists.

## Recorded

Run [44498482769](https://github.com/ckm-php/ckm-dev/actions/runs/44498482769) on 2026-11-19 (commit `f809e35`), for all four pairs: 7.4 and 8.5, on Linux x86_64 and macOS arm64.

- **`default`** is the archive's `ck-runtime.json` class. It matches the archive's listing: `ext/*.so` is exactly default plus optional. Each one loaded alone in the smoke runs of the PHP and contract jobs.
- **`builtin`** is the archive's `ck-runtime.json` class, which is copied from the manifest. Each name was confirmed loaded by the smoke test's `builtin` check (`smoke: ok (17 checks)` in the PHP, contract and bare-root runs). That run checked a subset only.
- PHP 7.4 builds opcache as a shared extension (`default`). PHP 8.5 always compiles it in (`builtin`) and has no `opcache.so`.
- The Linux and macOS lists of each line are the same.

**8.5 `builtin`, 2026-11-19 (round 10):** `lexbor` and `uri` were added to both OSes, and to the manifest. The macOS log of `php -n -m` showed them; the Linux list could not be read from its log and is assumed to be the same. The first post-push PHP build proves both: the smoke stage now fails unless `php -n -m` equals the manifest's list exactly.

Refresh these files only from a green run's log or from `ck-runtime.json` in its archives (`tar -xzOf <archive> ck-runtime.json`), never by running a downloaded PHP on a developer's machine.
