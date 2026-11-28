//! T17: a signed catalog and a tar.gz, served over loopback, are refreshed
//! and installed through [`HttpTransport`] in an isolated profile; then an
//! older catalog is refused and the cached one stays in use.
//!
//! Debug builds only: the policy of a release build refuses plain HTTP to
//! loopback. Nothing installed is ever run; the "program" in the archive is
//! inert bytes.
#![cfg(debug_assertions)]

#[path = "support/server.rs"]
mod server;
#[path = "../../ck-core/tests/support/signing.rs"]
mod signing;

use std::env;
use std::fs::{self, DirBuilder};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ck_core::Error;
use ck_core::catalog::schema::{Platform, RuntimeName, RuntimeVersion};
use ck_core::catalog::trust::{SIGNING_CONTEXT, Trust};
use ck_core::catalog::{cached, refresh};
use ck_core::download::policy::TransportPolicy;
use ck_core::install::{InstallOutcome, InstalledRuntime, install};
use ck_core::layout;
use ck_core::paths::{Paths, Profile};
use ck_core::runtime::RuntimeService;
use ck_net::HttpTransport;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;

use server::{Reply, TestServer};
use signing::TestKey;

/// The time every check runs at, in Unix seconds; the catalog expires 90
/// days later.
const NOW: u64 = 1_800_000_000;
const EXPIRES: u64 = NOW + 90 * 24 * 60 * 60;

/// The archive's files: the path inside it, the bytes, and the mode.
const FILES: [(&[&str], &[u8], u32); 2] = [
    (&["bin", "demo"], b"inert bytes, never run\n", 0o755),
    (&["share", "readme.txt"], b"demo 1.2.3\n", 0o644),
];

#[test]
fn a_catalog_and_an_archive_served_over_loopback_install_and_an_older_catalog_is_refused() {
    let root = TempRoot::create();
    let paths = Paths {
        home: root.path().to_path_buf(),
        data: root.path().join("data"),
        profile: Profile::Isolated,
    };
    layout::ensure(&paths).unwrap();

    let server = TestServer::start();
    let catalog_url = server.url("/catalog-v1.json");
    let archive = demo_archive();
    let platform = Platform::current().expect("a v1 computer");
    let runtimes = json!([signing::runtime(
        "demo",
        "1.2.3",
        json!([signing::artifact(
            platform.os.as_str(),
            platform.arch.as_str(),
            &server.url("/demo-1.2.3.tar.gz"),
            "tar.gz",
            &archive,
        )]),
    )]);
    let key = TestKey::generate();
    let catalog_at = |sequence| {
        key.sign(
            SIGNING_CONTEXT,
            &signing::payload(sequence, EXPIRES, runtimes.clone()),
        )
        .to_bytes()
    };
    server.reply("/catalog-v1.json", Reply::ok(catalog_at(7)));
    server.reply("/demo-1.2.3.tar.gz", Reply::ok(archive));

    let trust = Trust::new(vec![key.verifying_key()], 0).unwrap();
    // `refresh` and `install` choose the same policy for this profile.
    let transport = HttpTransport::new(&TransportPolicy::current(paths.profile));
    let now = UNIX_EPOCH + Duration::from_secs(NOW);

    let catalog = refresh(&paths, &trust, &transport, &catalog_url, now).unwrap();
    assert_eq!(catalog.sequence(), 7);

    let name = RuntimeName::new("demo").unwrap();
    let version = RuntimeVersion::new("1.2.3").unwrap();
    let dir = paths.runtime_dir(&name, &version);
    assert_eq!(
        install(&paths, &catalog, &transport, &name, &version, now).unwrap(),
        InstallOutcome::Installed(dir.clone())
    );
    for (parts, contents, mode) in FILES {
        let path = parts.iter().fold(dir.clone(), |path, part| path.join(part));
        assert_eq!(fs::read(&path).unwrap(), contents, "{}", path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(actual, mode, "{}", path.display());
        }
        // Windows keeps no Unix modes; the folder's DACL applies.
        #[cfg(not(unix))]
        let _ = mode;
    }
    assert_eq!(
        server.request_lines(),
        [
            "GET /catalog-v1.json HTTP/1.1",
            "GET /demo-1.2.3.tar.gz HTTP/1.1",
        ]
    );

    // An older catalog at the same URL is refused, and the cached one stays
    // in use: installing from it again fetches nothing.
    server.reply("/catalog-v1.json", Reply::ok(catalog_at(6)));
    let result = refresh(&paths, &trust, &transport, &catalog_url, now);
    assert!(
        matches!(
            result,
            Err(Error::CatalogOutdated {
                offered: 6,
                current: 7
            })
        ),
        "{result:?}"
    );
    let kept = cached(&paths, &trust).unwrap().expect("the cached catalog");
    assert_eq!(kept.sequence(), 7);
    assert_eq!(
        install(&paths, &kept, &transport, &name, &version, now).unwrap(),
        InstallOutcome::AlreadyPresent(dir)
    );
    assert_eq!(
        server.request_lines(),
        [
            "GET /catalog-v1.json HTTP/1.1",
            "GET /demo-1.2.3.tar.gz HTTP/1.1",
            "GET /catalog-v1.json HTTP/1.1",
        ]
    );
}

