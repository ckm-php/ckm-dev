//! The owner's catalog signing key (D25): where it is read from, and what it
//! is.
//!
//! - `sign` and `key check` refuse to run in CI ([`refuse_in_ci`]), and run
//!   on Linux and macOS only ([`load`] fails elsewhere).
//! - The key arrives on stdin (`--key -`) as the unencrypted PEM that
//!   `openssl pkey` prints after it decrypts the owner's key file, so this
//!   tool never sees the passphrase and the decrypted key never touches the
//!   disk. Stdin that is a terminal is refused, so a key is never typed or
//!   pasted into one. A key file (`--key <file>`) is for throwaway test keys.
//! - Key material is never printed, and no error quotes it. Our own buffers
//!   are wiped with `zeroize`.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ck_core::catalog::trust::{Trust, key_id};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use zeroize::Zeroizing;

use crate::{Error, Result};

/// The variables that mark a CI run. `sign` and `key check` refuse to run
/// when either is set, to any value.
pub const CI_VARIABLES: [&str; 2] = ["CI", "GITHUB_ACTIONS"];

/// The first and last lines of the one PEM block a key may hold.
const PEM_BEGIN: &[u8] = b"-----BEGIN PRIVATE KEY-----";
const PEM_END: &[u8] = b"-----END PRIVATE KEY-----";

/// What an encrypted key starts with, so its error can say what to do.
const PEM_ENCRYPTED: &[u8] = b"-----BEGIN ENCRYPTED PRIVATE KEY-----";

/// The PKCS#8 DER of every Ed25519 private key, before its 32-byte seed
/// (RFC 8410): a SEQUENCE of version 0, the Ed25519 algorithm identifier,
/// and an OCTET STRING holding an OCTET STRING of 32 bytes.
const DER_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The whole DER: the prefix, then the seed.
const DER_LEN: usize = DER_PREFIX.len() + 32;

/// The base64 text of [`DER_LEN`] bytes, which needs no padding.
const BODY_LEN: usize = DER_LEN / 3 * 4;

/// What [`render`](KeyCheck::render) prints for a key in the trust it was
/// checked against. Runbook step E looks for exactly this phrase.
pub const COMPILED: &str = "compiled release key";

/// What [`render`](KeyCheck::render) prints for any other key.
pub const NOT_COMPILED: &str = "not compiled in";

/// Where `--key` reads the key from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// `--key -`: all of stdin.
    Stdin,
    /// `--key <file>`: a key file (see [`load`]).
    File(PathBuf),
}

impl KeySource {
    /// `-` is stdin; any other non-empty text is a file path.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "" => Err(Error::Invalid("the key path is empty".to_owned())),
            "-" => Ok(Self::Stdin),
            path => Ok(Self::File(PathBuf::from(path))),
        }
    }
}

/// An Ed25519 signing key, held as its 32-byte seed and wiped on drop. It
/// has no `Clone`, and its `Debug` prints no key material.
pub struct SecretKey {
    seed: Zeroizing<[u8; 32]>,
}

