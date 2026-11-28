# Catalog

The runtime catalog lists every file ckm-dev downloads: the exact URL, SHA256, size, version, OS and CPU of each (D3). The owner signs it offline, and it is published on GitHub Releases (D6, D25). The app checks it with `ck_core::catalog` (see "Runtime catalog and downloader" in `docs/ARCHITECTURE.md`).

Where everything lives:

- **Runtime releases.** Each runtime version is one release, tagged `<name>-<version>`, for example `php-8.5.12-ck1`. It holds one archive per OS, each archive's descriptor (`.json`) and `SHA256SUMS`. The "PHP publish" workflow (`.github/workflows/php-publish.yml`) publishes it from a clean CI build and attests each archive. After that, the release is never changed.
- **The catalog.** `catalog-v1.json` lives on the fixed release `catalog-v1` and is replaced in place at each re-signing. Its URL is compiled into the app (`CATALOG_URL` in `crates/ck-core/src/catalog/trust.rs`): `https://github.com/ckm-php/ckm-dev/releases/download/catalog-v1/catalog-v1.json`.
- **The keys.** Two Ed25519 release keys (A, the primary, and B, the backup) have their public halves compiled into the app. Their private halves stay with the owner, always encrypted.

No catalog is published yet. The owner's two public keys are compiled into `RELEASE_KEYS` (key A signs; key B is the offline backup), so a release build accepts only a catalog signed by one of them. The first catalog will list PHP 8.5.13 only: the existing immutable PHP 8.5.12 and 7.4.33 releases are not approved for installation from the catalog. Do not change or delete those releases.

## Tracked files

| File                | What it holds                                                                                                                                                                                                                  | Who writes it                                                                                                                                                   |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `sources.lock.json` | One pin for each source file the PHP build downloads: name, version, URL, SHA256, size, licence, and what the pin refresh checked upstream besides the hash (`gpg`, `minisig`, `sha256-file` or `none`).                       | Only `ck-release lock set`, which `build/php/refresh-pins.sh` calls when a person refreshes a pin. The owner reviews the diff. The build and CI never write it. |
| `php-lines.json`    | The PHP lines we build. Each line has its library stack, build revision, catalog licence and extension classes (built in, default, optional). Each stack names the version series of the libraries that differ between stacks. | By hand, through a plan. It holds no pins.                                                                                                                      |
| `payload-v1.json`   | The unsigned payload of the published catalog: the exact bytes that were signed. It arrives with the first data-only commit (runbook step 9).                                                                                  | Only `ck-release payload build`, run by the owner. Prettier ignores it.                                                                                         |
| `catalog-v1.json`   | The signed envelope that is uploaded to the `catalog-v1` release. The next signing reads it as `--previous`. It arrives with the first data-only commit.                                                                       | Only `ck-release sign`, run by the owner. Prettier ignores it.                                                                                                  |

- The rules for the first two files are in `build/php/README.md` ("Frozen interfaces"). `ck-release` refuses a lock that is not in its canonical form, an unknown field, a URL outside the build hosts (`www.php.net`, `github.com`, `download.gnome.org`, `www.sqlite.org`, `curl.se`) and duplicates.
- Check both: `cargo run -p ck-release -- lock check`.
- A PHP patch refresh replaces the previous source pin for the same minor line.
- The build hosts are for the build and the pin refresh only. The app's downloader still allows only `github.com` and GitHub's asset hosts.
- A commit that changes only `payload-v1.json` and `catalog-v1.json` does not rebuild PHP: the PHP build's path filter names only the lock and `php-lines.json` in this folder.

## Format

