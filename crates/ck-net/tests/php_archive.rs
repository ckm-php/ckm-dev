//! T3 and T4: an archive packed by ck-release, listed in a payload built by
//! ck-release and signed with a throwaway key, is refreshed and installed
//! through ck-core over loopback, in an isolated profile.
//!
//! - T3 packs a small fixture tree. Nothing installed is run.
//! - T4 (`#[ignore]`, CI only) installs a real PHP build, named by
//!   `CK_PHP_ARCHIVE` and `CK_PHP_DESCRIPTOR`, into a path with a space, then
//!   runs it from there with a cleared environment (`support/php.rs`). It
//!   refuses to run unless `CI` or `GITHUB_ACTIONS` is set.
//!
//! Debug builds on Linux and macOS only: the policy of a release build
//! refuses plain HTTP to loopback, and `pack` runs on Unix only.
#![cfg(all(debug_assertions, unix))]

#[path = "support/php.rs"]
mod php;
#[path = "support/server.rs"]
mod server;
#[path = "../../ck-core/tests/support/signing.rs"]
mod signing;

use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ck_core::catalog::refresh;
use ck_core::catalog::schema::{Platform, RuntimeName, RuntimeVersion};
use ck_core::catalog::trust::{SIGNING_CONTEXT, Trust};
use ck_core::download::policy::TransportPolicy;
use ck_core::install::{InstallOutcome, install};
use ck_core::layout;
use ck_core::paths::{Paths, Profile};
use ck_net::HttpTransport;
use ck_release::pack::{PackSpec, pack};
use ck_release::payload::{self, PayloadSpec};
use ck_release::{Descriptor, asset_file_name};

use server::{Reply, TestServer};
use signing::TestKey;

/// How long the signed catalog stays valid.
const VALID_FOR: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// The fixture tree: path, bytes, the mode it is created with, and the mode
/// it must land with. The "programs" are inert bytes, never run.
const FILES: [(&str, &[u8], u32, u32); 5] = [
    ("bin/php", b"#!/bin/sh\nexit 1\n", 0o700, 0o755),
    (
        "ck-runtime.json",
        b"{\"version\": \"8.5.12-ck1\"}\n",
        0o600,
        0o644,
    ),
    ("etc/ssl/cert.pem", b"not a real bundle\n", 0o664, 0o644),
    ("ext/opcache.so", b"inert bytes\n", 0o755, 0o755),
    ("lib/libz.so.1", b"inert bytes\n", 0o644, 0o644),
];

/// Every folder the fixture has; `licenses` is empty.
const FOLDERS: [&str; 6] = ["bin", "etc", "etc/ssl", "ext", "lib", "licenses"];

// T3
#[test]
fn a_packed_tree_installs_through_a_signed_catalog_with_normal_modes() {
    let root = TempRoot::create("ck-net-php");
    let tree = root.path().join("tree");
    for (name, contents, mode, _) in FILES {
        let path = tree.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    }
    let licenses = tree.join("licenses");
    fs::create_dir(&licenses).unwrap();
    fs::set_permissions(&licenses, Permissions::from_mode(0o700)).unwrap();

    let spec = PackSpec {
        name: RuntimeName::new("php").unwrap(),
        version: RuntimeVersion::new("8.5.12-ck1").unwrap(),
        platform: Platform::current().expect("a v1 computer"),
        license: "PHP-3.01".to_owned(),
    };
    let out = root.path().join("out");
    fs::create_dir(&out).unwrap();
    let archive = out.join(asset_file_name(&spec.name, &spec.version, spec.platform));
    let descriptor = pack(&tree, &archive, &spec).unwrap();

    let (dir, _) = install_packed(root.path(), &archive, &descriptor);
    for (name, contents, _, mode) in FILES {
        let path = dir.join(name);
        assert_eq!(fs::read(&path).unwrap(), contents, "{}", path.display());
        assert_eq!(mode_of(&path), mode, "{}", path.display());
    }
    for folder in FOLDERS {
        let path = dir.join(folder);
        assert!(path.is_dir(), "{}", path.display());
        assert_eq!(mode_of(&path), 0o755, "{}", path.display());
    }
}

