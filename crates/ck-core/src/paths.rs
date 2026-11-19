//! Where ckm-dev keeps its data, and the debug-only test profile (D21).
//!
//! This is the only module that reads `CK_DEV_HOME` and `CK_DEV_DATA` or calls
//! `dirs`, and [`BuildKind::current`] is how runtime code asks for the build
//! kind. Resolving creates nothing.

use std::ffi::{OsStr, OsString};
use std::path::{Component, PathBuf};

use serde::{Deserialize, Serialize};

use crate::branding::{DATA_DIR_NAME, DEBUG_DATA_DIR_NAME};
use crate::error::{Error, Result};

/// Test-profile home. Debug builds only; must be absolute, with no `..`.
pub const HOME_VAR: &str = "CK_DEV_HOME";

/// Test-profile data folder. Debug builds only; needs [`HOME_VAR`]; must be
/// absolute, with no `..`.
pub const DATA_VAR: &str = "CK_DEV_DATA";

/// Whether the running binary is a debug or a release build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildKind {
    Debug,
    Release,
}

impl BuildKind {
    /// The kind of this binary. Code outside tests asks here rather than
    /// reading `cfg!(debug_assertions)` itself.
    pub const fn current() -> Self {
        if cfg!(debug_assertions) {
            Self::Debug
        } else {
            Self::Release
        }
    }
}

/// Whose folders a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// The user's normal folders: `ck-dev`, or `ck-dev-debug` in a debug build.
    Standard,
    /// A test profile under `CK_DEV_HOME` (debug builds only).
    Isolated,
}

/// The resolved folders for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The home folder: `CK_DEV_HOME` in a test profile, otherwise the user's home.
    pub home: PathBuf,
    /// The data folder (see the layout module for what is inside).
    pub data: PathBuf,
    pub profile: Profile,
}

impl Paths {
    /// Resolves the folders for this process from its environment, build kind
    /// and OS folders. Creates nothing.
    pub fn current() -> Result<Self> {
        resolve(
            &Overrides::from_env(),
            BuildKind::current(),
            &OsDirs::from_system(),
        )
    }
}

/// The `CK_DEV_*` values, as read from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub home: Option<OsString>,
    pub data: Option<OsString>,
}

impl Overrides {
    /// Reads `CK_DEV_HOME` and `CK_DEV_DATA`.
    pub fn from_env() -> Self {
        Self {
            home: std::env::var_os(HOME_VAR),
            data: std::env::var_os(DATA_VAR),
        }
    }
}

/// The OS folders that the normal layout builds on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsDirs {
    /// `dirs::home_dir()`.
    pub home: Option<PathBuf>,
    /// `dirs::data_local_dir()`: `~/.local/share` (or `$XDG_DATA_HOME`),
    /// `~/Library/Application Support`, or `%LOCALAPPDATA%`.
    pub data_local: Option<PathBuf>,
}

impl OsDirs {
    /// Asks the OS through `dirs`.
    pub fn from_system() -> Self {
        Self {
            home: dirs::home_dir(),
            data_local: dirs::data_local_dir(),
        }
    }
}

/// Resolves the folders. Pure: reads nothing from the process or the disk.
///
/// - **Release:** `env` is ignored. The data folder is
///   `<data_local>/ck-dev` and the profile is [`Profile::Standard`].
/// - **Debug, no `CK_DEV_*` set:** the same, with `ck-dev-debug`.
/// - **Debug, `CK_DEV_HOME` set:** [`Profile::Isolated`]. `home` is its value,
///   and the data folder is `CK_DEV_DATA`, or `<CK_DEV_HOME>/ck-dev` when that
///   is unset.
///
/// It fails closed, never falling back to the real folders: an empty or
/// relative value, a value with a `..` component, or `CK_DEV_DATA` without `CK_DEV_HOME`, gives
/// [`Error::BadOverride`](crate::Error::BadOverride).
pub fn resolve(env: &Overrides, build: BuildKind, os: &OsDirs) -> Result<Paths> {
    match build {
        BuildKind::Release => standard(os, DATA_DIR_NAME),
        BuildKind::Debug => match test_profile(env)? {
            Some(paths) => Ok(paths),
            None => standard(os, DEBUG_DATA_DIR_NAME),
        },
    }
}

/// The user's own folders, with `dir_name` as the data folder.
fn standard(os: &OsDirs, dir_name: &str) -> Result<Paths> {
    let home = os.home.clone().ok_or(Error::NoUserFolder("home"))?;
    let data_local = os
        .data_local
        .as_deref()
        .ok_or(Error::NoUserFolder("local data"))?;
    Ok(Paths {
        home,
        data: data_local.join(dir_name),
        profile: Profile::Standard,
    })
}

