//! The install path shared by the CLI and, later, the desktop bridge.
//!
//! [`RuntimeService`] is the only app-facing place that takes a signed
//! catalog and installs from it. It uses the trust the caller passes, the
//! transport the caller passes and a catalog URL; it never reads a key and
//! never writes to `catalog/`. An installed version is returned before
//! any catalog request, so installed runtimes keep working when the catalog is
//! expired or no longer lists them.

use std::time::SystemTime;

use crate::Result;
use crate::catalog;
use crate::catalog::schema::{RuntimeName, RuntimeVersion};
use crate::catalog::trust::Trust;
use crate::download::transport::Transport;
use crate::install::{InstallOutcome, InstalledRuntime};
use crate::layout;
use crate::paths::Paths;

/// One runtime install path over an isolated profile.
pub struct RuntimeService<'a> {
    paths: &'a Paths,
    trust: &'a Trust,
    transport: &'a dyn Transport,
    catalog_url: &'a str,
}

impl<'a> RuntimeService<'a> {
    /// Creates the data folders and binds the install path to one profile,
    /// trust set, transport and catalog URL.
    pub fn new(
        paths: &'a Paths,
        trust: &'a Trust,
        transport: &'a dyn Transport,
        catalog_url: &'a str,
    ) -> Result<Self> {
        layout::ensure(paths)?;
        Ok(Self {
            paths,
            trust,
            transport,
            catalog_url,
        })
    }

    /// Installs `name` `version` after refreshing the signed catalog.
    ///
    /// A version already on disk returns [`InstallOutcome::AlreadyPresent`]
    /// without fetching the catalog. `clock` is sampled once after the fetch,
    /// for the catalog's expiry check, and again before the artifact is
    /// selected. A refresh failure is returned to the caller: the cached
    /// catalog is left on disk but this call does not fall back to it.
    pub fn install<F>(&self, name: &str, version: &str, clock: F) -> Result<InstallOutcome>
    where
        F: Fn() -> SystemTime,
    {
        let name = RuntimeName::new(name)?;
        let version = RuntimeVersion::new(version)?;
        if let Some(path) = crate::install::installed_path(self.paths, &name, &version)? {
            return Ok(InstallOutcome::AlreadyPresent(path));
        }
        let catalog = catalog::refresh_with(
            self.paths,
            self.trust,
            self.transport,
            self.catalog_url,
            &clock,
        )?;
        crate::install::install(
            self.paths,
            &catalog,
            self.transport,
            &name,
            &version,
            clock(),
        )
    }

