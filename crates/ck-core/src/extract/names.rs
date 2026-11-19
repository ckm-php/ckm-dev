//! The rules for archive entry names, the same on every OS.
//!
//! Applied to an entry's effective name (after PAX and GNU long names):
//!
//! - valid UTF-8, with no NUL or other control character;
//! - no backslash, and none of `:<>"|?*`;
//! - relative, with no empty, `.` or `..` component; a leading `./` is
//!   dropped, and a bare `./` entry is skipped;
//! - no component that is a Windows reserved name by its stem
//!   ([`is_reserved_component`]), and none ending in a dot or a space;
//! - at most [`MAX_PATH_BYTES`] bytes and [`MAX_PATH_DEPTH`] components.
//!
//! The rule that no two entries are equal after case folding is checked
//! across the archive by the extractor.

use std::path::{Path, PathBuf};

use super::refused;
use crate::error::Result;

/// The longest entry name, in bytes.
pub const MAX_PATH_BYTES: usize = 240;

/// The most components in an entry name.
pub const MAX_PATH_DEPTH: usize = 32;

/// Characters Windows refuses in a name, or reads as a separator or a stream.
const FORBIDDEN: [char; 8] = ['\\', ':', '<', '>', '"', '|', '?', '*'];

/// An entry name that passed the rules: relative, `/`-separated, with no
/// leading `./`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryPath(String);

impl EntryPath {
    /// Checks an entry's effective name. `Ok(None)` for a bare `./`, which is
    /// skipped; [`Error::ArchiveEntryRefused`](crate::Error::ArchiveEntryRefused)
    /// when a rule is broken. A trailing `/` (a folder) is allowed and dropped.
    ///
    /// Only one leading `./` is dropped: `././x` and `.//x` are refused. The
    /// length and depth caps apply to the name after both are dropped.
    pub fn parse(raw: &[u8]) -> Result<Option<Self>> {
        let Ok(name) = std::str::from_utf8(raw) else {
            return Err(refused(raw, "is not valid UTF-8"));
        };
        if name.chars().any(char::is_control) {
            return Err(refused(raw, "has a control character"));
        }
        if name.starts_with('/') {
            return Err(refused(raw, "is an absolute path"));
        }
        if let Some(forbidden) = name.chars().find(|c| FORBIDDEN.contains(c)) {
            return Err(refused(raw, format!("has the character {forbidden:?}")));
        }
        if name == "./" {
            return Ok(None);
        }
        let name = name.strip_prefix("./").unwrap_or(name);
        let name = name.strip_suffix('/').unwrap_or(name);
        if name.len() > MAX_PATH_BYTES {
            return Err(refused(
                raw,
                format!("is longer than {MAX_PATH_BYTES} bytes"),
            ));
        }
        if name.split('/').count() > MAX_PATH_DEPTH {
            return Err(refused(
                raw,
                format!("is more than {MAX_PATH_DEPTH} levels deep"),
            ));
        }
        for component in name.split('/') {
            let reason = match component {
                "" => "has an empty component",
                "." => "has a \".\" component",
                ".." => "has a \"..\" component",
                _ if is_reserved_component(component) => {
                    "has a component that is a Windows device name"
                }
                _ if component.ends_with(['.', ' ']) => {
                    "has a component that ends in a dot or a space"
                }
                _ => continue,
            };
            return Err(refused(raw, reason));
        }
        Ok(Some(Self(name.to_owned())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// `root` joined with each component.
    pub fn to_path(&self, root: &Path) -> PathBuf {
        self.components()
            .fold(root.to_path_buf(), |path, component| path.join(component))
    }
}

/// Whether `component` is a Windows reserved name by its stem, ignoring case:
/// `CON`, `nul.txt`, `Lpt1`, `aux.`, `com¹.log`, `CONIN$`. The stem is the
/// part before the first dot, without trailing spaces. Refused on every OS,
/// so archives and runtime folders work on Windows too.
pub fn is_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    match stem.as_str() {
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" => true,
        _ => ["com", "lpt"].iter().any(|device| {
            stem.strip_prefix(device).is_some_and(|number| {
                let mut chars = number.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '¹' | '²' | '³'), None)
                )
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::service::spec::ServiceId;

    fn parsed(raw: &str) -> Option<String> {
        EntryPath::parse(raw.as_bytes())
            .unwrap()
            .map(|path| path.as_str().to_owned())
    }

    fn refusal(raw: &str) -> String {
        match EntryPath::parse(raw.as_bytes()) {
            Err(Error::ArchiveEntryRefused { entry, reason }) => {
                assert_eq!(entry, raw);
                reason
            }
            other => panic!("{raw:?} was not refused: {other:?}"),
        }
    }

    // T15
    #[test]
    fn reserved_components_agree_with_service_ids() {
        let devices = ["con", "prn", "aux", "nul"]
            .map(str::to_owned)
            .into_iter()
            .chain((0..10).flat_map(|n| [format!("com{n}"), format!("lpt{n}")]));
        for device in devices {
            assert!(is_reserved_component(&device), "{device}");
            assert!(ServiceId::new(device.as_str()).is_err(), "{device}");
        }
        for name in [
            "com", "lpt", "com10", "lpt10", "conx", "nul0", "auxx", "php",
        ] {
            assert!(!is_reserved_component(name), "{name}");
            assert!(ServiceId::new(name).is_ok(), "{name}");
        }
        for name in ["CON.txt", "Lpt1", "aux.", "nul.tar.gz", "COM¹", "CONIN$"] {
            assert!(is_reserved_component(name), "{name}");
            let reason = refusal(&format!("dir/{name}"));
            assert!(reason.contains("Windows device name"), "{name}: {reason}");
        }
    }

    #[test]
    fn one_leading_dot_slash_and_a_trailing_slash_are_dropped() {
        assert_eq!(parsed("./"), None);
        assert_eq!(parsed("./bin/tool").as_deref(), Some("bin/tool"));
        assert_eq!(parsed("lib/").as_deref(), Some("lib"));
        assert!(refusal("././x").contains("\".\" component"));
        assert!(refusal(".//x").contains("empty component"));
        assert!(refusal("lib//").contains("empty component"));
        assert!(refusal(".").contains("\".\" component"));
    }

    #[test]
    fn length_and_depth_are_capped() {
        let longest = "a".repeat(MAX_PATH_BYTES);
        assert_eq!(parsed(&format!("./{longest}/")), Some(longest.clone()));
        assert!(refusal(&format!("{longest}b")).contains("longer than 240 bytes"));

        let deepest = ["d"; MAX_PATH_DEPTH].join("/");
        assert_eq!(parsed(&deepest), Some(deepest.clone()));
        assert!(refusal(&format!("{deepest}/d")).contains("more than 32 levels"));
    }
}
