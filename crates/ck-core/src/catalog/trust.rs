//! The trust built into the app (D6, D25): the keys that sign the catalog,
//! the lowest sequence it accepts, and where it fetches the catalog.
//!
//! [`RELEASE_KEYS`] holds the owner's two public keys (`catalog/README.md`):
//! key A signs, key B is the offline backup. No environment variable, file
//! or debug hook adds a key. Tests sign with keys
//! they generate and pass them in through [`Trust::new`]; app code uses only
//! [`Trust::release`].

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The Ed25519 public keys that sign release catalogs, one [`hex32`] line
/// each. A new key is added before the first catalog it signs, and the old
/// one removed later.
pub const RELEASE_KEYS: &[[u8; 32]] = &[
    // Key A: signs the catalog.
    hex32("5390dc947c1d3fef3520193a26b8c2d257a16e67749bac5e705759566dfda6d8"),
    // Key B: the backup, kept offline.
    hex32("2bfd23f1407d07d78d9b513ce0b04dcecd63575f9cacabe12b79abca00cca56e"),
];

/// The lowest catalog sequence this app accepts, even on first use. Raised at
/// each app release, so a new install cannot be handed an older catalog.
pub const MIN_SEQUENCE: u64 = 0;

/// Where the app fetches `catalog-v1.json`: an asset of the fixed release
/// `catalog-v1` (D25), never a `latest` URL.
pub const CATALOG_URL: Option<&str> =
    Some("https://github.com/ckm-php/ckm-dev/releases/download/catalog-v1/catalog-v1.json");

/// What every release download URL of this repository starts with.
const RELEASE_DOWNLOAD: &str = "/releases/download/";

/// The release download folder of the repository [`CATALOG_URL`] names,
/// through `/releases/download/`:
/// `https://github.com/ckm-php/ckm-dev/releases/download/`. Each runtime
/// release's assets sit under it, as `<prefix><tag>/<file>`. `None` when
/// there is no catalog URL, or it is not a release download URL.
pub fn release_download_prefix() -> Option<&'static str> {
    let url = CATALOG_URL?;
    let end = url.find(RELEASE_DOWNLOAD)? + RELEASE_DOWNLOAD.len();
    url.get(..end)
}

/// Reads 64 lower-case hex characters as 32 bytes: one key line in
/// [`RELEASE_KEYS`]. It panics on any other length or character, which in a
/// `const` fails the build, so a typo in a key cannot ship.
///
/// ```
/// use ck_core::catalog::trust::hex32;
/// const KEY: [u8; 32] =
///     hex32("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
/// assert_eq!((KEY[0], KEY[31]), (0xd7, 0x1a));
/// ```
///
/// ```compile_fail
/// use ck_core::catalog::trust::hex32;
/// // The last character is not hex.
/// const KEY: [u8; 32] =
///     hex32("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511g");
/// ```
pub const fn hex32(text: &str) -> [u8; 32] {
    let digits = text.as_bytes();
    assert!(
        digits.len() == 64,
        "a key is exactly 64 lower-case hex characters"
    );
    let mut bytes = [0; 32];
    let mut index = 0;
    while index < 32 {
        bytes[index] = (hex_digit(digits[2 * index]) << 4) | hex_digit(digits[2 * index + 1]);
        index += 1;
    }
    bytes
}

/// One lower-case hex digit's value.
const fn hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => panic!("a key holds only the hex digits 0-9 and a-f"),
    }
}

/// Signed in front of the payload bytes, so a catalog signature never
/// verifies as any other message. Fixed for envelope format 1: it is part of
/// the signature, not branding.
pub const SIGNING_CONTEXT: &[u8] = b"ck-dev catalog v1\n";

/// Bytes of the key hash that make a key id.
const KEY_ID_BYTES: usize = 8;

/// The id of `key`: the first 8 bytes of the SHA-256 of its 32 bytes, as 16
/// lower-case hex characters. It only picks which trusted key to try; it is
/// never trusted on its own.
pub fn key_id(key: &VerifyingKey) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..KEY_ID_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The keys a catalog's signature is checked against, and the lowest
/// sequence accepted or read from the cache, before and after first use.
#[derive(Debug, Clone)]
pub struct Trust {
    keys: Vec<VerifyingKey>,
    min_sequence: u64,
}

