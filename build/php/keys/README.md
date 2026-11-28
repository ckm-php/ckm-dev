# Upstream signing keys

`refresh-pins.sh` checks upstream signatures against the files in this folder. The build itself never reads them: it trusts only the SHA256 pins in `catalog/sources.lock.json`.

## Files

| File                       | Holds                                                               |
| -------------------------- | ------------------------------------------------------------------- |
| `<component>.gpg`          | The pinned OpenPGP keyring: the upstream public keys, as downloaded |
| `<component>.fingerprints` | The allow-list: which keys in that keyring may sign the component   |
| `libsodium.minisign.pub`   | libsodium's minisign public key, in minisign's own two-line format  |
| `<component>.unsigned`     | Instead of the two above: why no keyring is pinned for a component  |

GPG components: `php`, `openssl`, `curl`, `icu` and `zlib`. Each needs both its `.gpg` and its `.fingerprints` file, or else a `.unsigned` note. A component with none of these, or with no listed fingerprint, cannot be pinned: `refresh-pins.sh` stops rather than record a weaker check.

A `.unsigned` note is for a component whose signing key is not published on an approved host. Its lines, without `#` comments, give the reason, which must not be empty. `refresh-pins.sh` then skips the signature with a warning and records only what it did check (`sha256-file` or `none`). A note next to a keyring stops the script. The owner reviews each note in the diff.

## Keyring

- A `.gpg` file may be binary or ASCII-armoured. `gpgv` reads only binary keyrings, so `refresh-pins.sh` de-armours an armoured one into its temporary folder first (with `gpg --dearmor`, which needs no agent and no keyring).
- `gpgv` runs with `--homedir` set to a new, empty temporary folder and `--keyring` set to this file, so the user's own keyring is never read or changed.
- `gpgv` treats every key in the keyring as trusted and does not check expiry or revocation (`man gpgv`). That is why the allow-list exists.

## Allow-list format

```text
# Comments start with '#'. Blank lines are ignored.
0123456789ABCDEF0123456789ABCDEF01234567  # Name of the key's owner
```

- One fingerprint per line: exactly 40 hex digits. Spaces inside it are allowed, so a fingerprint can be copied as upstream prints it. Case does not matter.
- Any other line stops the script.
- List the **primary key** fingerprint. `refresh-pins.sh` reads gpgv's `--status-fd` output. GnuPG's `DETAILS` file defines `VALIDSIG`: its first argument is the fingerprint of the key that made the signature (a subkey, when a subkey signed), and its tenth, when present, is the primary key's. A signature passes when either one is on the list.
- At least one valid signature from a listed key is required. A `BADSIG`, `ERRSIG` or `REVKEYSIG` line, or a non-zero exit from `gpgv`, fails the check.

The fingerprint above is a dummy that only shows the format.

## Filling and changing the keys

The keys are pinned by a person, never by a script or CI:

1. Download the upstream key file from the component's own key page.
2. Compare each key's fingerprint with a second source, such as the release announcement or the project's site.
3. Save the keyring as `<component>.gpg`, and list the fingerprints, with each owner's name, in `<component>.fingerprints`.
4. Record below where each one came from.

The owner reviews these files in the diff, like the lock.

Pinned on 2026-11-19. PHP's allow-list was extended on 2026-11-28 with the 8.2, 8.3 and 8.4 signers and re-cross-checked against the existing 8.5 signer. Each `.fingerprints` file repeats its sources.

| Component   | Key source (file as downloaded)                                                                                                           | Second source for the fingerprints                                                                                                                                                  |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `php`       | `https://www.php.net/distributions/php-keyring.gpg`                                                                                       | `https://www.php.net/gpg-keys.php`                                                                                                                                                  |
| `openssl`   | `packaging/common/openssl-release-keys.asc` in `https://github.com/openssl/packages`, commit `4de93290fbaaf299701a6511dab30e7bc6b3baae`   | `https://openssl-library.org/source/` and `doc/fingerprints.txt` in `https://github.com/openssl/openssl`                                                                            |
| `curl`      | `mykey.asc` in `https://github.com/bagder/daniel.haxx.se`, commit `fc396d15ec73e9f7b75f62c60349d36e8f3ca8f3`                              | `docs/VERIFY.md` in `https://github.com/curl/curl`                                                                                                                                  |
| `icu`       | `KEYS` in `https://github.com/unicode-org/icu`, commit `9e68dceba9994dfc5b11d8e9c4987b0d323f6d46`                                         | None: ICU publishes no fingerprint outside `KEYS`                                                                                                                                   |
| `zlib`      | None on an approved host: `zlib.unsigned`                                                                                                 | —                                                                                                                                                                                   |
| `libsodium` | `installation/README.md` in `https://github.com/jedisct1/libsodium-doc`, commit `38a790805a1848fd7badb02c109e146cac6755ac` (minisign key) | `README.md` in `https://github.com/jedisct1/minisign` and `https://doc.libsodium.org/installation`. `minisign` was not installed for the 1.0.22 refresh, so that pin records `none` |

PHP's release managers change per line. The `php` list must hold the managers of every line in `catalog/php-lines.json`.

## Without a signature

- **libsodium:** signed with minisign. When `minisign` is not installed, `refresh-pins.sh` prints a warning and records the pin with signature `none`, because the lock records only what was really checked. Install `minisign` and run the refresh again to record `minisig`.
- **zlib:** see `zlib.unsigned`. The pin records `none`.
- **sqlite, pkgconf and patchelf:** nothing upstream is checked besides the download. The pin records `none`, and the owner reviews the hash.
