//! The signed envelope, format 1: one JSON file, with no detached signature,
//! because two release assets cannot be replaced at once.
//!
//! ```json
//! {"format": 1, "key_id": "<16 hex>", "payload": "<base64>", "signature": "<base64>"}
//! ```
//!
//! `payload` and `signature` use standard base64 with padding (RFC 4648,
//! section 4); the signature is 64 bytes. The signature covers
//! [`SIGNING_CONTEXT`](super::trust::SIGNING_CONTEXT) followed by the decoded
//! payload bytes, and is checked with `verify_strict`. The payload is parsed
//! only after the signature passes.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{SIGNATURE_LENGTH, Signature, VerifyingKey};
use serde::Deserialize;

use super::trust::{self, SIGNING_CONTEXT};
use crate::error::{Error, Result, escape_untrusted};

/// The envelope format this app reads.
pub const FORMAT: u32 = 1;

/// The largest envelope accepted, checked before any parsing.
pub const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// The envelope's fields as written, before any is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    format: u32,
    key_id: String,
    payload: String,
    signature: String,
}

/// Checks `bytes` as a format-1 envelope signed by one of `keys`, and returns
/// the decoded payload bytes.
///
/// In order: the size cap, the envelope JSON (unknown fields refused), the
/// format, the base64 fields, then the signature by each key in `keys` whose
/// [`key_id`](super::trust::key_id) is the envelope's. A malformed envelope
/// gives [`Error::CatalogInvalid`]; no matching key, a failed signature or an
/// empty `keys` gives [`Error::CatalogUntrusted`].
///
/// Nothing compared here is secret (key ids, public keys and signatures are
/// all public), so plain comparisons are used.
pub fn verify(bytes: &[u8], keys: &[VerifyingKey]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(invalid(format!(
            "it is {} bytes, over the {MAX_CATALOG_BYTES}-byte limit",
            bytes.len()
        )));
    }
    let envelope: RawEnvelope = serde_json::from_slice(bytes).map_err(|error| {
        // The text can quote the unsigned bytes, such as an unknown field's
        // name.
        invalid(format!(
            "the envelope: {}",
            escape_untrusted(&error.to_string())
        ))
    })?;
    if envelope.format != FORMAT {
        return Err(invalid(format!(
            "envelope format {} is not supported; this app reads format {FORMAT}",
            envelope.format
        )));
    }
    if !is_key_id(&envelope.key_id) {
        return Err(invalid(format!(
            "key id {:?} is not 16 lower-case hex characters",
            envelope.key_id
        )));
    }
    let payload = decode("payload", &envelope.payload)?;
    let signature = decode("signature", &envelope.signature)?;
    let signature = <[u8; SIGNATURE_LENGTH]>::try_from(signature.as_slice()).map_err(|_| {
        invalid(format!(
            "the signature is {} bytes, not {SIGNATURE_LENGTH}",
            signature.len()
        ))
    })?;
    let signature = Signature::from_bytes(&signature);

    if keys.is_empty() {
        return Err(untrusted("this build trusts no catalog key".to_owned()));
    }
    // The id only picks which keys to try; a key whose id does not match is
    // never tried.
    let mut candidates = keys
        .iter()
        .filter(|key| trust::key_id(key) == envelope.key_id)
        .peekable();
    if candidates.peek().is_none() {
        return Err(untrusted(format!(
            "no trusted key has id {}",
            envelope.key_id
        )));
    }
    let message = [SIGNING_CONTEXT, &payload].concat();
    if candidates.any(|key| key.verify_strict(&message, &signature).is_ok()) {
        Ok(payload)
    } else {
        Err(untrusted(format!(
            "the signature does not verify with key {}",
            envelope.key_id
        )))
    }
}