impl SecretKey {
    /// Reads an unencrypted PKCS#8 PEM, as `openssl pkey` prints it: exactly
    /// one `PRIVATE KEY` block, whose DER is 48 bytes: the fixed Ed25519
    /// prefix `302e020100300506032b657004220420`, then the seed. Anything
    /// else fails, and no error quotes the input.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        let malformed = |reason: &str| {
            Error::Invalid(format!(
                "the key is not an unencrypted Ed25519 PKCS#8 PEM: {reason}"
            ))
        };
        let text = pem.trim_ascii();
        if text.starts_with(PEM_ENCRYPTED) {
            return Err(Error::Invalid(
                "the key is encrypted: decrypt it with `openssl pkey` and pipe it to `--key -`"
                    .to_owned(),
            ));
        }
        let mut lines = text
            .split(|&byte| byte == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
        if lines.next() != Some(PEM_BEGIN) {
            return Err(malformed("it does not start with a PRIVATE KEY block"));
        }

        // Copies of key material live only in these buffers, wiped on drop.
        let mut body = Zeroizing::new([0_u8; BODY_LEN]);
        let mut body_len = 0;
        let mut ended = false;
        for line in lines {
            if ended {
                return Err(malformed("there is more text after the PRIVATE KEY block"));
            }
            if line == PEM_END {
                ended = true;
                continue;
            }
            if !line
                .iter()
                .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            {
                return Err(malformed("the block holds a line that is not base64"));
            }
            let end = body_len + line.len();
            let Some(slot) = body.get_mut(body_len..end) else {
                return Err(malformed("the block is too long for an Ed25519 key"));
            };
            slot.copy_from_slice(line);
            body_len = end;
        }
        if !ended {
            return Err(malformed("the PRIVATE KEY block has no end line"));
        }

        // Larger than the DER, so a short or long body decodes and is then
        // refused by its length. The decode error is not quoted: it names the
        // offending byte.
        let mut der = Zeroizing::new([0_u8; BODY_LEN]);
        let der_len = STANDARD
            .decode_slice(&body[..body_len], der.as_mut_slice())
            .map_err(|_| malformed("the block is not standard base64"))?;
        if der_len != DER_LEN || der[..DER_PREFIX.len()] != DER_PREFIX {
            return Err(malformed(
                "the block is not a 48-byte Ed25519 private key with the fixed PKCS#8 prefix",
            ));
        }
        let mut seed = Zeroizing::new([0_u8; 32]);
        seed.copy_from_slice(&der[DER_PREFIX.len()..DER_LEN]);
        Ok(Self { seed })
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        SigningKey::from_bytes(&self.seed).verifying_key()
    }

    /// Signs `message`.
    ///
    /// ed25519-dalek's `zeroize` feature is off, so the `SigningKey` built
    /// here and in [`verifying_key`](Self::verifying_key) (the seed and its
    /// expanded secret) is not wiped when dropped; it lives only for the call.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Signature> {
        SigningKey::from_bytes(&self.seed)
            .try_sign(message)
            .map_err(|_| Error::Invalid("the key cannot sign the payload".to_owned()))
    }

    /// A key from 32 bytes of OS randomness, as its PEM: a throwaway key for
    /// tests.
    #[cfg(test)]
    pub(crate) fn random_pem() -> String {
        let mut seed = [0_u8; 32];
        getrandom::fill(&mut seed).expect("OS randomness");
        test_pem(&seed)
    }
}

/// The PEM `openssl pkey` prints for `seed`.
#[cfg(test)]
pub(crate) fn test_pem(seed: &[u8; 32]) -> String {
    let der = [DER_PREFIX.as_slice(), seed.as_slice()].concat();
    format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der)
    )
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretKey(..)")
    }
}

/// Fails when any of [`CI_VARIABLES`] is set, to any value, even an empty
/// one. `var` reads one variable; the CLI passes `std::env::var_os`, and
/// tests pass their own, because CI runs the tests with `CI` set.
pub fn refuse_in_ci(var: impl Fn(&str) -> Option<OsString>) -> Result<()> {
    match CI_VARIABLES.into_iter().find(|name| var(name).is_some()) {
        Some(name) => Err(Error::Invalid(format!(
            "{name} is set, so this is a CI run: the signing key is used offline only, \
             on the owner's computer"
        ))),
        None => Ok(()),
    }
}

/// Reads the key from `source` into a buffer wiped on drop, then
/// [`SecretKey::from_pem`]. Linux and macOS only; elsewhere it fails.
///
/// A key file must be a regular file (not a link), owned by the current
/// user (`ck_sys::unix::effective_uid`), with mode 0600 (no group or other
/// access), and outside the repository this tool was built from.
#[cfg(unix)]
pub fn load(source: &KeySource) -> Result<SecretKey> {
    SecretKey::from_pem(&unix::read(source)?)
}

