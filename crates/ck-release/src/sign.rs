//! Signs a payload into a format-1 envelope (`ck_core::catalog::envelope`),
//! and verifies an envelope (D25).
//!
//! The CLI passes `Trust::release()`, the keys compiled into `ck-core`, and
//! the current time. Tests pass a trust of throwaway keys.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ck_core::catalog::envelope::{self, FORMAT};
use ck_core::catalog::schema::{self, Payload};
use ck_core::catalog::trust::{SIGNING_CONTEXT, Trust, key_id};
use ck_core::download::policy::TransportPolicy;
use ck_core::paths::Profile;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::key::SecretKey;
use crate::{Error, Result, lock, payload, release_prefix, to_pretty_json};

/// The shortest lifetime `sign` accepts: the payload must expire at least
/// this long after now.
pub const MIN_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The longest lifetime `sign` accepts. A catalog lives 90 days and is
/// re-signed at least every 60.
pub const MAX_LIFETIME: Duration = Duration::from_secs(100 * 24 * 60 * 60);

/// What the new sequence must be higher than.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous<'a> {
    /// `--first`: no catalog was published before, and the output must not
    /// exist yet.
    First,
    /// `--previous <envelope>`: the last published envelope. Its signature
    /// must verify under the trust, and the new sequence must be higher.
    Envelope(&'a Path),
}

/// The files `sign` reads and writes.
#[derive(Debug, Clone, Copy)]
pub struct SignRequest<'a> {
    /// The payload file, in the canonical form `payload build` writes.
    pub payload: &'a Path,
    /// Where the envelope goes. It may be the `--previous` file.
    pub out: &'a Path,
    pub previous: Previous<'a>,
}

/// What `sign` and `verify` print about an envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub key_id: String,
    pub sequence: u64,
    /// Unix seconds.
    pub expires: u64,
    /// Entries in `runtimes`.
    pub runtimes: usize,
    /// Artifacts in all runtimes.
    pub artifacts: usize,
}

impl Summary {
    /// The summary of `payload`, signed by the key with id `key_id`.
    pub fn new(key_id: String, payload: &Payload) -> Self {
        Self {
            key_id,
            sequence: payload.sequence,
            expires: payload.expires,
            runtimes: payload.runtimes.len(),
            artifacts: payload
                .runtimes
                .iter()
                .map(|runtime| runtime.artifacts.len())
                .sum(),
        }
    }

    /// `key<TAB>value` lines, in this order, with `expires` in RFC 3339:
    ///
    /// ```text
    /// key_id     <16 hex>
    /// sequence   <n>
    /// expires    2027-02-17T00:00:00Z
    /// runtimes   <n>
    /// artifacts  <n>
    /// ```
    pub fn render(&self) -> Result<String> {
        let expires = i64::try_from(self.expires)
            .ok()
            .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
            .and_then(|time| time.format(&Rfc3339).ok())
            .ok_or_else(|| {
                Error::Invalid(format!("the expiry {} is out of range", self.expires))
            })?;
        Ok(format!(
            "key_id\t{}\nsequence\t{}\nexpires\t{expires}\nruntimes\t{}\nartifacts\t{}\n",
            self.key_id, self.sequence, self.runtimes, self.artifacts
        ))
    }
}