    /// The installed versions, sorted. This touches no catalog and no network.
    pub fn installed(&self) -> Result<Vec<InstalledRuntime>> {
        crate::install::installed(self.paths)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{self, Read};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    use serde_json::json;

    use super::*;
    use crate::Error;
    use crate::catalog::schema::Platform;
    use crate::catalog::trust::SIGNING_CONTEXT;
    use crate::catalog::trust::test_signing::{self, TestKey};
    use crate::download::transport::{FakeReply, FakeTransport, Request, Response, Transport};
    use crate::paths::Profile;
    use crate::test_support::TempDir;

    const NOW: u64 = 1_800_000_000;
    const EXPIRES: u64 = NOW + 60;
    const FIXTURE_CATALOG_URL: &str =
        "https://github.com/example/runtimes/releases/download/v1/catalog-v1.json";
    const FIXTURE_ARTIFACT_URL: &str =
        "https://github.com/example/runtimes/releases/download/v1/demo.tar.gz";

    /// A signed catalog and artifact that expire 60 seconds after `NOW`.
    struct Fixture {
        _root: TempDir,
        paths: Paths,
        trust: Trust,
        transport: FakeTransport,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new("runtime");
            let paths = Paths {
                home: root.path().to_path_buf(),
                data: root.path().join("data"),
                profile: Profile::Isolated,
            };
            let key = TestKey::generate();
            let trust = Trust::new(vec![key.verifying_key()], 0).unwrap();
            let platform = Platform::current().expect("a v1 computer");
            let bytes = b"demo";
            let runtimes = json!([test_signing::runtime(
                "demo",
                "1.0.0",
                json!([test_signing::artifact(
                    platform.os.as_str(),
                    platform.arch.as_str(),
                    FIXTURE_ARTIFACT_URL,
                    "tar.gz",
                    bytes,
                )]),
            )]);
            let envelope = key
                .sign(
                    SIGNING_CONTEXT,
                    &test_signing::payload(1, EXPIRES, runtimes),
                )
                .to_bytes();
            let transport = FakeTransport::new();
            transport.reply(FIXTURE_CATALOG_URL, FakeReply::ok(envelope));
            transport.reply(FIXTURE_ARTIFACT_URL, FakeReply::ok(bytes.to_vec()));
            Self {
                _root: root,
                paths,
                trust,
                transport,
            }
        }

        fn service(&self) -> RuntimeService<'_> {
            RuntimeService::new(
                &self.paths,
                &self.trust,
                &self.transport,
                FIXTURE_CATALOG_URL,
            )
            .unwrap()
        }
    }

    fn at(unix_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    /// The catalog is accepted before it expires, then the clock advances
    /// before the artifact is chosen. The cache is written, but no artifact
    /// is requested from that expired catalog.
    #[test]
    fn a_clock_that_advances_after_the_refresh_installs_nothing_after_expiry() {
        let fixture = Fixture::new();
        let service = fixture.service();
        let ticks = Cell::new(0);
        let clock = || {
            let tick = ticks.get();
            ticks.set(tick + 1);
            at(if tick == 0 { EXPIRES - 1 } else { EXPIRES })
        };

        let result = service.install("demo", "1.0.0", clock);
        assert!(
            matches!(result, Err(Error::CatalogExpired { expires: EXPIRES })),
            "{result:?}"
        );
        assert_eq!(ticks.get(), 2);
        assert_eq!(
            fixture.transport.requests().len(),
            1,
            "the artifact was fetched"
        );
        assert_eq!(fixture.transport.requests()[0].url, FIXTURE_CATALOG_URL);
        assert!(
            catalog::cached(&fixture.paths, &fixture.trust)
                .unwrap()
                .is_some()
        );
    }

    /// Advances the shared clock while the catalog body is being read, so a
    /// sample taken before the fetch would see a live catalog and a sample
    /// taken after it sees an expired one.
    struct ExpiringTransport<'a> {
        inner: &'a FakeTransport,
        clock: Arc<AtomicU64>,
    }

    impl Transport for ExpiringTransport<'_> {
        fn get(&self, request: &Request<'_>) -> Result<Response> {
            let mut response = self.inner.get(request)?;
            if request.url.as_str() == FIXTURE_CATALOG_URL {
                response.body = Box::new(ExpiringBody {
                    inner: response.body,
                    clock: Arc::clone(&self.clock),
                });
            }
            Ok(response)
        }
    }

    struct ExpiringBody {
        inner: Box<dyn Read>,
        clock: Arc<AtomicU64>,
    }

    impl Read for ExpiringBody {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.clock.store(EXPIRES, Ordering::SeqCst);
            Ok(read)
        }
    }

    /// If the clock reaches `expires` while the fetch is in progress,
    /// `accept` refuses the catalog before any cache or artifact write.
    #[test]
    fn a_catalog_that_expires_during_the_fetch_is_not_cached_or_installed() {
        let fixture = Fixture::new();
        let clock = Arc::new(AtomicU64::new(NOW));
        let result = {
            let transport = ExpiringTransport {
                inner: &fixture.transport,
                clock: Arc::clone(&clock),
            };
            let service = RuntimeService::new(
                &fixture.paths,
                &fixture.trust,
                &transport,
                FIXTURE_CATALOG_URL,
            )
            .unwrap();
            service.install("demo", "1.0.0", || at(clock.load(Ordering::SeqCst)))
        };
        assert!(
            matches!(result, Err(Error::CatalogExpired { expires: EXPIRES })),
            "{result:?}"
        );
        assert_eq!(
            clock.load(Ordering::SeqCst),
            EXPIRES,
            "the body was not read"
        );
        assert_eq!(
            fixture.transport.requests().len(),
            1,
            "the artifact was fetched"
        );
        assert_eq!(fixture.transport.requests()[0].url, FIXTURE_CATALOG_URL);
        assert!(
            catalog::cached(&fixture.paths, &fixture.trust)
                .unwrap()
                .is_none()
        );
    }
}
