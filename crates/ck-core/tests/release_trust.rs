//! A release build trusts only the catalog keys built into it, and never
//! requests plain HTTP, even when it is handed an isolated profile (D6, D21).
//!
//! Release builds only: `cargo test -p ck-core --release --locked --test
//! release_trust`. Each test asserts a refusal, never an empty key slot, so
//! it keeps passing once the owner adds the real key.
#![cfg(not(debug_assertions))]

#[path = "support/signing.rs"]
mod signing;

use std::fs;
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ck_core::Error;
use ck_core::catalog;
use ck_core::catalog::trust::{SIGNING_CONTEXT, Trust};
use ck_core::download::policy::TransportPolicy;
use ck_core::layout;
use ck_core::paths::{Paths, Profile};
use serde_json::json;
use url::Url;

use signing::TestKey;

const NOW: u64 = 1_800_000_000;
const EXPIRES: u64 = NOW + 90 * 24 * 60 * 60;
const URL: &str = "https://github.com/example/runtimes/releases/download/v1/php.tar.gz";

fn at(unix_seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(unix_seconds)
}

/// A new folder under the system's temporary folder, removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn create() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        // The time keeps a folder left by an earlier run with the same pid
        // from being reused: creating it fails instead.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let name = format!(
            "ck-release-trust-{}-{}-{nanos}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", path.display()));
        Self(path)
    }

    /// A test profile inside this folder, built by hand: `Paths::current`
    /// in a release build would resolve to the user's real folders.
    fn paths(&self) -> Paths {
        Paths {
            home: self.0.clone(),
            data: self.0.join("data"),
            profile: Profile::Isolated,
        }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_catalog_signed_by_a_key_the_app_does_not_hold_is_refused() {
    let key = TestKey::generate();
    let payload = signing::payload(
        1,
        EXPIRES,
        json!([signing::runtime(
            "php",
            "8.4.3",
            json!([signing::artifact("linux", "x86_64", URL, "tar.gz", b"php")]),
        )]),
    );
    let envelope = key.sign(SIGNING_CONTEXT, &payload).to_bytes();

    // Trusting the test key accepts the same bytes, so below only the
    // app's own keys can refuse them.
    let control = TempRoot::create();
    let control_paths = control.paths();
    layout::ensure(&control_paths).expect("create the control's data folder");
    let trusted = Trust::new(vec![key.verifying_key()], 0).expect("a strong test key");
    catalog::accept(&control_paths, &trusted, &envelope, at(NOW))
        .expect("the test key's own trust accepts the catalog");

    let root = TempRoot::create();
    let paths = root.paths();
    fs::create_dir(&paths.data).expect("create an empty data folder");
    let release = Trust::release().expect("the built-in keys load");
    let result = catalog::accept(&paths, &release, &envelope, at(NOW));
    assert!(
        matches!(result, Err(Error::CatalogUntrusted { .. })),
        "{result:?}"
    );
    assert!(
        fs::read_dir(&paths.data)
            .expect("list the data folder")
            .next()
            .is_none(),
        "a refused catalog wrote into {}",
        paths.data.display()
    );
}

#[test]
fn plain_http_to_loopback_is_refused_even_in_an_isolated_profile() {
    let policy = TransportPolicy::current(Profile::Isolated);
    assert!(!policy.allows_loopback_http());
    for text in [
        "http://127.0.0.1:8080/catalog-v1.json",
        "http://[::1]:8080/catalog-v1.json",
    ] {
        let url = Url::parse(text).expect("a valid URL");
        let result = policy.check_first(&url);
        assert!(
            matches!(result, Err(Error::UrlRefused { .. })),
            "{text}: {result:?}"
        );
    }
}