/// Signs the payload into an envelope at `request.out`, and returns its
/// summary. In order, and nothing is written unless every step passes:
///
/// 1. The payload: read with `payload::read` (canonical form), every URL
///    exactly where `payload::check_urls` puts it under
///    `crate::release_prefix()` with the release transport policy, and its
///    expiry between [`MIN_LIFETIME`] and [`MAX_LIFETIME`] after `now`.
/// 2. The sequence: at least `trust.min_sequence()`, the floor the app
///    refuses below. With [`Previous::Envelope`], that envelope must verify
///    under `trust` and the new sequence must be higher than its sequence;
///    with [`Previous::First`], `request.out` must not exist.
/// 3. The signature: `key` signs `SIGNING_CONTEXT` followed by the payload
///    bytes, into envelope format 1.
/// 4. The self-check: the envelope must pass
///    `ck_core::catalog::envelope::verify` with `trust.keys()` and give back
///    the same payload bytes. A key that `trust` does not hold fails here.
/// 5. The envelope is written atomically.
///
/// It runs anywhere; the CLI adds the CI and platform refusals before it
/// reads the key.
pub fn sign(
    request: &SignRequest<'_>,
    key: &SecretKey,
    trust: &Trust,
    now: SystemTime,
) -> Result<Summary> {
    let (bytes, payload) = payload::read(request.payload)?;
    payload::check_urls(
        &payload,
        release_prefix()?,
        &TransportPolicy::current(Profile::Standard),
    )?;
    check_lifetime(payload.expires, now)?;
    // The app refuses a sequence below the floor it was built with.
    if payload.sequence < trust.min_sequence() {
        return Err(Error::Invalid(format!(
            "the payload's sequence {} is below {}, the compiled MIN_SEQUENCE, so the app \
             would refuse it as outdated",
            payload.sequence,
            trust.min_sequence()
        )));
    }

    match request.previous {
        Previous::First => match fs::symlink_metadata(request.out) {
            Ok(_) => {
                return Err(Error::Invalid(format!(
                    "{} already exists: `--first` only writes a new file; pass the last \
                     published envelope with `--previous`",
                    request.out.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("inspect", request.out, e)),
        },
        Previous::Envelope(path) => {
            // Only the signature and the sequence: an expired previous
            // catalog can still be followed by a new one.
            let previous = fs::read(path).map_err(|e| Error::io("read", path, e))?;
            let previous = envelope::verify(&previous, trust.keys())
                .and_then(|bytes| schema::parse(&bytes))
                .map_err(|error| {
                    Error::Invalid(format!("the previous envelope {}: {error}", path.display()))
                })?;
            if payload.sequence <= previous.sequence {
                return Err(Error::Invalid(format!(
                    "the payload's sequence {} must be higher than {}, the previous envelope's",
                    payload.sequence, previous.sequence
                )));
            }
        }
    }

    let public = key.verifying_key();
    let id = key_id(&public);
    let signature = key.sign(&[SIGNING_CONTEXT, bytes.as_slice()].concat())?;
    let signed = to_pretty_json(&EnvelopeOut {
        format: FORMAT,
        key_id: &id,
        payload: STANDARD.encode(&bytes),
        signature: STANDARD.encode(signature.to_bytes()),
    })?;

    // The self-check: the app's own envelope check, with the same keys.
    if !trust.keys().contains(&public) {
        return Err(Error::Invalid(format!(
            "key {id} is not a compiled release key, so the app would refuse this \
             envelope; nothing was written"
        )));
    }
    let checked = envelope::verify(&signed, trust.keys()).map_err(|error| {
        Error::Invalid(format!(
            "the new envelope fails its self-check, so nothing was written: {error}"
        ))
    })?;
    if checked != bytes {
        return Err(Error::Invalid(
            "the new envelope reads back other payload bytes, so nothing was written".to_owned(),
        ));
    }

    lock::write_atomic(request.out, &signed)?;
    Ok(Summary::new(id, &payload))
}

/// Checks the envelope file `envelope` with `trust`: its signature, then its
/// payload with `ck_core::catalog::schema::parse`, and refuses one that is
/// expired at `now`, as the app does. With `payload`, the signed bytes must
/// equal that file's bytes exactly. Returns its summary.
pub fn verify(
    envelope: &Path,
    payload: Option<&Path>,
    trust: &Trust,
    now: SystemTime,
) -> Result<Summary> {
    let bytes = fs::read(envelope).map_err(|e| Error::io("read", envelope, e))?;
    let signed = envelope::verify(&bytes, trust.keys())?;
    let parsed = schema::parse(&signed)?;
    // The app's rule: a catalog is refused at or after its expiry.
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if now >= parsed.expires {
        return Err(Error::Core(ck_core::Error::CatalogExpired {
            expires: parsed.expires,
        }));
    }
    if let Some(path) = payload {
        let expected = fs::read(path).map_err(|e| Error::io("read", path, e))?;
        if expected != signed {
            return Err(Error::Invalid(format!(
                "{} is not the payload {} signs, byte for byte",
                path.display(),
                envelope.display()
            )));
        }
    }
    // The signature passed, so the id names the trusted key that made it.
    let EnvelopeKeyId { key_id: id } = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Invalid(format!("{} has no readable key id", envelope.display())))?;
    Ok(Summary::new(id, &parsed))
}

/// Envelope format 1 as `sign` writes it, in the field order
/// `ck_core::catalog::envelope` documents.
#[derive(Serialize)]
struct EnvelopeOut<'a> {
    format: u32,
    key_id: &'a str,
    /// Standard base64 with padding.
    payload: String,
    /// Standard base64 with padding.
    signature: String,
}

/// The one field `verify` reads itself, after `envelope::verify` passed.
#[derive(Deserialize)]
struct EnvelopeKeyId {
    key_id: String,
}

/// [`MIN_LIFETIME`] <= `expires` - `now` <= [`MAX_LIFETIME`].
fn check_lifetime(expires: u64, now: SystemTime) -> Result<()> {
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("the clock reads before 1970".to_owned()))?
        .as_secs();
    let earliest = now.saturating_add(MIN_LIFETIME.as_secs());
    let latest = now.saturating_add(MAX_LIFETIME.as_secs());
    if (earliest..=latest).contains(&expires) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "the payload expires at {expires} (Unix time); `sign` accepts {} to {} days \
             from now ({now})",
            MIN_LIFETIME.as_secs() / DAY,
            MAX_LIFETIME.as_secs() / DAY
        )))
    }
}