/// The test profile that `env` asks for, or `None` when neither variable is
/// set. It never looks at the OS folders.
fn test_profile(env: &Overrides) -> Result<Option<Paths>> {
    let Some(home) = &env.home else {
        return match env.data {
            Some(_) => Err(Error::BadOverride {
                var: DATA_VAR,
                reason: "is set without CK_DEV_HOME",
            }),
            None => Ok(None),
        };
    };
    let home = absolute(HOME_VAR, home)?;
    let data = match &env.data {
        Some(data) => absolute(DATA_VAR, data)?,
        None => home.join(DATA_DIR_NAME),
    };
    Ok(Some(Paths {
        home,
        data,
        profile: Profile::Isolated,
    }))
}

/// `value` as a path, when it is a non-empty absolute path with no `..`, which
/// could lead a test profile out of its folder.
fn absolute(var: &'static str, value: &OsStr) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(Error::BadOverride {
            var,
            reason: "is empty",
        });
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(Error::BadOverride {
            var,
            reason: "is not an absolute path",
        });
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(Error::BadOverride {
            var,
            reason: "contains a `..` component",
        });
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// An absolute path on every OS (`/x` is not absolute on Windows).
    fn absolute_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn os() -> OsDirs {
        OsDirs {
            home: Some(absolute_path("user")),
            data_local: Some(absolute_path("user-data")),
        }
    }

    fn overrides(home: Option<&Path>, data: Option<&Path>) -> Overrides {
        Overrides {
            home: home.map(|path| path.as_os_str().to_owned()),
            data: data.map(|path| path.as_os_str().to_owned()),
        }
    }

    #[test]
    fn release_builds_ignore_the_test_profile_variables() {
        let profile = absolute_path("profile");
        let profile_data = absolute_path("profile-data");
        let ignored = [
            overrides(Some(profile.as_path()), Some(profile_data.as_path())),
            overrides(Some(Path::new("relative")), Some(Path::new(""))),
            overrides(None, Some(profile_data.as_path())),
            overrides(Some(Path::new("")), None),
        ];
        for env in ignored {
            let paths = resolve(&env, BuildKind::Release, &os()).unwrap();
            assert_eq!(
                paths,
                Paths {
                    home: absolute_path("user"),
                    data: absolute_path("user-data").join(DATA_DIR_NAME),
                    profile: Profile::Standard,
                },
                "release with {env:?}"
            );
        }
    }

    #[test]
    fn debug_builds_honour_the_test_profile() {
        let plain = resolve(&Overrides::default(), BuildKind::Debug, &os()).unwrap();
        assert_eq!(
            plain,
            Paths {
                home: absolute_path("user"),
                data: absolute_path("user-data").join(DEBUG_DATA_DIR_NAME),
                profile: Profile::Standard,
            }
        );

        // An isolated profile never needs the OS folders.
        let no_os = OsDirs::default();
        let home = absolute_path("profile");
        let home_only = resolve(
            &overrides(Some(home.as_path()), None),
            BuildKind::Debug,
            &no_os,
        )
        .unwrap();
        assert_eq!(
            home_only,
            Paths {
                home: home.clone(),
                data: home.join(DATA_DIR_NAME),
                profile: Profile::Isolated,
            }
        );

        let data = absolute_path("profile-data");
        let both = resolve(
            &overrides(Some(home.as_path()), Some(data.as_path())),
            BuildKind::Debug,
            &no_os,
        )
        .unwrap();
        assert_eq!(
            both,
            Paths {
                home,
                data,
                profile: Profile::Isolated,
            }
        );
    }

    #[test]
    fn debug_builds_fail_closed_on_bad_values() {
        let home = absolute_path("profile");
        let data = absolute_path("profile-data");
        let home_up = home.join("..").join("escape");
        let data_up = data.join("..").join("escape");
        let cases = [
            (overrides(None, Some(data.as_path())), DATA_VAR),
            (overrides(Some(Path::new("relative/home")), None), HOME_VAR),
            (
                overrides(Some(home.as_path()), Some(Path::new("relative/data"))),
                DATA_VAR,
            ),
            (overrides(Some(Path::new("")), None), HOME_VAR),
            (
                overrides(Some(home.as_path()), Some(Path::new(""))),
                DATA_VAR,
            ),
            (overrides(Some(home_up.as_path()), None), HOME_VAR),
            (
                overrides(Some(home.as_path()), Some(data_up.as_path())),
                DATA_VAR,
            ),
        ];
        for (env, bad_var) in cases {
            match resolve(&env, BuildKind::Debug, &os()) {
                Err(Error::BadOverride { var, .. }) => assert_eq!(var, bad_var, "{env:?}"),
                other => panic!("{env:?} should fail on {bad_var}, got {other:?}"),
            }
        }
    }
}
