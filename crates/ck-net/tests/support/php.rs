//! Runs an installed PHP the way the relocation contract
//! (`build/php/README.md`) says the app does, and the CI guard every test
//! that runs a downloaded PHP calls first.
//!
//! Shared by `#[path]`: `php_archive.rs` and `published.rs`, on Unix only.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ck_release::key::CI_VARIABLES;

/// Fails unless one of [`CI_VARIABLES`] is set, to any value: a test that
/// runs a downloaded PHP never does so on a developer machine, even under
/// `--ignored`. Call it before anything else.
pub fn require_ci() {
    assert!(
        CI_VARIABLES.iter().any(|name| env::var_os(name).is_some()),
        "this test runs a downloaded PHP, so it runs in CI only (where {} is set)",
        CI_VARIABLES.join(" or ")
    );
}

/// Checks the PHP installed in `root`: `php -v` names `patch`, `ext/` holds
/// shared extensions, `smoke` (an absolute path, when given) passes with
/// every one of them loaded, and each loads on its own.
pub fn check_installed(root: &Path, patch: &str, smoke: Option<&OsStr>) {
    let php = Php {
        root: root.to_path_buf(),
    };
    let version = php.run(&[OsString::from("-v")]);
    assert_clean(&version, "php -v");
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(stdout.contains(&format!("PHP {patch} ")), "{stdout}");

    let extensions = php.shared_extensions();
    assert!(!extensions.is_empty(), "ext/ holds no shared extensions");

    // The smoke script runs with every shared extension loaded. PHP runs
    // from its own folder, so a relative path would be read from there.
    if let Some(smoke) = smoke {
        assert!(
            Path::new(smoke).is_absolute(),
            "the smoke script path must be absolute: {}",
            Path::new(smoke).display()
        );
        let mut args: Vec<OsString> = extensions.iter().flat_map(|ext| php.load(ext)).collect();
        args.push(smoke.to_os_string());
        assert_clean(&php.run(&args), "the smoke script");
    }

    for ext in &extensions {
        let mut args = php.load(ext).to_vec();
        args.push("-r".into());
        args.push(format!("exit(extension_loaded({:?}) ? 0 : 1);", ext.module).into());
        assert_clean(&php.run(&args), &format!("loading {} alone", ext.name));
    }
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
    /// says the app passes, then `args`.
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
    /// built so far, and it registers as "Zend OPcache".
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