/// Signing keys are read on Linux and macOS only: the key-file checks need
/// a Unix owner and mode.
#[cfg(not(unix))]
pub fn load(_source: &KeySource) -> Result<SecretKey> {
    Err(Error::Invalid(
        "signing keys are supported on Linux and macOS only".to_owned(),
    ))
}

#[cfg(unix)]
mod unix {
    use std::fs::{self, File};
    use std::io::{self, IsTerminal as _, Read};
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};

    use zeroize::Zeroizing;

    use super::KeySource;
    use crate::{Error, Result};

    /// The most key input read. An Ed25519 PEM is about 120 bytes.
    const MAX_KEY_BYTES: usize = 16 * 1024;

    pub(super) fn read(source: &KeySource) -> Result<Zeroizing<Vec<u8>>> {
        match source {
            KeySource::Stdin => {
                refuse_terminal(io::stdin().is_terminal())?;
                // Through a copy of the descriptor, not `io::stdin()`, whose
                // process-wide buffer would keep key bytes that are never
                // wiped.
                let what = Path::new("standard input");
                let fd = io::stdin()
                    .as_fd()
                    .try_clone_to_owned()
                    .map_err(|e| Error::io("open", what, e))?;
                read_capped(&mut File::from(fd), what)
            }
            KeySource::File(path) => read_key_file(path, &repository()?),
        }
    }

    /// A key typed or pasted at a terminal would sit in the terminal's
    /// scrollback; the runbook always pipes it from `openssl pkey`.
    pub(super) fn refuse_terminal(stdin_is_terminal: bool) -> Result<()> {
        if stdin_is_terminal {
            return Err(Error::Invalid(
                "pipe the key in with `openssl pkey -in <file> | …`; refusing to read a key \
                 typed at a terminal"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The repository this tool was built from.
    fn repository() -> Result<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        root.canonicalize()
            .map_err(|e| Error::io("resolve", &root, e))
    }

    /// Reads the key file at `path` after its checks (see [`super::load`]).
    /// `repository` is a canonical path the key must not sit in.
    pub(super) fn read_key_file(path: &Path, repository: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let refuse =
            |reason: &str| Error::Invalid(format!("the key file {}: {reason}", path.display()));
        let linked = fs::symlink_metadata(path).map_err(|e| Error::io("inspect", path, e))?;
        if !linked.file_type().is_file() {
            return Err(refuse(
                "it must be a regular file, not a link, folder or device",
            ));
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| Error::io("resolve", path, e))?;
        if canonical.starts_with(repository) {
            return Err(refuse(&format!(
                "it must sit outside the repository {}",
                repository.display()
            )));
        }
        let mut file = File::open(path).map_err(|e| Error::io("open", path, e))?;
        // The checks read the open file, which must be the one inspected
        // above: a file swapped in between is refused.
        let opened = file.metadata().map_err(|e| Error::io("inspect", path, e))?;
        if !opened.file_type().is_file()
            || (opened.dev(), opened.ino()) != (linked.dev(), linked.ino())
        {
            return Err(refuse("it changed while it was being checked"));
        }
        if opened.uid() != ck_sys::unix::effective_uid() {
            return Err(refuse("it must be owned by the current user"));
        }
        let mode = opened.mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(refuse(&format!(
                "its mode is {mode:o}; it must allow no group or other access (chmod 600)"
            )));
        }
        read_capped(&mut file, path)
    }

    /// Reads all of `reader` into one buffer, allocated once and wiped on
    /// drop, so no copy of the key is left in a freed allocation.
    fn read_capped(reader: &mut impl Read, what: &Path) -> Result<Zeroizing<Vec<u8>>> {
        let mut buffer = Zeroizing::new(vec![0_u8; MAX_KEY_BYTES + 1]);
        let mut filled = 0;
        while filled < buffer.len() {
            match reader.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io("read", what, e)),
            }
        }
        if filled > MAX_KEY_BYTES {
            return Err(Error::Invalid(format!(
                "{} holds more than {MAX_KEY_BYTES} bytes, too much for a key",
                what.display()
            )));
        }
        buffer.truncate(filled);
        Ok(buffer)
    }
}

