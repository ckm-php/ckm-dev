//! Throwaway Ed25519 keys and catalog envelopes for tests, broken ones too.
//!
//! Shared by `#[path]` only: ck-core's unit tests (from `src/catalog/trust.rs`),
//! its integration tests and ck-net's. So it names no ck-core item: callers
//! pass the signing context. Each key comes from the OS RNG when a test runs,
//! and no build outside a test contains this file.
#![allow(dead_code, reason = "each test crate uses part of this helper")]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

/// A key pair made from 32 bytes of OS randomness.
pub struct TestKey(SigningKey);

impl TestKey {
    pub fn generate() -> Self {
        let mut seed = [0; 32];
        getrandom::fill(&mut seed).expect("OS randomness");
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.0.verifying_key()
    }

    /// The first 8 bytes of the SHA-256 of the public key, in lower-case hex.
    pub fn key_id(&self) -> String {
        sha256_hex(self.verifying_key().as_bytes())[..16].to_owned()
    }

    /// A format-1 envelope whose signature covers `context`, then `payload`.
    pub fn sign(&self, context: &[u8], payload: &[u8]) -> Envelope {
        let message = [context, payload].concat();
        Envelope {
            format: 1,
            key_id: self.key_id(),
            payload: payload.to_vec(),
            signature: self.0.sign(&message).to_bytes().to_vec(),
            extra: Map::new(),
        }
    }
}

/// An envelope's fields before encoding. A test breaks one field, then calls
/// [`Envelope::to_bytes`].
pub struct Envelope {
    pub format: u64,
    pub key_id: String,
    /// The decoded payload; `to_bytes` writes it in standard base64.
    pub payload: Vec<u8>,
    /// The signature bytes (64 when valid); `to_bytes` writes them in
    /// standard base64.
    pub signature: Vec<u8>,
    /// More top-level fields, which a valid envelope never has.
    pub extra: Map<String, Value>,
}

impl Envelope {
    /// `{"format":…,"key_id":…,"payload":…,"signature":…}` plus `extra`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut object = Map::new();
        object.insert("format".to_owned(), json!(self.format));
        object.insert("key_id".to_owned(), json!(self.key_id));
        object.insert("payload".to_owned(), json!(STANDARD.encode(&self.payload)));
        object.insert(
            "signature".to_owned(),
            json!(STANDARD.encode(&self.signature)),
        );
        object.extend(self.extra.clone());
        serde_json::to_vec(&Value::Object(object)).expect("an envelope encodes")
    }
}

/// A schema-1 payload with `runtimes` as given.
pub fn payload(sequence: u64, expires: u64, runtimes: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": 1,
        "sequence": sequence,
        "expires": expires,
        "runtimes": runtimes,
    }))
    .expect("a payload encodes")
}

/// One `runtimes[]` entry.
pub fn runtime(name: &str, version: &str, artifacts: Value) -> Value {
    json!({
        "name": name,
        "version": version,
        "license": "MIT",
        "artifacts": artifacts,
    })
}

/// One `artifacts[]` entry that pins `bytes`, served at `url`.
pub fn artifact(os: &str, arch: &str, url: &str, format: &str, bytes: &[u8]) -> Value {
    json!({
        "os": os,
        "arch": arch,
        "url": url,
        "sha256": sha256_hex(bytes),
        "size": bytes.len(),
        "format": format,
    })
}

/// SHA-256 of `bytes` as 64 lower-case hex characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
