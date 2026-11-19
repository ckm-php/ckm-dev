//! What is published on GitHub Releases, fetched over real HTTPS through
//! [`HttpTransport`] and the same ck-core calls the app makes
//! (`catalog::refresh`, `install::install`), into a temporary data folder.
//!
//! `#[ignore]`: it needs the network, and `catalog-check.yml` runs it. The
//! mode comes from `CK_PUBLISHED_CHECK`; an unset or unknown mode fails.
//!
//! - `probe`: refreshes from `transport-probe.txt` on the `catalog-v1`
//!   release, and expects [`Error::CatalogInvalid`], which means the bytes
//!   arrived: the TLS handshake, GitHub's redirect and the strict parsing of
//!   its `Location` all passed.
//! - `fetch`: refreshes from [`CATALOG_URL`] with [`Trust::release`], prints
//!   the sequence, expiry and signing key id, and fails when fewer than
//!   `CK_PUBLISHED_MIN_DAYS` days are left, when that is set.
//! - `install`: `fetch`, then installs every version this computer has an
//!   artifact for. Nothing is run, so it may run anywhere.
//! - `run`: `install`, then runs each PHP with the relocation contract
//!   (`support/php.rs`) and the smoke script named by `CK_PHP_SMOKE`. It
//!   runs a downloaded PHP, so it refuses unless `CI` or `GITHUB_ACTIONS`
//!   is set. On Windows nothing is run, so it is `install`.
//!
//! The data folder is built by hand under a temporary folder and removed on
//! drop, never the user's. Its profile is [`Profile::Standard`], because
//! `refresh` and `install` take their policy from the profile: this gives the
//! rules of a release build (HTTPS only, GitHub's hosts only) in a debug
//! build too, where an isolated profile would also allow plain HTTP to
//! loopback.

#[cfg(unix)]
#[path = "support/php.rs"]
mod php;

use std::env;
use std::fs::{self, DirBuilder};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ck_core::Error;
use ck_core::catalog::schema::{Platform, RuntimeName, RuntimeVersion};
use ck_core::catalog::trust::{CATALOG_URL, Trust, release_download_prefix};
use ck_core::catalog::{Catalog, refresh};
use ck_core::download::policy::TransportPolicy;
use ck_core::install::{InstallOutcome, install};
use ck_core::layout;
use ck_core::paths::{Paths, Profile};
use ck_net::HttpTransport;

const CHECK_VAR: &str = "CK_PUBLISHED_CHECK";
const MIN_DAYS_VAR: &str = "CK_PUBLISHED_MIN_DAYS";
#[cfg(unix)]
const SMOKE_VAR: &str = "CK_PHP_SMOKE";

/// The probe asset, under the release download prefix.
const PROBE: &str = "catalog-v1/transport-probe.txt";

const DAY_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    Probe,
    Fetch,
    Install,
    Run,
}

impl Check {
    /// The mode in [`CHECK_VAR`]. Nothing has a default, so a typo never
    /// passes by checking less.
    fn from_env() -> Self {
        let value = env::var(CHECK_VAR).unwrap_or_default();
        match value.as_str() {
            "probe" => Self::Probe,
            "fetch" => Self::Fetch,
            "install" => Self::Install,
            "run" => Self::Run,
            _ => panic!("set {CHECK_VAR} to probe, fetch, install or run (it is {value:?})"),
        }
    }
}

#[test]
#[ignore = "network: GitHub Releases; catalog-check.yml runs it with CK_PUBLISHED_CHECK"]
fn the_published_catalog_and_runtimes_fetch_and_install_over_https() {
    let check = Check::from_env();
    // Before any request, so `--ignored` never runs a downloaded PHP on
    // a developer machine.
    let runner = (check == Check::Run).then(Runner::prepare);
    let min_days = min_days();

    // A space in the path, as a user's data folder may have.
    let root = TempRoot::create("ck net-published");
    let paths = Paths {
        home: root.path().to_path_buf(),
        data: root.path().join("data"),
        profile: Profile::Standard,
    };
    layout::ensure(&paths).unwrap();
    // The transport and `refresh` take the same policy, as in the app.
    let transport = HttpTransport::new(&TransportPolicy::current(paths.profile));
    let trust = Trust::release().unwrap();

    if check == Check::Probe {
        probe(&paths, &trust, &transport);
        return;
    }

    let catalog = fetch(&paths, &trust, &transport, min_days);
    if check == Check::Fetch {
        return;
    }

    let installed = install_all(&paths, &catalog, &transport);
    if let Some(runner) = runner {
        runner.run(&installed);
    }
}

/// One installed version: its name, version and folder in the data folder.
type Installed = (RuntimeName, RuntimeVersion, PathBuf);

/// `run`: runs each installed PHP from its folder.
#[cfg(unix)]
struct Runner {
    smoke: PathBuf,
}

#[cfg(unix)]
impl Runner {
    /// Called before any request: refuses outside CI, and needs the
    /// smoke script.
    fn prepare() -> Self {
        php::require_ci();
        let smoke = env::var_os(SMOKE_VAR)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("run needs {SMOKE_VAR}: the smoke script's path"));
        assert!(smoke.is_absolute(), "{SMOKE_VAR} must be absolute");
        assert!(smoke.is_file(), "{SMOKE_VAR} is not a file");
        Self { smoke }
    }

    fn run(&self, installed: &[Installed]) {
        for (name, version, dir) in installed {
            if name.as_str() != "php" {
                eprintln!("{name} {version}: nothing to run");
                continue;
            }
            let patch = version.as_str().split('-').next().unwrap();
            let started = Instant::now();
            php::check_installed(dir, patch, Some(self.smoke.as_os_str()));
            eprintln!(
                "{name} {version}: php -v, the smoke script and each extension passed in {:?}",
                started.elapsed()
            );
        }
    }
}