/// What `key check` prints about a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCheck {
    /// The public key, 64 lower-case hex characters: the line that goes into
    /// `ck_core::catalog::trust::RELEASE_KEYS`.
    pub public_key: String,
    /// `ck_core::catalog::trust::key_id` of the public key.
    pub key_id: String,
    /// Whether the public key is one of the trust's keys.
    pub compiled: bool,
}

impl KeyCheck {
    /// `key<TAB>value` lines, in this order:
    ///
    /// ```text
    /// public_key  <64 hex>
    /// key_id      <16 hex>
    /// trust       compiled release key | not compiled in
    /// ```
    pub fn render(&self) -> String {
        let trust = if self.compiled {
            COMPILED
        } else {
            NOT_COMPILED
        };
        format!(
            "public_key\t{}\nkey_id\t{}\ntrust\t{trust}\n",
            self.public_key, self.key_id
        )
    }
}

/// Describes `key` against `trust`; the CLI passes `Trust::release()`.
pub fn check(key: &SecretKey, trust: &Trust) -> KeyCheck {
    let public = key.verifying_key();
    KeyCheck {
        public_key: public
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        key_id: key_id(&public),
        compiled: trust.keys().contains(&public),
    }
}

#[cfg(test)]
mod tests {
    use ck_core::catalog::trust::hex32;

    use super::*;

    #[test]
    fn key_check_prints_the_public_key_its_id_and_whether_it_is_compiled_in() {
        // RFC 8032, section 7.1, test 1.
        let key = SecretKey {
            seed: Zeroizing::new(hex32(
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            )),
        };
        let public = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let lines = |trust: &str| {
            format!("public_key\t{public}\nkey_id\t21fe31dfa154a261\ntrust\t{trust}\n")
        };
        let compiled = Trust::new(vec![key.verifying_key()], 0).unwrap();
        let empty = Trust::new(Vec::new(), 0).unwrap();
        assert_eq!(check(&key, &compiled).render(), lines(COMPILED));
        assert_eq!(check(&key, &empty).render(), lines(NOT_COMPILED));
        assert_eq!(format!("{key:?}"), "SecretKey(..)");
    }

    /// RFC 8032, section 7.1, test 1: the seed, and its key as `openssl pkey`
    /// prints it.
    const RFC_8032_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const RFC_8032_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g\n-----END PRIVATE KEY-----\n";
    const RFC_8032_BODY: &str = "MC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g";
    const RFC_8032_PUBLIC: &str =
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    fn public_key(key: &SecretKey) -> String {
        check(key, &Trust::new(Vec::new(), 0).unwrap()).public_key
    }