impl Trust {
    /// [`RELEASE_KEYS`] and [`MIN_SEQUENCE`]: the only trust app code uses.
    /// With no keys, every catalog is refused.
    pub fn release() -> Result<Self> {
        let keys = RELEASE_KEYS
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                VerifyingKey::from_bytes(bytes).map_err(|_| {
                    Error::Internal(format!(
                        "built-in catalog key {index} is not an Ed25519 public key"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Self::new(keys, MIN_SEQUENCE)
    }

    /// Any keys and floor. Tests use it with keys they generate; app code
    /// uses [`Trust::release`]. A weak (small-order) key is refused, because
    /// it would verify almost any signature.
    pub fn new(keys: Vec<VerifyingKey>, min_sequence: u64) -> Result<Self> {
        if let Some(weak) = keys.iter().find(|key| key.is_weak()) {
            return Err(Error::CatalogUntrusted {
                reason: format!("key {} is weak", key_id(weak)),
            });
        }
        Ok(Self { keys, min_sequence })
    }

    pub fn keys(&self) -> &[VerifyingKey] {
        &self.keys
    }

    pub fn min_sequence(&self) -> u64 {
        self.min_sequence
    }
}

/// Throwaway keys and envelopes for tests (`tests/support/signing.rs`).
#[cfg(test)]
#[path = "../../tests/support/signing.rs"]
pub(crate) mod test_signing;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::test_signing::TestKey;
    use super::*;
    use crate::download::fetch::parse_strict_url;
    use crate::download::policy::{TransportPolicy, redact};
    use crate::paths::{BuildKind, Profile};

    #[test]
    fn a_key_id_is_the_start_of_the_key_hash() {
        // RFC 8032, section 7.1, test 1.
        const RFC_8032_KEY: [u8; 32] =
            hex32("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let key = VerifyingKey::from_bytes(&RFC_8032_KEY).unwrap();
        // `printf <key hex> | xxd -r -p | sha256sum | cut -c1-16`
        assert_eq!(key_id(&key), "21fe31dfa154a261");

        let test_key = TestKey::generate();
        assert_eq!(test_key.key_id(), key_id(&test_key.verifying_key()));
    }

    #[test]
    fn a_weak_key_is_refused() {
        // The identity point: small order, so it is weak.
        let mut identity = [0; 32];
        identity[0] = 1;
        let weak = VerifyingKey::from_bytes(&identity).unwrap();
        assert!(weak.is_weak());

        let strong = TestKey::generate().verifying_key();
        assert!(matches!(
            Trust::new(vec![strong, weak], 0),
            Err(Error::CatalogUntrusted { .. })
        ));
        assert_eq!(Trust::new(vec![strong], 7).unwrap().min_sequence(), 7);
    }

    #[test]
    #[should_panic(expected = "hex digits")]
    fn hex32_refuses_upper_case() {
        hex32("D75A980182B10AB7D54BFED3C964073A0EE172F3DAA62325AF021A68F707511A");
    }

    #[test]
    #[should_panic(expected = "exactly 64")]
    fn hex32_refuses_a_short_key() {
        hex32("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f70751");
    }

    #[test]
    fn the_release_prefix_is_the_catalog_url_up_to_releases_download() {
        assert_eq!(
            release_download_prefix(),
            Some("https://github.com/ckm-php/ckm-dev/releases/download/")
        );
    }

    // Written to bite once the owner's keys are in the slot: while it is
    // empty, the loop and the distinctness checks pass trivially.
    #[test]
    fn every_built_in_key_is_a_strong_ed25519_key_with_its_own_id() {
        let mut ids = HashSet::new();
        let mut bytes = HashSet::new();
        for (index, key) in RELEASE_KEYS.iter().enumerate() {
            let verifying = VerifyingKey::from_bytes(key)
                .unwrap_or_else(|_| panic!("key {index} is not an Ed25519 public key"));
            assert!(!verifying.is_weak(), "key {index} is weak");
            assert!(bytes.insert(*key), "key {index} is listed twice");
            assert!(
                ids.insert(key_id(&verifying)),
                "key {index} shares its key id with another key"
            );
        }
        let trust = Trust::release().expect("the built-in trust loads");
        assert_eq!(trust.keys().len(), RELEASE_KEYS.len());
        assert_eq!(trust.min_sequence(), MIN_SEQUENCE);
    }

    // The URL every app fetches first passes the checks the downloader
    // makes before its first request, under the rules of a release build.
    #[test]
    fn the_catalog_url_passes_the_strict_parser_and_the_release_policy() {
        let text = CATALOG_URL.expect("a catalog URL is built in");
        let url = parse_strict_url(text).expect("the catalog URL parses strictly");
        // Nothing was normalised, and there is no query or userinfo to hide.
        assert_eq!(url.as_str(), text);
        assert_eq!(redact(&url), text);
        for policy in [
            TransportPolicy::for_build(BuildKind::Release, Profile::Standard),
            TransportPolicy::current(Profile::Standard),
        ] {
            policy
                .check_first(&url)
                .expect("the release policy allows the catalog URL");
        }

        let prefix = release_download_prefix().expect("the catalog URL is a release download");
        assert!(text.starts_with(prefix), "{prefix}");
        assert!(prefix.ends_with("/releases/download/"), "{prefix}");
        assert!(text.len() > prefix.len(), "{text}");
    }
}