// T4
#[test]
#[ignore = "CI only: needs a real PHP archive in CK_PHP_ARCHIVE and CK_PHP_DESCRIPTOR"]
fn a_real_php_archive_installs_into_a_path_with_a_space_and_runs_there() {
    // First of all, so `--ignored` never runs a downloaded PHP on a
    // developer machine.
    php::require_ci();
    let archive = required_path("CK_PHP_ARCHIVE");
    let descriptor = Descriptor::read(&required_path("CK_PHP_DESCRIPTOR")).unwrap();
    assert_eq!(
        Some(descriptor.platform()),
        Platform::current(),
        "the archive is for another computer"
    );
    let root = TempRoot::create("ck net-php");

    let (dir, took) = install_packed(root.path(), &archive, &descriptor);
    eprintln!(
        "installing {} {} ({} bytes) took {took:?}",
        descriptor.name, descriptor.version, descriptor.size
    );
    assert!(dir.to_string_lossy().contains(' '), "{}", dir.display());

    let patch = descriptor.version.as_str().split('-').next().unwrap();
    php::check_installed(&dir, patch, env::var_os("CK_PHP_SMOKE").as_deref());
}

/// Builds a payload from `descriptor` under a loopback release prefix, signs
/// the bytes `payload::write` wrote, serves the catalog and `archive`, then
/// refreshes and installs in an isolated profile under `root`. Returns the
/// version's folder and how long the install took.
fn install_packed(root: &Path, archive: &Path, descriptor: &Descriptor) -> (PathBuf, Duration) {
    let paths = Paths {
        home: root.to_path_buf(),
        data: root.join("data"),
        profile: Profile::Isolated,
    };
    layout::ensure(&paths).unwrap();
    // `refresh`, `install` and the payload's URL check share this policy.
    let policy = TransportPolicy::current(paths.profile);

    let server = TestServer::start();
    let now = SystemTime::now();
    let expires = (now + VALID_FOR)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let prefix = server.url("/releases/download/");
    let spec = PayloadSpec {
        prefix: &prefix,
        sequence: 1,
        expires,
    };
    let payload_path = root.join("payload.json");
    payload::write(
        &payload::assemble(std::slice::from_ref(descriptor), &spec, &policy).unwrap(),
        &payload_path,
    )
    .unwrap();
    let key = TestKey::generate();
    let envelope = key.sign(SIGNING_CONTEXT, &fs::read(&payload_path).unwrap());
    server.reply("/catalog-v1.json", Reply::ok(envelope.to_bytes()));
    server.reply(
        &format!(
            "/releases/download/{}-{}/{}",
            descriptor.name, descriptor.version, descriptor.file
        ),
        Reply::ok(fs::read(archive).unwrap()),
    );

    let trust = Trust::new(vec![key.verifying_key()], 0).unwrap();
    let transport = HttpTransport::new(&policy);
    let catalog = refresh(
        &paths,
        &trust,
        &transport,
        &server.url("/catalog-v1.json"),
        now,
    )
    .unwrap();
    let dir = paths.runtime_dir(&descriptor.name, &descriptor.version);
    let started = Instant::now();
    let outcome = install(
        &paths,
        &catalog,
        &transport,
        &descriptor.name,
        &descriptor.version,
        now,
    )
    .unwrap();
    let took = started.elapsed();
    assert_eq!(outcome, InstallOutcome::Installed(dir.clone()));
    (dir, took)
}

fn required_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {name}; this test runs in CI after the PHP build"))
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// A new private folder under the system temp folder, removed on drop. A
/// copy of the one in `catalog_install.rs`, with a label so T4 can put a
/// space in the path.
struct TempRoot(PathBuf);

impl TempRoot {
    fn create(label: &str) -> Self {
        // The time keeps a folder left by an earlier run with the same pid
        // from being reused: creating it fails instead.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let path = env::temp_dir().join(format!("{label}-{}-{nanos}", process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", path.display()));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