    // T2
    #[test]
    fn only_one_unencrypted_ed25519_pem_block_reads_and_no_error_quotes_the_key() {
        let read = |pem: &str| public_key(&SecretKey::from_pem(pem.as_bytes()).unwrap());
        assert_eq!(read(RFC_8032_PEM), RFC_8032_PUBLIC);
        assert_eq!(read(&RFC_8032_PEM.replace('\n', "\r\n")), RFC_8032_PUBLIC);
        assert_eq!(read(&format!("\n{RFC_8032_PEM}\n \n")), RFC_8032_PUBLIC);
        assert_eq!(read(&test_pem(&hex32(RFC_8032_SEED))), RFC_8032_PUBLIC);

        let block = |label: &str, body: &str| {
            format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
        };
        let cases = [
            (
                "PKCS#8 version 1",
                block(
                    "PRIVATE KEY",
                    "MC4CAQEwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g",
                ),
                "fixed PKCS#8 prefix",
            ),
            (
                "two blocks",
                format!("{RFC_8032_PEM}{RFC_8032_PEM}"),
                "more text after",
            ),
            (
                "an encrypted key",
                block("ENCRYPTED PRIVATE KEY", RFC_8032_BODY),
                "encrypted",
            ),
            (
                "a public key",
                block("PUBLIC KEY", RFC_8032_BODY),
                "does not start",
            ),
            (
                "text before the block",
                format!("key:\n{RFC_8032_PEM}"),
                "does not start",
            ),
            (
                "a character outside base64",
                block("PRIVATE KEY", &RFC_8032_BODY.replace('/', "!")),
                "not base64",
            ),
            (
                "a body twice as long",
                block("PRIVATE KEY", &RFC_8032_BODY.repeat(2)),
                "too long",
            ),
            (
                "a body cut short",
                block("PRIVATE KEY", &RFC_8032_BODY[..60]),
                "48-byte",
            ),
            (
                "no end line",
                RFC_8032_PEM.replace("-----END PRIVATE KEY-----\n", ""),
                "no end line",
            ),
            ("nothing", String::new(), "does not start"),
        ];
        for (case, pem, expected) in cases {
            let error = SecretKey::from_pem(pem.as_bytes()).unwrap_err().to_string();
            assert!(error.contains(expected), "{case}: {error}");
            // The characters that hold the seed.
            assert!(!error.contains(&RFC_8032_BODY[24..56]), "{case}: {error}");
        }
    }

    // T2
    #[test]
    fn any_ci_variable_refuses_even_when_empty() {
        assert!(refuse_in_ci(|_| None).is_ok());
        for name in CI_VARIABLES {
            for value in ["", "true"] {
                let error = refuse_in_ci(|asked| (asked == name).then(|| OsString::from(value)))
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(name) && error.contains("offline only"),
                    "{name}={value:?}: {error}"
                );
            }
        }
    }

    // T2
    #[cfg(unix)]
    #[test]
    fn a_key_on_stdin_must_be_piped_not_typed_at_a_terminal() {
        assert!(super::unix::refuse_terminal(false).is_ok());
        let error = super::unix::refuse_terminal(true).unwrap_err().to_string();
        assert!(error.contains("typed at a terminal"), "{error}");
    }

    // T2
    #[cfg(unix)]
    #[test]
    fn a_key_file_must_be_a_private_regular_file_outside_the_repository() {
        use std::fs;
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        use std::path::Path;

        use crate::test_support::TempDir;

        let temp = TempDir::new();
        let repository = temp.0.join("repository");
        fs::create_dir(&repository).unwrap();
        // The temp folder may sit behind a link (macOS `/var`).
        let repository = repository.canonicalize().unwrap();
        let write = |path: &Path, mode: u32| {
            fs::write(path, RFC_8032_PEM).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        let read = |path: &Path| super::unix::read_key_file(path, &repository);

        let private = temp.0.join("key.pem");
        write(&private, 0o600);
        assert_eq!(read(&private).unwrap().as_slice(), RFC_8032_PEM.as_bytes());
        // The same file through `load`, against the real repository.
        let key = load(&KeySource::File(private.clone())).unwrap();
        assert_eq!(public_key(&key), RFC_8032_PUBLIC);

        let loose = temp.0.join("loose.pem");
        write(&loose, 0o644);
        let link = temp.0.join("link.pem");
        symlink(&private, &link).unwrap();
        let inside = repository.join("key.pem");
        write(&inside, 0o600);
        for (case, path, expected) in [
            ("mode 0644", &loose, "chmod 600"),
            ("a link to a private key", &link, "regular file"),
            ("inside the repository", &inside, "outside the repository"),
            ("a folder", &repository, "regular file"),
        ] {
            let error = read(path).unwrap_err().to_string();
            assert!(error.contains(expected), "{case}: {error}");
        }
    }
}