#[test]
fn the_shared_runtime_service_installs_and_lists_a_version_from_a_signed_catalog() {
    let root = TempRoot::create();
    let paths = Paths {
        home: root.path().to_path_buf(),
        data: root.path().join("data"),
        profile: Profile::Isolated,
    };
    let server = TestServer::start();
    let catalog_url = server.url("/catalog-v1.json");
    let archive = demo_archive();
    let platform = Platform::current().expect("a v1 computer");
    let runtimes = json!([signing::runtime(
        "demo",
        "1.2.3",
        json!([signing::artifact(
            platform.os.as_str(),
            platform.arch.as_str(),
            &server.url("/demo-1.2.3.tar.gz"),
            "tar.gz",
            &archive,
        )]),
    )]);
    let key = TestKey::generate();
    let catalog_at = |sequence| {
        key.sign(
            SIGNING_CONTEXT,
            &signing::payload(sequence, EXPIRES, runtimes.clone()),
        )
        .to_bytes()
    };
    server.reply("/catalog-v1.json", Reply::ok(catalog_at(7)));
    server.reply("/demo-1.2.3.tar.gz", Reply::ok(archive));

    let trust = Trust::new(vec![key.verifying_key()], 0).unwrap();
    let transport = HttpTransport::new(&TransportPolicy::current(paths.profile));
    let now = UNIX_EPOCH + Duration::from_secs(NOW);
    let service = RuntimeService::new(&paths, &trust, &transport, &catalog_url).unwrap();

    let name = RuntimeName::new("demo").unwrap();
    let version = RuntimeVersion::new("1.2.3").unwrap();
    let dir = paths.runtime_dir(&name, &version);
    assert_eq!(
        service.install("demo", "1.2.3", || now).unwrap(),
        InstallOutcome::Installed(dir.clone())
    );
    assert_eq!(
        fs::read(dir.join("share/readme.txt")).unwrap(),
        b"demo 1.2.3\n"
    );
    assert_eq!(
        service.installed().unwrap(),
        vec![InstalledRuntime { name, version }]
    );
    assert_eq!(
        server.request_lines(),
        [
            "GET /catalog-v1.json HTTP/1.1",
            "GET /demo-1.2.3.tar.gz HTTP/1.1",
        ]
    );

    // An installed version returns before a catalog request, even when the
    // server now offers an older one.
    server.reply("/catalog-v1.json", Reply::ok(catalog_at(6)));
    assert_eq!(
        service.install("demo", "1.2.3", || now).unwrap(),
        InstallOutcome::AlreadyPresent(dir)
    );
    assert_eq!(
        server.request_lines(),
        [
            "GET /catalog-v1.json HTTP/1.1",
            "GET /demo-1.2.3.tar.gz HTTP/1.1",
        ]
    );
}

/// [`FILES`] as a tar.gz, with no folder entries: the folders are implied.
fn demo_archive() -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (parts, contents, mode) in FILES {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(mode);
        header.set_size(contents.len() as u64);
        builder
            .append_data(&mut header, parts.join("/"), contents)
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// A new private folder under the system temp folder, removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn create() -> Self {
        // The time keeps a folder left by an earlier run with the same pid
        // from being reused: creating it fails instead.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let path = env::temp_dir().join(format!("ck-net-{}-{nanos}", process::id()));
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