/// Seconds in a day.
const DAY: u64 = 24 * 60 * 60;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ck_core::catalog::schema::{
        Arch, ArchiveFormat, Artifact, Os, Platform, Runtime, RuntimeName, RuntimeVersion, SCHEMA,
        Sha256Digest,
    };
    use ck_core::catalog::{self, Catalog};
    use ck_core::layout;
    use ck_core::paths::Paths;

    use super::*;
    use crate::payload::PayloadSpec;
    use crate::test_support::TempDir;
    use crate::{DESCRIPTOR_SCHEMA, Descriptor, asset_file_name};

    /// 2100-01-01T00:00:00Z: far enough ahead for `payload::assemble`, which
    /// reads the real clock. Each test picks `now` relative to it.
    const EXPIRES: u64 = 4_102_444_800;

    /// `days` before [`EXPIRES`].
    fn days_before_expiry(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(EXPIRES - days * DAY)
    }

    /// A throwaway key, read the way `load` reads one.
    fn new_key() -> SecretKey {
        SecretKey::from_pem(SecretKey::random_pem().as_bytes()).unwrap()
    }

    fn trusting(key: &SecretKey) -> Trust {
        Trust::new(vec![key.verifying_key()], 0).unwrap()
    }

    /// Writes the canonical payload of one PHP build, with its URL under
    /// `prefix`, to `dir/payload-<sequence>.json`.
    fn write_payload(dir: &Path, sequence: u64, prefix: &str) -> PathBuf {
        let name = RuntimeName::new("php").unwrap();
        let version = RuntimeVersion::new("8.5.12-ck1").unwrap();
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let descriptor = Descriptor {
            schema: DESCRIPTOR_SCHEMA,
            file: asset_file_name(&name, &version, platform),
            name,
            version,
            license: "PHP-3.01".to_owned(),
            os: platform.os,
            cpu: platform.arch,
            sha256: Sha256Digest::of(b"php"),
            size: 31_457_280,
            format: ArchiveFormat::TarGz,
        };
        let spec = PayloadSpec {
            prefix,
            sequence,
            expires: EXPIRES,
        };
        let built = payload::assemble(
            &[descriptor],
            &spec,
            &TransportPolicy::current(Profile::Standard),
        )
        .unwrap();
        let path = dir.join(format!("payload-{sequence}.json"));
        payload::write(&built, &path).unwrap();
        path
    }

    fn accept(
        root: &TempDir,
        trust: &Trust,
        bytes: &[u8],
        now: SystemTime,
    ) -> ck_core::Result<Catalog> {
        let paths = Paths {
            home: root.0.clone(),
            data: root.0.join("data"),
            profile: Profile::Isolated,
        };
        layout::ensure(&paths).unwrap();
        catalog::accept(&paths, trust, bytes, now)
    }

    // T1
    #[test]
    fn a_signed_envelope_is_accepted_by_the_app_and_refused_after_one_flipped_byte() {
        let temp = TempDir::new();
        let key = new_key();
        let trust = trusting(&key);
        let now = days_before_expiry(90);
        let payload = write_payload(&temp.0, 1, release_prefix().unwrap());
        let out = temp.0.join("catalog-v1.json");
        let request = SignRequest {
            payload: &payload,
            out: &out,
            previous: Previous::First,
        };
        let summary = sign(&request, &key, &trust, now).unwrap();
        assert_eq!(
            summary,
            Summary {
                key_id: key_id(&key.verifying_key()),
                sequence: 1,
                expires: EXPIRES,
                runtimes: 1,
                artifacts: 1,
            }
        );
        assert_eq!(
            verify(&out, Some(payload.as_path()), &trust, now).unwrap(),
            summary
        );

        let signed = fs::read(&out).unwrap();
        let profile = TempDir::new();
        assert_eq!(
            accept(&profile, &trust, &signed, now).unwrap().sequence(),
            1
        );

        // One base64 character of the payload changed, so one payload byte.
        let field = b"\"payload\": \"";
        let at = signed
            .windows(field.len())
            .position(|window| window == field)
            .unwrap()
            + field.len()
            + 10;
        let mut flipped = signed.clone();
        flipped[at] = if flipped[at] == b'A' { b'B' } else { b'A' };
        let result = accept(&TempDir::new(), &trust, &flipped, now);
        assert!(
            matches!(result, Err(ck_core::Error::CatalogUntrusted { .. })),
            "{result:?}"
        );

        let other = write_payload(&temp.0, 2, release_prefix().unwrap());
        let error = verify(&out, Some(other.as_path()), &trust, now).unwrap_err();
        assert!(error.to_string().contains("byte for byte"), "{error}");
        let expired = verify(&out, None, &trust, days_before_expiry(0));
        assert!(
            matches!(
                expired,
                Err(Error::Core(ck_core::Error::CatalogExpired { .. }))
            ),
            "{expired:?}"
        );
    }

    // T1
    #[test]
    fn a_key_the_trust_does_not_hold_writes_nothing() {
        let temp = TempDir::new();
        let key = new_key();
        let payload = write_payload(&temp.0, 1, release_prefix().unwrap());
        let out = temp.0.join("catalog-v1.json");
        let request = SignRequest {
            payload: &payload,
            out: &out,
            previous: Previous::First,
        };
        let error = sign(
            &request,
            &key,
            &trusting(&new_key()),
            days_before_expiry(90),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a compiled release key"), "{error}");
        assert!(fs::symlink_metadata(&out).is_err());
    }

    // T2
    #[test]
    fn sign_refuses_a_bad_sequence_lifetime_or_payload_and_writes_nothing() {
        let temp = TempDir::new();
        let key = new_key();
        let trust = trusting(&key);
        let now = days_before_expiry(90);
        let prefix = release_prefix().unwrap();
        let out = temp.0.join("catalog-v1.json");
        let first = SignRequest {
            payload: &write_payload(&temp.0, 2, prefix),
            out: &out,
            previous: Previous::First,
        };
        sign(&first, &key, &trust, now).unwrap();
        let published = fs::read(&out).unwrap();

        // An envelope by a key the trust does not hold.
        let stranger = new_key();
        let foreign = temp.0.join("foreign.json");
        let request = SignRequest {
            payload: &write_payload(&temp.0, 3, prefix),
            out: &foreign,
            previous: Previous::First,
        };
        sign(&request, &stranger, &trusting(&stranger), now).unwrap();

        let outside_dir = temp.0.join("outside");
        fs::create_dir(&outside_dir).unwrap();
        let outside = write_payload(
            &outside_dir,
            3,
            "https://github.com/someone/else/releases/download/",
        );
        let compact = temp.0.join("compact.json");
        let (_, parsed) = payload::read(&write_payload(&temp.0, 3, prefix)).unwrap();
        fs::write(&compact, serde_json::to_vec(&parsed).unwrap()).unwrap();

        let next = write_payload(&temp.0, 3, prefix);
        let cases = [
            (
                "the same sequence",
                write_payload(&temp.0, 2, prefix),
                Previous::Envelope(out.as_path()),
                now,
                "must be higher",
            ),
            (
                "a lower sequence",
                write_payload(&temp.0, 1, prefix),
                Previous::Envelope(out.as_path()),
                now,
                "must be higher",
            ),
            (
                "--first over an existing file",
                next.clone(),
                Previous::First,
                now,
                "already exists",
            ),
            (
                "a previous envelope no trusted key signed",
                next.clone(),
                Previous::Envelope(foreign.as_path()),
                now,
                "the previous envelope",
            ),
            (
                "an expiry 6 days away",
                next.clone(),
                Previous::Envelope(out.as_path()),
                days_before_expiry(6),
                "7 to 100 days",
            ),
            (
                "an expiry 101 days away",
                next.clone(),
                Previous::Envelope(out.as_path()),
                days_before_expiry(101),
                "7 to 100 days",
            ),
            (
                "a URL outside the release prefix",
                outside,
                Previous::Envelope(out.as_path()),
                now,
                "the URL",
            ),
            (
                "a payload not in canonical form",
                compact,
                Previous::Envelope(out.as_path()),
                now,
                "canonical form",
            ),
        ];
        for (case, payload, previous, at, expected) in cases {
            let request = SignRequest {
                payload: &payload,
                out: &out,
                previous,
            };
            let error = sign(&request, &key, &trust, at).unwrap_err().to_string();
            assert!(error.contains(expected), "{case}: {error}");
            assert_eq!(fs::read(&out).unwrap(), published, "{case}");
        }

        // Higher than the previous envelope, but below the compiled floor:
        // the app would refuse it as outdated.
        let floor = Trust::new(vec![key.verifying_key()], 4).unwrap();
        let request = SignRequest {
            payload: &next,
            out: &out,
            previous: Previous::Envelope(&out),
        };
        let error = sign(&request, &key, &floor, now).unwrap_err().to_string();
        assert!(error.contains("MIN_SEQUENCE"), "{error}");
        assert_eq!(fs::read(&out).unwrap(), published);

        // A higher sequence replaces its own previous envelope, at both ends
        // of the lifetime.
        for at in [days_before_expiry(7), days_before_expiry(100)] {
            fs::write(&out, &published).unwrap();
            let request = SignRequest {
                payload: &next,
                out: &out,
                previous: Previous::Envelope(&out),
            };
            assert_eq!(sign(&request, &key, &trust, at).unwrap().sequence, 3);
            assert_eq!(
                verify(&out, Some(next.as_path()), &trust, at)
                    .unwrap()
                    .sequence,
                3
            );
        }
    }

    #[test]
    fn a_summary_counts_the_payload_and_prints_the_expiry_in_rfc_3339() {
        let artifact = |os, arch| Artifact {
            os,
            arch,
            url: String::new(),
            sha256: Sha256Digest::of(b"php"),
            size: 3,
            format: ArchiveFormat::TarGz,
        };
        let runtime = |version: &str, artifacts| Runtime {
            name: RuntimeName::new("php").unwrap(),
            version: RuntimeVersion::new(version).unwrap(),
            license: "PHP-3.01".to_owned(),
            artifacts,
        };
        let payload = Payload {
            schema: SCHEMA,
            sequence: 4,
            // 2027-01-31T00:00:00Z.
            expires: 1_801_353_600,
            runtimes: vec![
                runtime("7.4.33-ck1", vec![artifact(Os::Linux, Arch::X86_64)]),
                runtime(
                    "8.5.12-ck1",
                    vec![
                        artifact(Os::Linux, Arch::X86_64),
                        artifact(Os::Macos, Arch::Aarch64),
                    ],
                ),
            ],
        };
        let summary = Summary::new("0123456789abcdef".to_owned(), &payload);
        assert_eq!(
            summary.render().unwrap(),
            "key_id\t0123456789abcdef\nsequence\t4\nexpires\t2027-01-31T00:00:00Z\nruntimes\t2\nartifacts\t3\n"
        );
        let far = Summary {
            expires: u64::MAX,
            ..summary
        };
        assert!(far.render().is_err());
    }
}
