//! The signed catalog (D3, D6): accepting a new one, reading the cached one,
//! and fetching one.
//!
//! [`accept`] checks, in this order: the size cap, the envelope, the
//! signature, the payload, then expiry. Only then does it touch the disk.
//! Under `state/catalog.lock` it compares the sequence with the stored state
//! ([`state::decide`]), then writes the exact envelope bytes to
//! `catalog/catalog.json` and then the new state, each with
//! `layout::write_atomic`. A refused catalog leaves both files as they were,
//! so the cached catalog stays in use while it is not below the floor.
//!
//! Every function here expects `layout::ensure` to have created the data
//! folders. App code passes [`Trust::release`]; tests pass their own keys.

pub mod envelope;
pub mod schema;
pub mod state;
pub mod trust;

use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::download::fetch;
use crate::download::policy::TransportPolicy;
use crate::download::transport::{CATALOG_TIMEOUT, Transport};
use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;

use self::schema::{
    Artifact, Payload, Platform, Runtime, RuntimeName, RuntimeVersion, Sha256Digest,
};
use self::state::{CatalogLock, CatalogState, Decision};
use self::trust::Trust;

/// A catalog whose signature, payload and sequence were checked.
/// Only [`accept`] and [`cached`] build one, and an install takes one, so
/// nothing installs from an unchecked catalog.
#[derive(Debug, Clone)]
pub struct Catalog {
    payload: Payload,
    payload_sha256: Sha256Digest,
}

impl Catalog {
    pub fn sequence(&self) -> u64 {
        self.payload.sequence
    }

    /// Unix seconds.
    pub fn expires(&self) -> u64 {
        self.payload.expires
    }

    pub fn runtimes(&self) -> &[Runtime] {
        &self.payload.runtimes
    }

    /// The SHA-256 of the decoded payload bytes.
    pub fn payload_sha256(&self) -> &Sha256Digest {
        &self.payload_sha256
    }

    /// The artifact of `name` `version` for `platform`.
    ///
    /// [`Error::CatalogExpired`] when `now` is at or after `expires`: this is
    /// the check that stops installs from an expired cached catalog, and it
    /// shares one expiry rule with [`accept`]. [`Error::NotInCatalog`] when
    /// the catalog has no such artifact.
    pub fn artifact(
        &self,
        name: &RuntimeName,
        version: &RuntimeVersion,
        platform: Platform,
        now: SystemTime,
    ) -> Result<&Artifact> {
        check_expiry(self.payload.expires, now)?;
        // One version may be split over several entries, so every entry is
        // searched. The version matches exactly, so the folder is always
        // named as the catalog spells it.
        self.payload
            .runtimes
            .iter()
            .filter(|runtime| &runtime.name == name && &runtime.version == version)
            .flat_map(|runtime| &runtime.artifacts)
            .find(|artifact| artifact.os == platform.os && artifact.arch == platform.arch)
            .ok_or_else(|| Error::NotInCatalog {
                name: name.to_string(),
                version: version.to_string(),
                platform: platform.to_string(),
            })
    }
}

/// Checks `bytes` (a whole envelope) against `trust` and `now`, and stores it
/// as the cached catalog when it is newer (see the module notes).
///
/// Errors: [`Error::CatalogInvalid`], [`Error::CatalogUntrusted`],
/// [`Error::CatalogExpired`] (when `now` is at or after `expires`),
/// [`Error::CatalogOutdated`], [`Error::CatalogConflict`], and
/// [`Error::Timeout`] when the lock stays held.
pub fn accept(paths: &Paths, trust: &Trust, bytes: &[u8], now: SystemTime) -> Result<Catalog> {
    let catalog = check(bytes, trust)?;
    check_expiry(catalog.expires(), now)?;

    let _lock = CatalogLock::acquire(paths)?;
    let offered = CatalogState {
        sequence: catalog.sequence(),
        payload_sha256: catalog.payload_sha256,
    };
    let stored = state::read(paths)?;
    match state::decide(stored.as_ref(), &offered, trust.min_sequence())? {
        // The catalog before the state: `cached` relies on the cached
        // catalog never being older than the state.
        Decision::Store => {
            layout::write_atomic(&paths.catalog_file(), bytes)?;
            state::write(paths, &offered)?;
        }
        Decision::Unchanged => {}
    }
    Ok(catalog)
}

