//! T3 and T4: an archive packed by ck-release, listed in a payload built by
//! ck-release and signed with a throwaway key, is refreshed and installed
//! through ck-core over loopback, in an isolated profile.
//!
//! - T3 packs a small fixture tree. Nothing installed is run.
//! - T4 (`#[ignore]`, CI only) installs a real PHP build, named by
//!   `CK_PHP_ARCHIVE` and `CK_PHP_DESCRIPTOR`, into a path with a space, then
//!   runs it from there with a cleared environment.
//!
//! Debug builds on Linux and macOS only: the policy of a release build
//! refuses plain HTTP to loopback, and `pack` runs on Unix only.
#![cfg(all(debug_assertions, unix))]

#[path = "support/server.rs"]
mod server;
#[path = "../../ck-core/tests/support/signing.rs"]
mod signing;

use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output};
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
    let descriptors = root.path().join("descriptors");
    fs::create_dir(&out).unwrap();
    fs::create_dir(&descriptors).unwrap();
    let archive = out.join(asset_file_name(&spec.name, &spec.version, spec.platform));
    pack(&tree, &archive, &spec)
        .unwrap()
        .write(&descriptors.join("php.json"))
        .unwrap();

    let (dir, _) = install_packed(root.path(), &archive, &descriptors);
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
    let archive = required_path("CK_PHP_ARCHIVE");
    let descriptor = Descriptor::read(&required_path("CK_PHP_DESCRIPTOR")).unwrap();
    assert_eq!(
        Some(descriptor.platform()),
        Platform::current(),
        "the archive is for another computer"
    );
    let root = TempRoot::create("ck net-php");
    let descriptors = root.path().join("descriptors");
    fs::create_dir(&descriptors).unwrap();
    descriptor.write(&descriptors.join("php.json")).unwrap();

    let (dir, took) = install_packed(root.path(), &archive, &descriptors);
    eprintln!(
        "installing {} {} ({} bytes) took {took:?}",
        descriptor.name, descriptor.version, descriptor.size
    );
    assert!(dir.to_string_lossy().contains(' '), "{}", dir.display());

    let php = Php { root: dir };
    let patch = descriptor.version.as_str().split('-').next().unwrap();
    let version = php.run(&[OsString::from("-v")]);
    assert_clean(&version, "php -v");
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(stdout.contains(&format!("PHP {patch} ")), "{stdout}");

    let extensions = php.shared_extensions();
    assert!(!extensions.is_empty(), "ext/ holds no shared extensions");

    // The smoke script runs with every shared extension loaded.
    if let Some(smoke) = env::var_os("CK_PHP_SMOKE") {
        let mut args: Vec<OsString> = extensions.iter().flat_map(|ext| php.load(ext)).collect();
        args.push(smoke);
        assert_clean(&php.run(&args), "the smoke script");
    }

    for ext in &extensions {
        let mut args = php.load(ext).to_vec();
        args.push("-r".into());
        args.push(format!("exit(extension_loaded({:?}) ? 0 : 1);", ext.module).into());
        assert_clean(&php.run(&args), &format!("loading {} alone", ext.name));
    }
}

/// Builds a payload from the one descriptor in `descriptors` with a loopback
/// base URL, signs the bytes `payload::write` wrote, serves the catalog and
/// `archive`, then refreshes and installs in an isolated profile under
/// `root`. Returns the version's folder and how long the install took.
fn install_packed(root: &Path, archive: &Path, descriptors: &Path) -> (PathBuf, Duration) {
    let paths = Paths {
        home: root.to_path_buf(),
        data: root.join("data"),
        profile: Profile::Isolated,
    };
    layout::ensure(&paths).unwrap();
    // `refresh`, `install` and the payload's URL check share this policy.
    let policy = TransportPolicy::current(paths.profile);

    let descriptors = payload::read_descriptors(descriptors).unwrap();
    let [descriptor] = descriptors.as_slice() else {
        panic!("expected one descriptor, found {}", descriptors.len());
    };
    let server = TestServer::start();
    let now = SystemTime::now();
    let expires = (now + VALID_FOR)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let base_url = server.url("");
    let spec = PayloadSpec {
        base_url: &base_url,
        sequence: 1,
        expires,
    };
    let payload_path = root.join("payload.json");
    payload::write(
        &payload::build(&descriptors, &spec, &policy).unwrap(),
        &payload_path,
    )
    .unwrap();
    let key = TestKey::generate();
    let envelope = key.sign(SIGNING_CONTEXT, &fs::read(&payload_path).unwrap());
    server.reply("/catalog-v1.json", Reply::ok(envelope.to_bytes()));
    server.reply(
        &format!("/{}", descriptor.file),
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

/// One shared extension in `ext/`.
struct Extension {
    /// The file name without `.so`.
    name: String,
    /// The name `extension_loaded` knows it by.
    module: String,
    path: PathBuf,
    zend: bool,
}

/// The installed PHP, run only from its folder in the test profile.
struct Php {
    root: PathBuf,
}

impl Php {
    /// `bin/php` with an empty environment except a fixed `PATH`, from its
    /// own folder, with no ini file and the settings the relocation contract
    /// (`build/php/README.md`) says the app passes, then `args`.
    fn run(&self, args: &[OsString]) -> Output {
        let cert = self.root.join("etc").join("ssl").join("cert.pem");
        Command::new(self.root.join("bin").join("php"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.root)
            .arg("-n")
            .arg("-d")
            .arg(setting("extension_dir", &self.root.join("ext")))
            .arg("-d")
            .arg(setting("openssl.cafile", &cert))
            .arg("-d")
            .arg(setting("curl.cainfo", &cert))
            .args(["-d", "include_path=."])
            .args(args)
            .output()
            .unwrap()
    }

    /// Every `ext/*.so`, sorted by name. opcache is the only Zend extension
    /// this round builds, and it registers as "Zend OPcache".
    fn shared_extensions(&self) -> Vec<Extension> {
        let mut extensions: Vec<Extension> = fs::read_dir(self.root.join("ext"))
            .unwrap()
            .map(|item| item.unwrap().path())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.strip_suffix(".so")?.to_owned();
                let zend = name == "opcache";
                let module = if zend {
                    "Zend OPcache".to_owned()
                } else {
                    name.clone()
                };
                Some(Extension {
                    name,
                    module,
                    path,
                    zend,
                })
            })
            .collect();
        extensions.sort_by(|a, b| a.name.cmp(&b.name));
        extensions
    }

    /// The `-d` arguments that load `ext`: a Zend extension by its path.
    fn load(&self, ext: &Extension) -> [OsString; 2] {
        let setting = if ext.zend {
            setting("zend_extension", &ext.path)
        } else {
            OsString::from(format!("extension={}", ext.name))
        };
        ["-d".into(), setting]
    }
}

/// `<key>=<path>` for `-d`. PHP quotes a value that starts with `/`, so a
/// path with a space stays one value.
fn setting(key: &str, path: &Path) -> OsString {
    let mut setting = OsString::from(format!("{key}="));
    setting.push(path);
    setting
}

/// The run exited 0 and printed no startup error, such as a library or an
/// extension that failed to load.
fn assert_clean(output: &Output, what: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && !format!("{stdout}{stderr}").contains("PHP Startup"),
        "{what}: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
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
