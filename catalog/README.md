# Catalog

The runtime catalog lists every file ckm-dev downloads: the exact URL, SHA256, size, version, OS and CPU of each (D3). It is signed and published on GitHub Releases (D6). The app checks it with `ck_core::catalog` (see "Runtime catalog and downloader" in `docs/ARCHITECTURE.md`).

This folder holds no catalog data yet. M2 adds the files and the script that publishes them. Everything below was built in M1b (round 7) and is **provisional** until the owner confirms it (`docs/plans/round-7.md`).

## Format

There is one published file per schema, `catalog-v1.json`. It is a single JSON envelope, with no detached signature file:

```json
{
  "format": 1,
  "key_id": "<16 lower-case hex>",
  "payload": "<standard base64, with padding>",
  "signature": "<standard base64 of 64 bytes>"
}
```

- **`signature`:** Ed25519 over the bytes `ck-dev catalog v1\n` followed by the decoded payload bytes.
- **`key_id`:** the first 8 bytes of the SHA-256 of the 32-byte public key, in hex.
- **Limits:** the file is at most 1 MiB. Unknown fields are refused at every level.

The decoded payload (schema 1):

```json
{
  "schema": 1,
  "sequence": 7,
  "expires": 1790000000,
  "runtimes": [
    {
      "name": "php",
      "version": "8.4.3",
      "license": "PHP-3.01",
      "artifacts": [
        {
          "os": "linux",
          "arch": "x86_64",
          "url": "https://github.com/<owner>/<repo>/releases/download/<tag>/php-8.4.3-linux-x86_64.tar.gz",
          "sha256": "<64 lower-case hex>",
          "size": 31457280,
          "format": "tar.gz"
        }
      ]
    }
  ]
}
```

- **`sequence`:** must be higher in every catalog published after this one. An app refuses a lower sequence, and refuses the same sequence with different contents.
- **`expires`:** Unix seconds. Nothing is accepted or installed at or after it; versions already installed keep working. The suggested lifetime is 90 days.
- **`name`:** `[a-z0-9][a-z0-9-]{0,31}`, and not a Windows device name.
- **`version`:** `[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}`, with no `..`, no trailing dot, and no Windows device name as its stem. Two versions that differ only in case are duplicates.
- **`os`:** `linux`, `macos` or `windows`.
- **`arch`:** `x86_64` or `aarch64`.
- **`format`:** `tar.gz` or `zip`.
- **`size`:** the exact size, from 1 byte to 2 GiB.
- **`url`:** starts at `https://github.com/`. GitHub's redirects to `release-assets.githubusercontent.com` or `objects.githubusercontent.com` are followed, at most 5.
- **Archives:** unpacked as they are into `runtimes/<name>/<version>/`, with no strip-components. They hold only files, folders and relative symlinks that stay inside the archive. Windows archives hold no links at all.

## The release key (owner, offline)

The app trusts only the public keys built into it, in `crates/ck-core/src/catalog/trust.rs`. All three slots are empty now, so a release build refuses every catalog. The private key never enters the repository, logs or CI. No signing tool exists in the repository yet; make and keep the key pair offline.

1. **Add the key.** Paste the 32 public-key bytes into `RELEASE_KEYS`.
2. **Set the URL.** Set `CATALOG_URL` to the published `catalog-v1.json`, as a download URL with a fixed tag: `https://github.com/<owner>/<repo>/releases/download/<fixed-tag>/catalog-v1.json`. Do not use a `releases/latest/download/…` URL, which breaks the "no latest URLs" rule. The policy also allows `github.com` only for the first request, never for a redirect. So any URL whose first redirect stays on `github.com` would be refused; GitHub may answer a `latest` URL that way. None of this has been tried against GitHub yet.
3. **At every app release,** raise `MIN_SEQUENCE` to the sequence of the newest published catalog. Then a fresh install cannot be handed an older catalog. An updated app also stops reading a cached catalog below the new floor: nothing new installs until its next refresh succeeds, while installed versions keep working.
4. **To rotate the key:**
   1. Add the new public key next to the old one, and release the app.
   2. Publish a catalog signed with the new key, with a **higher sequence**. An app that already holds the same sequence and payload keeps its cached copy, which is signed with the old key.
   3. Once that catalog is out, remove the old key in a later release.

## Checking a release build

`cargo test -p ck-core --release --locked --test release_trust` checks two things. A release build refuses a catalog signed by a throwaway test key. It also refuses plain HTTP to loopback. The test asserts the refusal, not an empty slot, so it keeps passing after the real key is added.