/// 16 lower-case hex characters, as [`trust::key_id`] writes them.
fn is_key_id(text: &str) -> bool {
    text.len() == 16
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn decode(field: &str, text: &str) -> Result<Vec<u8>> {
    STANDARD.decode(text).map_err(|error| {
        invalid(format!(
            "{field} is not standard base64 with padding: {error}"
        ))
    })
}

fn invalid(reason: String) -> Error {
    Error::CatalogInvalid { reason }
}

fn untrusted(reason: String) -> Error {
    Error::CatalogUntrusted { reason }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::catalog::trust::test_signing::{self, Envelope, TestKey};

    #[test]
    fn a_catalog_verifies_only_with_a_trusted_key_over_the_context_and_payload() {
        let signer = TestKey::generate();
        let other = TestKey::generate();
        let trusted = [other.verifying_key(), signer.verifying_key()];
        let no_keys: [VerifyingKey; 0] = [];
        let payload = test_signing::payload(1, 2, json!([]));
        let good = signer.sign(SIGNING_CONTEXT, &payload).to_bytes();

        // The signer is found by its id, not by its place in the list.
        assert_eq!(verify(&good, &trusted).unwrap(), payload);

        let mut at_cap = good.clone();
        at_cap.resize(MAX_CATALOG_BYTES, b' ');
        assert_eq!(verify(&at_cap, &trusted).unwrap(), payload);
        let mut over_cap = at_cap;
        over_cap.push(b' ');

        let broken = |change: &dyn Fn(&mut Envelope)| {
            let mut envelope = signer.sign(SIGNING_CONTEXT, &payload);
            change(&mut envelope);
            envelope.to_bytes()
        };
        let edited = |edit: &dyn Fn(&mut Value)| {
            let mut document: Value = serde_json::from_slice(&good).unwrap();
            edit(&mut document);
            serde_json::to_vec(&document).unwrap()
        };

        let untrusted_cases: [(&str, Vec<u8>, &[VerifyingKey]); 7] = [
            (
                "a flipped payload byte",
                broken(&|e: &mut Envelope| e.payload[0] ^= 1),
                &trusted,
            ),
            (
                "a flipped signature byte",
                broken(&|e: &mut Envelope| e.signature[0] ^= 1),
                &trusted,
            ),
            (
                "an unknown key",
                TestKey::generate()
                    .sign(SIGNING_CONTEXT, &payload)
                    .to_bytes(),
                &trusted,
            ),
            (
                "a well-formed id of no trusted key",
                broken(&|e: &mut Envelope| e.key_id = "0123456789abcdef".to_owned()),
                &trusted,
            ),
            (
                "a trusted key's id on another key's signature",
                broken(&|e: &mut Envelope| e.key_id = other.key_id()),
                &trusted,
            ),
            (
                "another context string",
                signer.sign(b"ck-dev catalog v2\n", &payload).to_bytes(),
                &trusted,
            ),
            ("no trusted key at all", good.clone(), &no_keys),
        ];
        for (case, bytes, keys) in &untrusted_cases {
            let result = verify(bytes, keys);
            assert!(
                matches!(result, Err(Error::CatalogUntrusted { .. })),
                "{case}: {result:?}"
            );
        }

        let invalid_cases = [
            ("one byte over the size cap", over_cap),
            ("cut short", good[..good.len() - 1].to_vec()),
            (
                "an unknown field",
                broken(&|e: &mut Envelope| {
                    e.extra.insert("comment".to_owned(), json!("hi"));
                }),
            ),
            ("format 2", broken(&|e: &mut Envelope| e.format = 2)),
            (
                "a key id in upper case",
                broken(&|e: &mut Envelope| e.key_id = "0123456789ABCDEF".to_owned()),
            ),
            (
                "a payload that is not base64",
                edited(&|d: &mut Value| d["payload"] = json!("not base64!")),
            ),
            (
                "a 63-byte signature",
                broken(&|e: &mut Envelope| {
                    e.signature.pop();
                }),
            ),
        ];
        for (case, bytes) in &invalid_cases {
            let result = verify(bytes, &trusted);
            assert!(
                matches!(result, Err(Error::CatalogInvalid { .. })),
                "{case}: {result:?}"
            );
        }
    }
}