/// The cached catalog, checked again: its signature against `trust`, its
/// payload, and a sequence no lower than the stored state's or
/// `trust.min_sequence()`. `Ok(None)` when none is cached. An expired catalog
/// is returned, but [`Catalog::artifact`] refuses to install from it.
///
/// A cache below the floor (an app update raised it) gives
/// [`Error::CatalogOutdated`] until a refresh stores a newer catalog.
/// Installed versions stay usable, since using one needs no catalog.
pub fn cached(paths: &Paths, trust: &Trust) -> Result<Option<Catalog>> {
    // The state is read first. `accept` writes the catalog before the state,
    // so the catalog read next is never older than this state, even while an
    // accept runs: no lock is needed.
    let stored = state::read(paths)?;
    let path = paths.catalog_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read", path, e)),
    };
    let catalog = check(&bytes, trust)?;
    if let Some(stored) = stored
        && catalog.sequence() < stored.sequence
    {
        return Err(Error::CatalogOutdated {
            offered: catalog.sequence(),
            current: stored.sequence,
        });
    }
    if catalog.sequence() < trust.min_sequence() {
        return Err(Error::CatalogOutdated {
            offered: catalog.sequence(),
            current: trust.min_sequence(),
        });
    }
    Ok(Some(catalog))
}

/// Fetches the envelope at `url` through `transport`, under
/// `TransportPolicy::current(paths.profile)`, at most
/// [`envelope::MAX_CATALOG_BYTES`] and within [`CATALOG_TIMEOUT`], then
/// [`accept`]s it. When it fails, the cached catalog stays in use.
pub fn refresh(
    paths: &Paths,
    trust: &Trust,
    transport: &dyn Transport,
    url: &str,
    now: SystemTime,
) -> Result<Catalog> {
    let policy = TransportPolicy::current(paths.profile);
    let bytes = fetch::fetch_bytes(
        transport,
        &policy,
        url,
        envelope::MAX_CATALOG_BYTES,
        CATALOG_TIMEOUT,
    )?;
    accept(paths, trust, &bytes, now)
}

/// The checks that [`accept`] and [`cached`] share: the envelope and its
/// signature, then the payload.
fn check(bytes: &[u8], trust: &Trust) -> Result<Catalog> {
    let payload_bytes = envelope::verify(bytes, trust.keys())?;
    let payload = schema::parse(&payload_bytes)?;
    Ok(Catalog {
        payload,
        payload_sha256: Sha256Digest::of(&payload_bytes),
    })
}