The format and the app's checks below were built in M1b (round 7) and confirmed by the owner on 2026-11-19 (`docs/plans/round-7.md`).

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
  "sequence": 1,
  "expires": 1802822400,
  "runtimes": [
    {
      "name": "php",
      "version": "8.5.12-ck1",
      "license": "PHP-3.01",
      "artifacts": [
        {
          "os": "linux",
          "arch": "x86_64",
          "url": "https://github.com/ckm-php/ckm-dev/releases/download/php-8.5.12-ck1/php-8.5.12-ck1-linux-x86_64.tar.gz",
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
- **`expires`:** Unix seconds. Nothing is accepted or installed at or after it; versions already installed keep working. A catalog lives 90 days and is re-signed at least every 60. `sign` accepts an expiry 7 to 100 days ahead.
- **`name`:** `[a-z0-9][a-z0-9-]{0,31}`, and not a Windows device name.
- **`version`:** `[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}`, with no `..`, no trailing dot, and no Windows device name as its stem. Two versions that differ only in case are duplicates.
- **`os`:** `linux`, `macos` or `windows`.
- **`arch`:** `x86_64` or `aarch64`.
- **`format`:** `tar.gz` or `zip`.
- **`size`:** the exact size, from 1 byte to 2 GiB.
- **`url`:** starts at `https://github.com/`. GitHub's redirects to `release-assets.githubusercontent.com` or `objects.githubusercontent.com` are followed, at most 5. `payload build` writes each URL as `https://github.com/ckm-php/ckm-dev/releases/download/<name>-<version>/<file>`, and `sign` refuses any other.
- **Archives:** unpacked as they are into `runtimes/<name>/<version>/`, with no strip-components. They hold only files, folders and relative symlinks that stay inside the archive. Windows archives hold no links at all.

## Publishing and signing: the owner's runbook

**Ground rules:**

- **Tags cannot be reused.** Once immutable releases are on, a published runtime release can never be changed, and the tag of a deleted one can never be used again. So:
  - Publishing creates the tags last, after every check passed.
  - Never delete, edit or re-upload a published runtime release. A bad build gets a new build revision (`-ck2`) instead.
- **The private keys never enter the repository, CI, logs or an AI session.** Steps D, F and every signing run happen in a terminal with **no** Claude Code or other agent session open. Never pass a passphrase on the command line (`-pass`), and never write a decrypted key to disk.
- **Where to run:** from the repository root, with `gh` signed in as the owner. `sign` and `key check` run on Linux and macOS only, and refuse when `CI` or `GITHUB_ACTIONS` is set.
- **OpenSSL 3** (`openssl version`). On macOS the system `openssl` is LibreSSL, which may not make Ed25519 keys. Homebrew's `openssl@3` does.
- **The tool that touches a key is built from `main` as published.** Before step 7 and every re-signing, before anything writes to `catalog/`:

  ```bash
  git fetch origin
  git status --porcelain                                  # must print nothing
  git rev-parse HEAD origin/main                          # the two lines must be equal
  cargo build -q --locked -p ck-release
  ```

  Then call the built binary, `./target/debug/ck-release`, never `cargo run`: nothing is rebuilt between the check and the key, and no build output mixes with the passphrase prompt.

- **`gh` 2.51 or later** (`gh --version`) for `gh attestation verify --signer-workflow` in step 7. Older versions lack the flag (2.49 and 2.50 lack `--signer-workflow`; 2.48 has no `attestation` at all), so update `gh` first.

### Before the push

Steps A to D can be done at any time, in any order. Then E (commit locally, no push), F (check the keys) and G (push), in that order. The public keys must be compiled in before E.

**A. Repository settings.**

1. Under Settings → Actions → General, set the approval for fork pull request workflows to "Require approval for all outside collaborators".
2. Under Settings → Rules → Rulesets, add a branch ruleset:
   - Target: the default branch (`main`).
   - Enforcement: Active, with an empty bypass list.
   - Rules: "Restrict deletions" and "Block force pushes".
3. Optional: under Settings → Actions → General, require actions to be pinned to a full-length commit SHA. Every workflow here already pins by SHA.

**B. The `release` environment.** It **must exist before the first publish run**: a job that names a missing environment creates it with no protection, and would then publish without your approval. Under Settings → Environments, add `release` with:

- **Required reviewers:** you.
  - Leave "Prevent self-review" off. You start the publish run and approve it yourself, and that setting forbids exactly that.
- **Deployment branches and tags:** selected branches only, `main`.
- **"Allow administrators to bypass configured protection rules":** off.
- **Secrets:** none.
- **Environment variables:** exactly one, `RELEASE_GATE` with the value `armed`. Add it under this environment's own "Environment variables", never as a repository or organization variable.

Why the variable: the `publish` job refuses to run unless `RELEASE_GATE` is exactly `armed`. A job that names a missing environment creates it with no protection, so without the variable a deleted or never-created `release` environment would publish with no approval. The variable exists only inside the protected environment, so the job fails closed instead. A job also sees repository and organization variables of the same name, which is why it must never be set at those levels.

Check the environment right before every publish:

```bash
gh api repos/ckm-php/ckm-dev/environments/release \
  --jq '{name, protection_rules, deployment_branch_policy}'
gh variable list -e release -R ckm-php/ckm-dev   # must list RELEASE_GATE  armed
gh variable list -R ckm-php/ckm-dev              # must not list RELEASE_GATE
gh variable list -o ckm-php                      # organizations only: must not list RELEASE_GATE
```

- An error (404) means the environment does not exist: stop and create it.
- The output must show a rule whose `reviewers` lists you, with `prevent_self_review` false, and a branch policy that is not `null`.
- The API does not show the administrator bypass. Confirm it is off under Settings → Environments → `release`.

**C. The catalog release, then immutability.** First check under Settings → General → Releases that release immutability is still **off**. Then:

```bash
gh release create catalog-v1 -R ckm-php/ckm-dev --target main --latest=false \
  --title "Runtime catalog (schema 1)" --notes "Signed runtime catalog for CK Dev."
mkdir -p ~/ck-release-work
printf 'ck-dev transport probe\n' > ~/ck-release-work/transport-probe.txt
gh release upload catalog-v1 ~/ck-release-work/transport-probe.txt -R ckm-php/ckm-dev
gh release view catalog-v1 -R ckm-php/ckm-dev --json assets --jq '.assets[].name'   # transport-probe.txt
```

Only then turn on release immutability (Settings → General → Releases). Right after, upload the probe again:

```bash
gh release upload catalog-v1 ~/ck-release-work/transport-probe.txt -R ckm-php/ckm-dev --clobber
```

It must succeed. If it is refused, the catalog release became immutable: stop, and ask before pushing anything, because the catalog's URL is compiled into the app.

- Immutability applies only to releases published after it is turned on. So `catalog-v1` stays replaceable: later steps upload `catalog-v1.json` to it and delete the probe. Every runtime release published from then on is locked.
- The catalog's protection is its signature, not immutability.
- Never delete the `catalog-v1` release or its tag, because its URL is compiled into the app.
- If a later upload to `catalog-v1` is ever refused as immutable, stop and ask: moving the catalog to another tag needs a new app build.

**D. Key ceremony (once).** Run it with no AI session open.

1. **Make a passphrase** with your password manager's generator: a **6-word** generated passphrase (preferred, because you can type it), or 20 or more random characters. Never choose one yourself. Store it in the password manager before you go on.
2. Make key A. The key is created in memory and encrypted on its way to the file (PBKDF2-SHA256 with 600,000 iterations), so the unencrypted key never touches the disk. The `[ ! -e … ] &&` guard stops a re-run from destroying an existing key, because `-out` empties the file before it asks for the passphrase:

   ```bash
   umask 077
   mkdir -p ~/.ck-dev-keys
   [ ! -e ~/.ck-dev-keys/ck-catalog-key-a.pem ] && openssl genpkey -algorithm ed25519 | \
     openssl pkcs8 -topk8 -v2 aes-256-cbc -iter 600000 -out ~/.ck-dev-keys/ck-catalog-key-a.pem
   # asks for the passphrase twice (from the terminal): paste the generated one
   head -n 1 ~/.ck-dev-keys/ck-catalog-key-a.pem
   # must print -----BEGIN ENCRYPTED PRIVATE KEY-----
   openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem -passin pass: -noout
   # must FAIL ("bad decrypt"): an empty passphrase must be refused
   openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem -pubout | \
     openssl pkey -pubin -outform DER | tail -c 32 | xxd -p -c 32
   # asks for the passphrase: TYPE it by hand, then it prints 64 hex characters: key A's public line
   ```

   - **Paste** at the two encryption prompts with the terminal's real paste key: Ctrl+Shift+V in a Linux terminal, Cmd+V on macOS. Never Ctrl+V: it can insert nothing or a stray character, and the prompt shows nothing either way.
   - **Type** the passphrase by hand at the public-key command's prompt. Do not paste it there. If the typed passphrase is refused, the paste failed and the key is locked with the wrong passphrase: `rm -f ~/.ck-dev-keys/ck-catalog-key-a.pem` and redo the key from the start.
   - **The empty-passphrase check** (`-passin pass:`) uses an empty literal on purpose; it carries no secret. Good: it prints errors and exits non-zero. Bad: it prints nothing and exits 0, which means the key was encrypted with an empty passphrase: delete the file and redo. This does not catch every bad paste (a stray character such as `\r` is not empty), which is why the hand-typed step matters.
   - **Ground rule:** never put the real passphrase in `-pass`, `-passout` or `-passin` arguments.
   - Without `xxd`, use `od -An -tx1 | tr -d ' \n'; echo` in its place.
   - Type the public-key line exactly as shown. With `-pubin` the second `openssl pkey` only ever writes a public key, so a wrong passphrase or a missing `-pubout` prints an error and no hex. Never send hex from any other command: it could be the private key.
   - If the two passphrases differed, `pkcs8` stops with an error. Delete the file it may have left (`rm -f ~/.ck-dev-keys/ck-catalog-key-a.pem`) and run the command again.

3. Do the same for key B with its own generated passphrase: in every command change the file name to `~/.ck-dev-keys/ck-catalog-key-b.pem` (the guard then tests `-e` on that file).
4. Copy both encrypted `.pem` files to your cloud drive.
5. Delete key B from the PC (`rm ~/.ck-dev-keys/ck-catalog-key-b.pem`), so the cloud keeps the only copy of the backup.
6. Send the two public lines, labelled A and B. They are public; the `.pem` files and the passphrases are not.

**If you already ran an older step D** (`openssl genpkey -algorithm ed25519 -aes-256-cbc -out …`): that form protects the key with only 2,048 PBKDF2 iterations. Check a key file with:

```bash
openssl asn1parse -in ~/.ck-dev-keys/ck-catalog-key-a.pem | grep INTEGER
```

The iteration count is printed in hex: `:0800` is 2,048 (the old form), and `:0927C0` is 600,000 (the form above). The public keys are not compiled in yet, so replacing old keys costs nothing: make new keys A and B with the steps above, send the new public lines, and delete every old `.pem` file, on the PC and in the cloud drive, including the drive's version history and trash.

**E. Commit locally (no push yet).** The one commit holds the publish workflow, offline signing, the release keys and the round-9 fixes. Make it now, and check the tree is clean:

```bash
git status --porcelain     # must print nothing
```

**F. Check the keys,** after they are compiled into `RELEASE_KEYS`, the gate passes and the commit from E exists. Again, no AI session open. Build from that commit and use the binary:

```bash
cargo build -q --locked -p ck-release
openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem | ./target/debug/ck-release key check --key -
```

- It prints three lines: `public_key`, `key_id` and `trust`.
- `public_key` must equal the line you sent, and `trust` must read `compiled release key`.
- If `public_key` differs from the line you sent, destroy that key (the `.pem` file and every copy of it) and redo step D for it. Never send a line again for a key that already exists. Then change the keys and amend or redo the commit from E before any push.
- The command exits 0 either way, so read the `trust` line.
- Note each key's `key_id`: every envelope names the key that signed it by this id.
- Do the same for key B, from its cloud copy (`openssl pkey -in <path of the cloud copy>`).

**G. Push once.** Only after F passed for both keys. Pushing starts CI and the PHP build. Nothing publishes by itself.

### After the push

**1. Wait for green.** CI and the PHP build of the pushed commit must pass (`gh run list -R ckm-php/ckm-dev -L 10`). If anything is red, fix it first; nothing below runs on a red commit.

**2. Probe.** This is the first real TLS handshake on each OS, the first GitHub redirect, and the strict URL parser on GitHub's real `Location`.

```bash
gh workflow run "Catalog check" -R ckm-php/ckm-dev -f mode=probe
```

- It must pass on Linux, macOS and Windows.
- `probe` works only while `transport-probe.txt` is on the `catalog-v1` release (until step 7).
- The weekly run of the same workflow starts with the push. It stays red until the first catalog is uploaded; that is expected.

**3. Clean candidate.** Pass all three inputs:

```bash
gh workflow run "PHP build" -R ckm-php/ckm-dev -f clean=true -f lines='8.5' -f oses='linux macos'
gh run list -R ckm-php/ckm-dev --workflow php-build.yml --event workflow_dispatch -L 3 \
  --json databaseId,headSha,status,conclusion
```

- The `oses` default (`linux`) is refused for a clean build. Build only 8.5 for this candidate; a PHP 7.4 release already exists and must not be republished.
- A clean run restores no cache, so it takes longer than a test run, and nothing cancels it.
- Note its run id (`databaseId`) and commit (`headSha`). The commit must be `main`'s current head.
- **From now until the publish run, push nothing to `main`.** Publishing requires the candidate's commit to be `main`'s head. A push in between means a new candidate.
- **Publish within 7 days.** The candidate's files are kept for 7 days.

**4. Your manual checks.** Download the candidate's release set into a new, empty folder:

```bash
gh run download <run id> -R ckm-php/ckm-dev -n release-set -D ~/ck-release-work/candidate
```

- It holds one folder per tag, each with the archives, their descriptors and `SHA256SUMS`.
- Unpack each archive into a new folder of its own and run `bin/php -v` there:
  - the macOS archives on a Mac without Homebrew;
  - the Linux archives on a Linux that is not Ubuntu.
- These are downloaded binaries, so use a throwaway folder, account or VM.
- Keep the hashes (`sha256sum` or `shasum -a 256`) for step 5.

**5. Publish.** First run the environment check from step B. Then confirm that release immutability is on: Settings → General → Releases shows "Enable release immutability" selected. A release published while it is off stays changeable for good. The API gives the same answer: `gh api repos/ckm-php/ckm-dev/immutable-releases` prints `"enabled": true`, and a 404 means it is off. Then:

```bash
gh workflow run "PHP publish" -R ckm-php/ckm-dev -f run_id=<run id> -f commit=<headSha, 40 hex>
```

1. The `prepare` job checks, with read-only permissions and before anything is created:
   - that the candidate is a clean, successful "PHP build" dispatch on `main` at this commit;
   - that each archive matches its descriptor and `SHA256SUMS`, and each version is the manifest's, with both platforms;
   - that no release or tag of those versions exists.

   It writes a summary of tags, files, sizes and SHA256 hashes. Compare the hashes with the files you checked.

2. The run then waits at `publish`. In the run's page, choose "Review deployments", then `release`, then "Approve and deploy". Reject it if anything differs.
   - While the job runs, no one should create the candidate's tags or edit or publish its drafts. GitHub does not provide an atomic check-and-publish operation.
3. `publish` then works in two phases:
   - It first refuses unless the `release` environment holds `RELEASE_GATE=armed` (step B).
   - It attests each archive in Sigstore's public log, signed by this workflow (`php-publish.yml`) for these exact files, and creates each release as a draft at the commit. It uploads the files, downloads them again and compares them byte for byte.
   - Only then does it publish the drafts. It checks each draft's verified asset list again before publishing and checks each new tag's commit before publishing the next draft. **This uses up the `-ck1` tags.**
4. Check: `gh release list -R ckm-php/ckm-dev`.
5. Each release names its attestation. `gh attestation verify <archive> --repo ckm-php/ckm-dev --signer-workflow ckm-php/ckm-dev/.github/workflows/php-publish.yml` (`gh` 2.51 or later) checks a downloaded archive against it. Step 7 runs it on every archive.

**6. If publishing fails.**

- **"Check the release environment" refused:** the `release` environment is missing or lacks `RELEASE_GATE=armed`. Nothing was created. Fix the environment (step B), then dispatch "PHP publish" again.
- **In `prepare`, or before you approve:** nothing was created. Fix the cause, then dispatch "PHP publish" again for the same candidate (still at `main`'s head and within 7 days).
- **In `publish`, before "Publish the drafts":** no tag exists yet. The job reports the releases it can identify, but never deletes them.
  - Check Releases for leftovers even if the report is empty. GitHub can create a draft without returning its ID, and it may have an `untagged-…` URL. Delete only an affected release that is still marked **Draft**.
  - Then dispatch "PHP publish" again. Use a new dispatch rather than "Re-run jobs", so `prepare`'s checks run again.
- **During "Publish the drafts":** check the summary and the Releases page for versions already published.
  - Those releases are final. Never delete, edit or re-upload them.
  - Check for leftover drafts and delete them by hand only if still marked **Draft**.
  - The same candidate cannot be published again, because `prepare` refuses a set when any of its tags exists. Build a new clean candidate with only the missing lines (for example `-f lines='8.5'`, still with `-f oses='linux macos'`), then repeat steps 4 and 5 for it. It is a new build, so check it again.

**7. Payload, sign, upload.** No AI session open from here to the end of this step.

- The releases folder must be new and empty, and must hold only the release folders the catalog lists.
- Run the commands one at a time, and stop at the first one that fails. A failed check, `sign` or `verify` must never be followed by the upload or by deleting the probe.

First, the checkout and the tool (ground rules): `git fetch origin`, `git status --porcelain` prints nothing, `git rev-parse HEAD origin/main` prints two equal lines, then `cargo build -q --locked -p ck-release`. Then download the releases and check where each archive came from:

```bash
mkdir -p ~/ck-release-work && mkdir ~/ck-release-work/releases
gh release download php-8.5.13-ck1 -R ckm-php/ckm-dev -D ~/ck-release-work/releases/php-8.5.13-ck1
( set -e; n=0
  for f in ~/ck-release-work/releases/*/*.tar.gz; do
    gh attestation verify "$f" --repo ckm-php/ckm-dev \
      --signer-workflow ckm-php/ckm-dev/.github/workflows/php-publish.yml
    n=$((n+1))
  done
  test "$n" -eq 2
  echo "all $n archives verified" )
```

It must end with `all 2 archives verified`. Any failure stops it with a non-zero exit and no such line; stop there. Then build the payload and compare its hashes with what you approved:

```bash
./target/debug/ck-release payload build --releases ~/ck-release-work/releases \
  --sequence 1 --expires <about 90 days ahead, such as 2027-02-17T00:00:00Z> --out catalog/payload-v1.json
git status --porcelain     # only catalog/payload-v1.json
jq -e '
  .sequence == 1 and
  (.runtimes | length == 1) and
  (.runtimes[0] | .name == "php" and .version == "8.5.13-ck1" and
    ([.artifacts[] | "\(.os)/\(.arch)"] | sort == ["linux/x86_64", "macos/aarch64"]))
' catalog/payload-v1.json
jq -r '.runtimes[].artifacts[] | "\(.sha256)  \(.url | split("/") | last)"' catalog/payload-v1.json
```

Each hash must equal the one for the same file in the publish run's summary (the `prepare` job's table, which you compared with your own checks in steps 4 and 5). Then:

```bash
./target/debug/ck-release assets verify --payload catalog/payload-v1.json --releases ~/ck-release-work/releases
openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem | \
  ./target/debug/ck-release sign --payload catalog/payload-v1.json --key - --first --out catalog/catalog-v1.json
./target/debug/ck-release verify catalog/catalog-v1.json --payload catalog/payload-v1.json
gh release upload catalog-v1 catalog/catalog-v1.json -R ckm-php/ckm-dev
gh release download catalog-v1 -R ckm-php/ckm-dev -p catalog-v1.json -O - | cmp - catalog/catalog-v1.json
gh release delete-asset catalog-v1 transport-probe.txt -R ckm-php/ckm-dev -y
```

- **The expiry,** 90 days from today at midnight UTC:
  - Linux: `date -u -d '+90 days' +%Y-%m-%dT00:00:00Z`
  - macOS: `date -u -v+90d +%Y-%m-%dT00:00:00Z`
- **`payload build`** measures every archive and checks it against its descriptor and `SHA256SUMS`. It refuses a stray file or folder.
- **`assets verify`** prints `ok: 2 artifacts match the release files`. It is mandatory: never sign a payload it did not pass.
- **Read `catalog/payload-v1.json` before signing.** It must contain only PHP `8.5.13-ck1` with two artifacts, Linux x86_64 and macOS arm64, both under `https://github.com/ckm-php/ckm-dev/releases/download/`.
- **`sign`** writes nothing unless every rule passes:
  - the payload is in canonical form, and its URLs are exactly where `payload build` puts them;
  - the expiry is 7 to 100 days ahead;
  - the sequence is at least the compiled `MIN_SEQUENCE`;
  - with `--first`, the output does not exist yet;
  - the new envelope verifies under the compiled keys.

  It prints `key_id` (key A's), `sequence`, `expires`, `runtimes` and `artifacts`. `verify` prints the same lines, from the compiled keys only.

- **The `cmp` line** prints nothing when the uploaded file equals yours.

**8. Full check.**

```bash
gh workflow run "Catalog check" -R ckm-php/ckm-dev -f mode=run
```

On Linux and macOS it fetches and verifies the catalog, installs each PHP into a temporary isolated profile, and runs `php -v` and the smoke script. On Windows it fetches and verifies only, since there is no Windows PHP yet.

**9. Data-only commit.** Commit `catalog/payload-v1.json` and `catalog/catalog-v1.json` alone:

```text
chore(catalog): catalog sequence 1 (php 8.5.13-ck1)
```

Push it only after publishing is complete: it moves `main`'s head, so any later publish needs a new candidate anyway.

### Re-signing (at least every 60 days)

Re-sign at least every 60 days (a catalog lives 90), whenever the weekly check fails, and to add or remove a version. A new runtime version is first published with steps 3 to 6.

1. **Check the checkout and build the tool,** as in the ground rules: after `git fetch origin`, `git status --porcelain` prints nothing and `git rev-parse HEAD origin/main` prints two equal lines; then `cargo build -q --locked -p ck-release`.
2. **Check the previous envelope.** The local copy must be what is published:

   ```bash
   gh release download catalog-v1 -R ckm-php/ckm-dev -p catalog-v1.json -O - | cmp - catalog/catalog-v1.json
   jq .sequence catalog/payload-v1.json   # the current sequence, n
   ```

   `verify` also prints the sequence, but it refuses a catalog that has already expired.

3. **Download the releases** the new catalog lists into a new, empty folder: every release in the current payload, plus any newly published one.

   ```bash
   jq -r '.runtimes[] | "\(.name)-\(.version)"' catalog/payload-v1.json | sort -u
   ```

4. **Check and build the payload,** as in step 7: `gh attestation verify` on every archive, then `payload build` with `--sequence <n+1>` and a new expiry about 90 days ahead, then the hash comparison (for a newly published version, against its publish run's summary), then `assets verify`. `git diff catalog/payload-v1.json` must show only the sequence and the expiry, plus any versions you meant to add or remove.

5. **Sign against the previous envelope,** which is overwritten atomically:

   ```bash
   openssl pkey -in ~/.ck-dev-keys/ck-catalog-key-a.pem | \
     ./target/debug/ck-release sign --payload catalog/payload-v1.json --key - \
       --previous catalog/catalog-v1.json --out catalog/catalog-v1.json
   ./target/debug/ck-release verify catalog/catalog-v1.json --payload catalog/payload-v1.json
   ```

6. **Replace the published file,** then check it:

   ```bash
   gh release upload catalog-v1 catalog/catalog-v1.json -R ckm-php/ckm-dev --clobber
   gh release download catalog-v1 -R ckm-php/ckm-dev -p catalog-v1.json -O - | cmp - catalog/catalog-v1.json
   ```

   The file may be missing for a moment during the replace. An app whose refresh fails then keeps its cached catalog until its next start.

7. **Finish:** dispatch "Catalog check" with `mode=run`, then make the data-only commit `chore(catalog): catalog sequence <n+1>`.

### The weekly check

- "Catalog check" runs every Monday at 06:17 UTC in `run` mode (`fetch` on Windows).
- It fails on any fetch, verify or install error, and when fewer than 21 days of the catalog's lifetime are left (`CK_PUBLISHED_MIN_DAYS`). A red weekly run means: re-sign now.
- GitHub turns off scheduled workflows in a public repository after 60 days with no activity. The re-signing commits keep it on; if it was turned off, turn it on again on the Actions page.

### Keys: rotation, loss and compromise

Key A signs; key B waits in the cloud. Both public keys are compiled into every app, so the app accepts either one without an update.

- **Switch to key B** (A is lost, its passphrase is forgotten, or A is retired):
  1. Sign the next catalog with B, at a higher sequence as always. Use the cloud copy, or a copy in a private folder (`umask 077`) that you delete afterwards.
  2. In the next app release:
     - remove A from `RELEASE_KEYS`;
     - add a new backup key from a new ceremony (step D);
     - raise `MIN_SEQUENCE` to the sequence of the first catalog signed with B.
  3. B becomes the primary, kept on the PC and in the cloud. The new key is the cloud-only backup.
- **A leaked encrypted file** is harmless only while its passphrase stays strong and secret. Still switch keys when convenient.
- **A compromised key** (both the file and its passphrase exposed):
  1. Sign and publish a new catalog with the other key at once.
  2. Release an app without the compromised key, as above.
  3. Review who can write to this repository.
  - Until users update, their app still trusts the old key. A catalog it signs reaches them only through this repository's `catalog-v1` release, so repository access is the second lock.
  - A leaked key can sign any sequence. If such a catalog was published, apps that took it refuse lower sequences. Read its sequence with `verify` and sign above it.

### At every app release

Raise `MIN_SEQUENCE` in `crates/ck-core/src/catalog/trust.rs` to the sequence of the newest published catalog. Then a fresh install cannot be handed an older catalog. An updated app also stops reading a cached catalog below the new floor: nothing new installs until its next refresh succeeds, while installed versions keep working. `sign` refuses a sequence below the `MIN_SEQUENCE` it was built with, so the app would never refuse a catalog it signed as outdated.

## Checking a release build

`cargo test -p ck-core --release --locked --test release_trust` checks two things. A release build refuses a catalog signed by a throwaway test key. It also refuses plain HTTP to loopback. The test asserts the refusal, not an empty slot, so it keeps passing after the real keys are added.
