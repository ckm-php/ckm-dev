//! The trust built into the app (D6): the keys that sign the catalog, the
//! lowest sequence it accepts, and where it fetches the catalog.
//!
//! The three slots stay empty until the owner adds the real key offline (see
//! `catalog/README.md`). No environment variable, file or debug hook adds a
//! key. Tests sign with keys they generate and pass them in through
//! [`Trust::new`]; app code uses only [`Trust::release`].

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The Ed25519 public keys that sign release catalogs. A new key is added
/// before the first catalog it signs, and the old one removed later.
pub const RELEASE_KEYS: &[[u8; 32]] = &[];

/// The lowest catalog sequence this app accepts, even on first use. Raised at
/// each app release, so a new install cannot be handed an older catalog.
pub const MIN_SEQUENCE: u64 = 0;

/// Where the app fetches `catalog-v1.json`. `None` until one is published.
pub const CATALOG_URL: Option<&str> = None;

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
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::Signature;
    use serde_json::Value;

    use super::test_signing::{self, TestKey};
    use super::*;

    #[test]
    fn a_key_id_is_the_start_of_the_key_hash() {
        // RFC 8032, section 7.1, test 1.
        let key = VerifyingKey::from_bytes(&[
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ])
        .unwrap();
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

    /// The helper's envelopes are what T1 relies on: the good one verifies
    /// over the context and payload, and a flipped byte does not.
    #[test]
    fn the_signing_helper_signs_the_context_then_the_payload() {
        let key = TestKey::generate();
        let payload = test_signing::payload(1, 2, Value::Array(Vec::new()));
        let mut envelope = key.sign(SIGNING_CONTEXT, &payload);

        let document: Value = serde_json::from_slice(&envelope.to_bytes()).unwrap();
        assert_eq!(document["format"], 1);
        assert_eq!(
            document["key_id"].as_str(),
            Some(key_id(&key.verifying_key()).as_str())
        );
        let decoded = STANDARD
            .decode(document["payload"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, payload);
        let signature: [u8; 64] = STANDARD
            .decode(document["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let signature = Signature::from_bytes(&signature);
        let message = [SIGNING_CONTEXT, &payload].concat();
        key.verifying_key()
            .verify_strict(&message, &signature)
            .unwrap();

        envelope.payload[0] ^= 1;
        let message = [SIGNING_CONTEXT, &envelope.payload].concat();
        assert!(
            key.verifying_key()
                .verify_strict(&message, &signature)
                .is_err()
        );
    }
}