/// The one expiry rule, for [`accept`] and [`Catalog::artifact`]: nothing is
/// accepted or installed at or after `expires`. A clock set before 1970
/// reads as 1970.
fn check_expiry(expires: u64, now: SystemTime) -> Result<()> {
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if now < expires {
        Ok(())
    } else {
        Err(Error::CatalogExpired { expires })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::catalog::schema::{Arch, MAX_ARTIFACT_BYTES, Os};
    use crate::catalog::trust::SIGNING_CONTEXT;
    use crate::catalog::trust::test_signing::{self, TestKey};
    use crate::paths::Profile;
    use crate::test_support::TempDir;

    const NOW: u64 = 1_800_000_000;
    const EXPIRES: u64 = NOW + 90 * 24 * 60 * 60;
    const URL: &str = "https://github.com/example/runtimes/releases/download/v1/php.tar.gz";

    fn at(unix_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    /// An isolated profile whose data folder does not exist yet.
    fn profile(root: &TempDir) -> Paths {
        Paths {
            home: root.path().to_path_buf(),
            data: root.path().join("data"),
            profile: Profile::Isolated,
        }
    }

    fn ensured(root: &TempDir) -> Paths {
        let paths = profile(root);
        layout::ensure(&paths).unwrap();
        paths
    }

    fn trusting(keys: &[&TestKey], min_sequence: u64) -> Trust {
        Trust::new(
            keys.iter().map(|key| key.verifying_key()).collect(),
            min_sequence,
        )
        .unwrap()
    }

    /// php 8.4.3 for Linux x86_64.
    fn php() -> Value {
        json!([test_signing::runtime(
            "php",
            "8.4.3",
            json!([test_signing::artifact(
                "linux", "x86_64", URL, "tar.gz", b"php"
            )]),
        )])
    }

    fn signed(key: &TestKey, payload: &[u8]) -> Vec<u8> {
        key.sign(SIGNING_CONTEXT, payload).to_bytes()
    }

    /// A catalog of [`php`] at `sequence`, signed by `key`.
    fn php_catalog(key: &TestKey, sequence: u64) -> Vec<u8> {
        signed(key, &test_signing::payload(sequence, EXPIRES, php()))
    }

    fn stored(paths: &Paths) -> Option<CatalogState> {
        state::read(paths).unwrap()
    }

    #[test]
    fn a_signed_payload_that_breaks_a_rule_is_refused_before_the_disk_is_touched() {
        let root = TempDir::new("catalog");
        let paths = profile(&root);
        let key = TestKey::generate();
        let trust = trusting(&[&key], 0);
        let base: Value =
            serde_json::from_slice(&test_signing::payload(5, EXPIRES, php())).unwrap();

        type BreakRule = fn(&mut Value);
        let cases: &[(&str, BreakRule)] = &[
            ("an unknown payload field", |p: &mut Value| {
                p["mirror"] = json!("x");
            }),
            ("an unknown runtime field", |p: &mut Value| {
                p["runtimes"][0]["mirror"] = json!("x");
            }),
            ("an unknown artifact field", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["mirror"] = json!("x");
            }),
            ("no expiry", |p: &mut Value| {
                p.as_object_mut().unwrap().remove("expires");
            }),
            ("schema 2", |p: &mut Value| {
                p["schema"] = json!(2);
            }),
            ("an upper-case name", |p: &mut Value| {
                p["runtimes"][0]["name"] = json!("PHP");
            }),
            ("a device name", |p: &mut Value| {
                p["runtimes"][0]["name"] = json!("nul");
            }),
            ("a version with '..'", |p: &mut Value| {
                p["runtimes"][0]["version"] = json!("8..4");
            }),
            ("a version that climbs out", |p: &mut Value| {
                p["runtimes"][0]["version"] = json!("../8");
            }),
            ("a version with a trailing dot", |p: &mut Value| {
                p["runtimes"][0]["version"] = json!("8.4.");
            }),
            ("an upper-case hash", |p: &mut Value| {
                let hash = &mut p["runtimes"][0]["artifacts"][0]["sha256"];
                *hash = json!(hash.as_str().unwrap().to_ascii_uppercase());
            }),
            ("a short hash", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["sha256"] = json!("abc");
            }),
            ("size 0", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["size"] = json!(0);
            }),
            ("a size over 2 GiB", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["size"] = json!(MAX_ARTIFACT_BYTES + 1);
            }),
            ("an unsupported OS", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["os"] = json!("freebsd");
            }),
            ("an unsupported CPU", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["arch"] = json!("riscv64");
            }),
            ("an unsupported format", |p: &mut Value| {
                p["runtimes"][0]["artifacts"][0]["format"] = json!("tar.xz");
            }),
            ("a duplicate artifact", |p: &mut Value| {
                let copy = p["runtimes"][0]["artifacts"][0].clone();
                p["runtimes"][0]["artifacts"]
                    .as_array_mut()
                    .unwrap()
                    .push(copy);
            }),
            ("a duplicate in a second entry", |p: &mut Value| {
                let copy = p["runtimes"][0].clone();
                p["runtimes"].as_array_mut().unwrap().push(copy);
            }),
            ("a version that differs only in case", |p: &mut Value| {
                p["runtimes"][0]["version"] = json!("8.4.3rc1");
                let mut copy = p["runtimes"][0].clone();
                copy["version"] = json!("8.4.3RC1");
                p["runtimes"].as_array_mut().unwrap().push(copy);
            }),
        ];
        for (case, break_rule) in cases {
            let mut payload = base.clone();
            break_rule(&mut payload);
            let bytes = signed(&key, &serde_json::to_vec(&payload).unwrap());
            let result = accept(&paths, &trust, &bytes, at(NOW));
            assert!(
                matches!(result, Err(Error::CatalogInvalid { .. })),
                "{case}: {result:?}"
            );
            assert!(!paths.data.exists(), "{case}");
        }

        // The signature is checked before the payload is read.
        let mut forged = key.sign(SIGNING_CONTEXT, b"not json");
        forged.signature[0] ^= 1;
        let result = accept(&paths, &trust, &forged.to_bytes(), at(NOW));
        assert!(
            matches!(result, Err(Error::CatalogUntrusted { .. })),
            "{result:?}"
        );
        assert!(!paths.data.exists());

        // Versions that differ by more than case are two versions.
        layout::ensure(&paths).unwrap();
        let mut two_versions = base;
        two_versions["runtimes"][0]["version"] = json!("8.4.3rc1");
        let mut rc2 = two_versions["runtimes"][0].clone();
        rc2["version"] = json!("8.4.3rc2");
        two_versions["runtimes"].as_array_mut().unwrap().push(rc2);
        let bytes = signed(&key, &serde_json::to_vec(&two_versions).unwrap());
        let catalog = accept(&paths, &trust, &bytes, at(NOW)).unwrap();
        assert_eq!(catalog.runtimes().len(), 2);
    }

    #[test]
    fn the_sequence_only_goes_up_and_a_refused_catalog_keeps_the_cached_one() {
        let root = TempDir::new("catalog");
        let paths = ensured(&root);
        let first = TestKey::generate();
        let second = TestKey::generate();
        let trust = trusting(&[&first, &second], 0);
        let payload_five = test_signing::payload(5, EXPIRES, php());
        let five = signed(&first, &payload_five);

        // First use.
        assert_eq!(
            accept(&paths, &trust, &five, at(NOW)).unwrap().sequence(),
            5
        );
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), five);
        assert_eq!(
            stored(&paths),
            Some(CatalogState {
                sequence: 5,
                payload_sha256: Sha256Digest::of(&payload_five),
            })
        );

        // The same payload in other envelope bytes: accepted, nothing written.
        let five_again = signed(&second, &payload_five);
        assert_ne!(five_again, five);
        assert_eq!(
            accept(&paths, &trust, &five_again, at(NOW))
                .unwrap()
                .sequence(),
            5
        );
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), five);

        // The same sequence with other contents.
        let five_other = signed(&first, &test_signing::payload(5, EXPIRES + 1, php()));
        let result = accept(&paths, &trust, &five_other, at(NOW));
        assert!(
            matches!(result, Err(Error::CatalogConflict { sequence: 5 })),
            "{result:?}"
        );
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), five);

        // A higher sequence.
        let six = php_catalog(&first, 6);
        assert_eq!(accept(&paths, &trust, &six, at(NOW)).unwrap().sequence(), 6);
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), six);

        // A lower sequence: refused, and the cached catalog stays in use.
        let result = accept(&paths, &trust, &five, at(NOW));
        assert!(
            matches!(
                result,
                Err(Error::CatalogOutdated {
                    offered: 5,
                    current: 6
                })
            ),
            "{result:?}"
        );
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), six);
        assert_eq!(stored(&paths).map(|state| state.sequence), Some(6));
        assert_eq!(
            cached(&paths, &trust)
                .unwrap()
                .map(|catalog| catalog.sequence()),
            Some(6)
        );
    }

    #[test]
    fn the_floor_refuses_older_catalogs_on_first_use_and_after() {
        let root = TempDir::new("catalog");
        let paths = ensured(&root);
        let key = TestKey::generate();

        let result = accept(
            &paths,
            &trusting(&[&key], 5),
            &php_catalog(&key, 4),
            at(NOW),
        );
        assert!(
            matches!(
                result,
                Err(Error::CatalogOutdated {
                    offered: 4,
                    current: 5
                })
            ),
            "{result:?}"
        );
        assert!(!paths.catalog_file().exists());
        assert_eq!(stored(&paths), None);

        // An older app with a lower floor accepted 3; this one's floor is 10.
        accept(
            &paths,
            &trusting(&[&key], 0),
            &php_catalog(&key, 3),
            at(NOW),
        )
        .unwrap();
        let floor_ten = trusting(&[&key], 10);
        let result = accept(&paths, &floor_ten, &php_catalog(&key, 7), at(NOW));
        assert!(
            matches!(
                result,
                Err(Error::CatalogOutdated {
                    offered: 7,
                    current: 10
                })
            ),
            "{result:?}"
        );
        assert_eq!(stored(&paths).map(|state| state.sequence), Some(3));
        // The cached 3 installs nothing either, until a refresh succeeds.
        let result = cached(&paths, &floor_ten).map(|cached| cached.map(|c| c.sequence()));
        assert!(
            matches!(
                result,
                Err(Error::CatalogOutdated {
                    offered: 3,
                    current: 10
                })
            ),
            "{result:?}"
        );
        assert_eq!(
            accept(&paths, &floor_ten, &php_catalog(&key, 10), at(NOW))
                .unwrap()
                .sequence(),
            10
        );
        assert_eq!(
            cached(&paths, &floor_ten)
                .unwrap()
                .map(|catalog| catalog.sequence()),
            Some(10)
        );
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_never_first_use() {
        let root = TempDir::new("catalog");
        let paths = ensured(&root);
        let key = TestKey::generate();
        let trust = trusting(&[&key], 0);
        let five = php_catalog(&key, 5);
        accept(&paths, &trust, &five, at(NOW)).unwrap();

        let corrupt = b"{\"sequence\": 5".as_slice();
        fs::write(paths.catalog_state_file(), corrupt).unwrap();
        let result = accept(&paths, &trust, &php_catalog(&key, 6), at(NOW));
        assert!(matches!(result, Err(Error::BadFile { .. })), "{result:?}");
        assert_eq!(fs::read(paths.catalog_file()).unwrap(), five);
        assert_eq!(fs::read(paths.catalog_state_file()).unwrap(), corrupt);

        let result = cached(&paths, &trust);
        assert!(matches!(result, Err(Error::BadFile { .. })), "{result:?}");
    }

    #[test]
    fn the_cached_catalog_is_checked_again_when_read() {
        let root = TempDir::new("catalog");
        let paths = ensured(&root);
        let key = TestKey::generate();
        let trust = trusting(&[&key], 0);
        assert!(cached(&paths, &trust).unwrap().is_none());

        let five = php_catalog(&key, 5);
        accept(&paths, &trust, &five, at(NOW)).unwrap();
        accept(&paths, &trust, &php_catalog(&key, 6), at(NOW)).unwrap();
        assert_eq!(
            cached(&paths, &trust)
                .unwrap()
                .map(|catalog| catalog.sequence()),
            Some(6)
        );

        let result = cached(&paths, &trusting(&[&TestKey::generate()], 0));
        assert!(
            matches!(result, Err(Error::CatalogUntrusted { .. })),
            "{result:?}"
        );

        let mut tampered = key.sign(SIGNING_CONTEXT, &test_signing::payload(6, EXPIRES, php()));
        tampered.payload[0] ^= 1;
        layout::write_atomic(&paths.catalog_file(), &tampered.to_bytes()).unwrap();
        let result = cached(&paths, &trust);
        assert!(
            matches!(result, Err(Error::CatalogUntrusted { .. })),
            "{result:?}"
        );

        // An older catalog put back in the cache.
        layout::write_atomic(&paths.catalog_file(), &five).unwrap();
        let result = cached(&paths, &trust);
        assert!(
            matches!(
                result,
                Err(Error::CatalogOutdated {
                    offered: 5,
                    current: 6
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn an_expired_catalog_is_refused_at_accept_and_installs_nothing_once_cached() {
        let root = TempDir::new("catalog");
        let paths = ensured(&root);
        let key = TestKey::generate();
        let trust = trusting(&[&key], 0);
        // php 8.4.3 for Linux in one entry and for macOS in another.
        let runtimes = json!([
            test_signing::runtime(
                "php",
                "8.4.3",
                json!([test_signing::artifact(
                    "linux", "x86_64", URL, "tar.gz", b"linux"
                )]),
            ),
            test_signing::runtime(
                "php",
                "8.4.3",
                json!([test_signing::artifact(
                    "macos", "aarch64", URL, "tar.gz", b"macos"
                )]),
            ),
        ]);
        let bytes = signed(&key, &test_signing::payload(5, EXPIRES, runtimes));

        let result = accept(&paths, &trust, &bytes, at(EXPIRES));
        assert!(
            matches!(result, Err(Error::CatalogExpired { expires: EXPIRES })),
            "{result:?}"
        );
        assert!(!paths.catalog_file().exists());
        accept(&paths, &trust, &bytes, at(EXPIRES - 1)).unwrap();

        // Read back after it expired: it can be read, but installs nothing.
        let catalog = cached(&paths, &trust).unwrap().unwrap();
        let php = RuntimeName::new("php").unwrap();
        let version = RuntimeVersion::new("8.4.3").unwrap();
        let mac = Platform {
            os: Os::Macos,
            arch: Arch::Aarch64,
        };
        let artifact = catalog
            .artifact(&php, &version, mac, at(EXPIRES - 1))
            .unwrap();
        assert_eq!(artifact.sha256, Sha256Digest::of(b"macos"));
        let result = catalog.artifact(&php, &version, mac, at(EXPIRES));
        assert!(
            matches!(result, Err(Error::CatalogExpired { expires: EXPIRES })),
            "{result:?}"
        );

        let windows = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        };
        let result = catalog.artifact(&php, &version, windows, at(NOW));
        assert!(
            matches!(result, Err(Error::NotInCatalog { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn concurrent_accepts_never_leave_an_older_sequence_stored() {
        let key = TestKey::generate();
        let trust = trusting(&[&key], 0);
        let five = php_catalog(&key, 5);
        let six = php_catalog(&key, 6);
        for _ in 0..20 {
            let root = TempDir::new("catalog");
            let paths = ensured(&root);
            let barrier = Barrier::new(2);
            let offer = |bytes: &[u8]| {
                barrier.wait();
                accept(&paths, &trust, bytes, at(NOW))
            };
            let (older, newer) = thread::scope(|scope| {
                let older = scope.spawn(|| offer(five.as_slice()));
                let newer = scope.spawn(|| offer(six.as_slice()));
                (older.join().unwrap(), newer.join().unwrap())
            });

            // Whichever ran first, 5 is refused only for being older than 6.
            assert!(
                matches!(
                    older,
                    Ok(_)
                        | Err(Error::CatalogOutdated {
                            offered: 5,
                            current: 6
                        })
                ),
                "{older:?}"
            );
            assert_eq!(newer.unwrap().sequence(), 6);
            assert_eq!(stored(&paths).map(|state| state.sequence), Some(6));
            assert_eq!(
                cached(&paths, &trust)
                    .unwrap()
                    .map(|catalog| catalog.sequence()),
                Some(6)
            );
        }
    }
}