/// Nothing is run on Windows, which has no PHP archives yet (M2c), so `run`
/// is `install` there.
#[cfg(not(unix))]
struct Runner;

#[cfg(not(unix))]
impl Runner {
    fn prepare() -> Self {
        Self
    }

    fn run(&self, installed: &[Installed]) {
        eprintln!(
            "run: nothing is run on this OS; {} installed",
            installed.len()
        );
    }
}

/// `probe`: the probe file reaches the catalog checks, so every transport
/// step before them passed.
fn probe(paths: &Paths, trust: &Trust, transport: &HttpTransport) {
    let prefix = release_download_prefix().expect("CATALOG_URL is a release download URL");
    let url = format!("{prefix}{PROBE}");
    let error = refresh(paths, trust, transport, &url, SystemTime::now())
        .expect_err("the probe file is not a catalog");
    eprintln!("probe: {error}");
    assert!(
        matches!(error, Error::CatalogInvalid { .. }),
        "expected a catalog-format error, which comes only once the bytes arrived; got {error:?}"
    );
    assert!(
        !paths.catalog_file().exists(),
        "a refused catalog was stored"
    );
}

/// `fetch`: the published catalog, accepted under the built-in trust.
fn fetch(
    paths: &Paths,
    trust: &Trust,
    transport: &HttpTransport,
    min_days: Option<u64>,
) -> Catalog {
    let url = CATALOG_URL.expect("a catalog URL is built in");
    let now = SystemTime::now();
    let catalog = refresh(paths, trust, transport, url, now)
        .unwrap_or_else(|error| panic!("the published catalog is refused: {error}"));

    let now_seconds = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let left = catalog.expires().saturating_sub(now_seconds);
    eprintln!(
        "catalog: sequence {}, expires at {} (Unix seconds, {} days {} hours from now), \
         signed by key {}, {} runtime entries",
        catalog.sequence(),
        catalog.expires(),
        left / DAY_SECONDS,
        left % DAY_SECONDS / 3600,
        stored_key_id(paths),
        catalog.runtimes().len()
    );
    if let Some(days) = min_days {
        assert!(
            left >= days.saturating_mul(DAY_SECONDS),
            "the catalog expires in less than {days} days: sign a new one"
        );
    }
    catalog
}

/// `install`: every version this computer has an artifact for, each into its
/// own folder.
fn install_all(paths: &Paths, catalog: &Catalog, transport: &HttpTransport) -> Vec<Installed> {
    let platform = Platform::current().expect("a v1 computer");
    // One version may be split over several entries; it installs once.
    let mut versions: Vec<(RuntimeName, RuntimeVersion)> = Vec::new();
    for runtime in catalog.runtimes() {
        let here = runtime
            .artifacts
            .iter()
            .any(|artifact| artifact.os == platform.os && artifact.arch == platform.arch);
        let version = (runtime.name.clone(), runtime.version.clone());
        if here && !versions.contains(&version) {
            versions.push(version);
        }
    }
    // Windows gets its archives later (M2c); Linux and macOS must have some.
    if cfg!(unix) {
        assert!(
            !versions.is_empty(),
            "the catalog has nothing for {platform}"
        );
    } else if versions.is_empty() {
        eprintln!("install: the catalog has nothing for {platform} yet");
    }

    let mut installed = Vec::new();
    for (name, version) in versions {
        let now = SystemTime::now();
        let size = catalog
            .artifact(&name, &version, platform, now)
            .unwrap()
            .size;
        let started = Instant::now();
        let outcome = install(paths, catalog, transport, &name, &version, now)
            .unwrap_or_else(|error| panic!("installing {name} {version} failed: {error}"));
        let took = started.elapsed();
        let dir = paths.runtime_dir(&name, &version);
        assert_eq!(outcome, InstallOutcome::Installed(dir.clone()));
        eprintln!("{name} {version}: {size} bytes downloaded and installed in {took:?}");
        installed.push((name, version, dir));
    }
    installed
}

/// [`MIN_DAYS_VAR`], when set: a whole number of days.
fn min_days() -> Option<u64> {
    let value = env::var(MIN_DAYS_VAR).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{MIN_DAYS_VAR} is not a whole number of days: {value:?}")),
    )
}

/// The key id of the envelope `refresh` stored. Only an id the signature
/// was verified against reaches the cache.
fn stored_key_id(paths: &Paths) -> String {
    let bytes = fs::read(paths.catalog_file()).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    envelope["key_id"]
        .as_str()
        .expect("the envelope has a key id")
        .to_owned()
}

/// A new private folder under the system temp folder, removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn create(label: &str) -> Self {
        // The time keeps a folder left by an earlier run with the same pid
        // from being reused: creating it fails instead.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let path = env::temp_dir().join(format!("{label}-{}-{nanos}", process::id()));
        private_dir_builder()
            .create(&path)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", path.display()));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
fn private_dir_builder() -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// The user's temporary folder on Windows already admits only this user,
/// SYSTEM and Administrators.
#[cfg(windows)]
fn private_dir_builder() -> DirBuilder {
    DirBuilder::new()
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
