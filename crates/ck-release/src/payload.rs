//! Turns descriptors into an unsigned catalog payload (`ck-core`'s schema 1).
//!
//! The payload groups descriptors by name and version into one runtime each,
//! with one artifact per OS and CPU. Each artifact URL is
//! `<base url>/<descriptor file>`. Before it returns, [`build`] parses its
//! own output again with `ck_core::catalog::schema::parse` and checks every
//! URL with the given transport policy; the CLI always passes the release
//! rules (`TransportPolicy::current(Profile::Standard)`).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ck_core::catalog::schema::{self, Artifact, Payload, Runtime, RuntimeName, RuntimeVersion};
use ck_core::download::policy::TransportPolicy;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::{Descriptor, Error, Result, lock, to_pretty_json};

/// The inputs besides the descriptors.
#[derive(Debug, Clone)]
pub struct PayloadSpec<'a> {
    /// The release download folder, with no trailing `/`, for example
    /// `https://github.com/<owner>/<repo>/releases/download/<tag>`.
    pub base_url: &'a str,
    /// Higher than every catalog published before.
    pub sequence: u64,
    /// Unix seconds.
    pub expires: u64,
}

/// Reads every `*.json` file in `dir` as a [`Descriptor`], sorted by file
/// name. Any other entry in `dir` is an error.
pub fn read_descriptors(dir: &Path) -> Result<Vec<Descriptor>> {
    let mut paths = Vec::new();
    for item in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let item = item.map_err(|e| Error::io("list", dir, e))?;
        let path = item.path();
        // `file_type` does not follow links, so a link is refused too.
        let is_file = item
            .file_type()
            .map_err(|e| Error::io("read", &path, e))?
            .is_file();
        if !is_file || path.extension() != Some("json".as_ref()) {
            return Err(Error::Invalid(format!(
                "{} is not a descriptor (a *.json file)",
                path.display()
            )));
        }
        paths.push(path);
    }
    if paths.is_empty() {
        return Err(Error::Invalid(format!(
            "{} holds no descriptors",
            dir.display()
        )));
    }
    paths.sort();
    paths
        .iter()
        .map(PathBuf::as_path)
        .map(Descriptor::read)
        .collect()
}

/// Builds and checks the payload.
///
/// Refuses no descriptors, an expiry that is not in the future, two
/// descriptors for one name, version (ignoring case, as `ck-core` compares
/// them), OS and CPU, one name and version with two licences, and any URL
/// that is not `<base url>/<file>` in normal form with no query or fragment,
/// or that `policy` refuses as a first hop. Runtimes are sorted by name and
/// version, and artifacts by OS and CPU.
pub fn build(
    descriptors: &[Descriptor],
    spec: &PayloadSpec<'_>,
    policy: &TransportPolicy,
) -> Result<Payload> {
    if descriptors.is_empty() {
        return Err(Error::Invalid("there are no descriptors".to_owned()));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if spec.expires <= now {
        return Err(Error::Invalid(format!(
            "the expiry {} is not in the future",
            spec.expires
        )));
    }

    let mut runtimes: BTreeMap<(RuntimeName, RuntimeVersion), Runtime> = BTreeMap::new();
    let mut seen = HashSet::new();
    for descriptor in descriptors {
        descriptor.validate()?;
        let platform = descriptor.platform();
        let what = format!("{} {} for {platform}", descriptor.name, descriptor.version);
        // Versions are ASCII, so ASCII lower case folds every case.
        let key = (
            descriptor.name.clone(),
            descriptor.version.as_str().to_ascii_lowercase(),
            platform,
        );
        if !seen.insert(key) {
            return Err(Error::Invalid(format!(
                "{what} has more than one descriptor (versions compare ignoring case)"
            )));
        }
        let url = artifact_url(spec.base_url, &descriptor.file, policy)?;
        let runtime = runtimes
            .entry((descriptor.name.clone(), descriptor.version.clone()))
            .or_insert_with(|| Runtime {
                name: descriptor.name.clone(),
                version: descriptor.version.clone(),
                license: descriptor.license.clone(),
                artifacts: Vec::new(),
            });
        if runtime.license != descriptor.license {
            return Err(Error::Invalid(format!(
                "{what} has the licence {:?}, but another descriptor of it has {:?}",
                descriptor.license, runtime.license
            )));
        }
        runtime.artifacts.push(Artifact {
            os: descriptor.os,
            arch: descriptor.cpu,
            url,
            sha256: descriptor.sha256,
            size: descriptor.size,
            format: descriptor.format,
        });
    }
    let runtimes = runtimes
        .into_values()
        .map(|mut runtime| {
            runtime
                .artifacts
                .sort_by_key(|artifact| (artifact.os.as_str(), artifact.arch.as_str()));
            runtime
        })
        .collect();
    let payload = Payload {
        schema: schema::SCHEMA,
        sequence: spec.sequence,
        expires: spec.expires,
        runtimes,
    };

    // What the app will read is what was built.
    let parsed = schema::parse(&to_pretty_json(&payload)?)?;
    if parsed != payload {
        return Err(Error::Invalid(
            "the payload reads back differently from how it was built".to_owned(),
        ));
    }
    Ok(payload)
}

/// `<base>/<file>`, checked.
fn artifact_url(base: &str, file: &str, policy: &TransportPolicy) -> Result<String> {
    if base.ends_with('/') {
        return Err(Error::Invalid(format!(
            "the base URL {base:?} must not end in '/'"
        )));
    }
    let text = format!("{base}/{file}");
    let url = Url::parse(&text)
        .map_err(|error| Error::Invalid(format!("the URL {text:?} does not parse: {error}")))?;
    if url.as_str() != text {
        return Err(Error::Invalid(format!(
            "the URL {text:?} must be written in its normal form, {:?}",
            url.as_str()
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::Invalid(format!(
            "the URL {text:?} must have no query or fragment"
        )));
    }
    if url.path_segments().and_then(Iterator::last) != Some(file) {
        return Err(Error::Invalid(format!(
            "the URL {text:?} must end in the file name {file:?}"
        )));
    }
    policy.check_first(&url)?;
    Ok(text)
}

/// Reads an RFC 3339 time, such as `2027-01-31T00:00:00Z`, as Unix seconds.
pub fn parse_expires(text: &str) -> Result<u64> {
    let time = OffsetDateTime::parse(text, &Rfc3339).map_err(|error| {
        Error::Invalid(format!(
            "the expiry {text:?} is not an RFC 3339 time: {error}"
        ))
    })?;
    u64::try_from(time.unix_timestamp())
        .map_err(|_| Error::Invalid(format!("the expiry {text:?} is before 1970")))
}

/// Writes `payload` to `out` as two-space JSON with a trailing newline, the
/// form the owner reviews as a diff.
pub fn write(payload: &Payload, out: &Path) -> Result<()> {
    lock::write_atomic(out, &to_pretty_json(payload)?)
}

#[cfg(test)]
mod tests {
    use ck_core::catalog::schema::{Arch, ArchiveFormat, Os, Platform, Sha256Digest};
    use ck_core::paths::Profile;

    use super::*;
    use crate::{DESCRIPTOR_SCHEMA, asset_file_name};

    const BASE: &str = "https://github.com/owner/repo/releases/download/php-build-1";

    /// 2100-01-01T00:00:00Z.
    const EXPIRES: u64 = 4_102_444_800;

    fn descriptor(version: &str, os: Os, cpu: Arch) -> Descriptor {
        let name = RuntimeName::new("php").unwrap();
        let version = RuntimeVersion::new(version).unwrap();
        let file = asset_file_name(&name, &version, Platform { os, arch: cpu });
        Descriptor {
            schema: DESCRIPTOR_SCHEMA,
            sha256: Sha256Digest::of(file.as_bytes()),
            name,
            version,
            license: "PHP-3.01".to_owned(),
            os,
            cpu,
            file,
            size: 31_457_280,
            format: ArchiveFormat::TarGz,
        }
    }

    fn spec(base_url: &str) -> PayloadSpec<'_> {
        PayloadSpec {
            base_url,
            sequence: 3,
            expires: EXPIRES,
        }
    }

    /// The release rules, which a debug build also applies outside a test
    /// profile.
    fn release() -> TransportPolicy {
        TransportPolicy::current(Profile::Standard)
    }

    // T2
    #[test]
    fn the_payload_parses_with_ck_core_and_its_urls_pass_the_release_policy() {
        let descriptors = [
            descriptor("8.5.12-ck1", Os::Macos, Arch::Aarch64),
            descriptor("7.4.33-ck1", Os::Linux, Arch::X86_64),
            descriptor("8.5.12-ck1", Os::Linux, Arch::X86_64),
        ];
        let payload = build(&descriptors, &spec(BASE), &release()).unwrap();
        assert_eq!(
            (payload.schema, payload.sequence, payload.expires),
            (schema::SCHEMA, 3, EXPIRES)
        );
        let listed: Vec<(&str, &str, &str)> = payload
            .runtimes
            .iter()
            .flat_map(|runtime| {
                runtime.artifacts.iter().map(|artifact| {
                    (
                        runtime.version.as_str(),
                        runtime.license.as_str(),
                        artifact.url.as_str(),
                    )
                })
            })
            .collect();
        let url = |file: &str| format!("{BASE}/{file}");
        assert_eq!(
            listed,
            [
                (
                    "7.4.33-ck1",
                    "PHP-3.01",
                    url("php-7.4.33-ck1-linux-x86_64.tar.gz").as_str()
                ),
                (
                    "8.5.12-ck1",
                    "PHP-3.01",
                    url("php-8.5.12-ck1-linux-x86_64.tar.gz").as_str()
                ),
                (
                    "8.5.12-ck1",
                    "PHP-3.01",
                    url("php-8.5.12-ck1-macos-aarch64.tar.gz").as_str()
                ),
            ]
        );
        assert_eq!(
            schema::parse(&to_pretty_json(&payload).unwrap()).unwrap(),
            payload
        );

        let result = build(
            &descriptors,
            &spec("https://example.com/owner/repo/releases/download/php-build-1"),
            &release(),
        );
        assert!(
            matches!(result, Err(Error::Core(ck_core::Error::UrlRefused { .. }))),
            "{result:?}"
        );
        for base in [
            "http://github.com/owner/repo/releases/download/php-build-1",
            "https://github.com.evil.com/owner/repo/releases/download/php-build-1",
            "http://127.0.0.1:8080",
            "https://github.com/owner/repo/releases/download/php-build-1/",
            "https://github.com/owner/repo/releases/download/php-build-1?x=1",
            "https://GitHub.com/owner/repo/releases/download/php-build-1",
        ] {
            let result = build(&descriptors, &spec(base), &release());
            assert!(result.is_err(), "{base}: {result:?}");
        }

        assert_eq!(
            parse_expires("2027-01-31T00:00:00Z").unwrap(),
            1_801_353_600
        );
        assert!(parse_expires("2027-01-31").is_err());
    }

    #[test]
    fn duplicates_mixed_licences_and_a_past_expiry_are_refused() {
        let linux = descriptor("8.5.12-ck1", Os::Linux, Arch::X86_64);
        let mut mit = descriptor("8.5.12-ck1", Os::Macos, Arch::Aarch64);
        mit.license = "MIT".to_owned();
        let cases = [
            (
                "the same descriptor twice",
                vec![linux.clone(), linux.clone()],
            ),
            (
                "versions that differ only in case",
                vec![
                    linux.clone(),
                    descriptor("8.5.12-CK1", Os::Linux, Arch::X86_64),
                ],
            ),
            ("two licences", vec![linux.clone(), mit]),
            ("no descriptors", Vec::new()),
        ];
        for (case, descriptors) in cases {
            let result = build(&descriptors, &spec(BASE), &release());
            assert!(result.is_err(), "{case}: {result:?}");
        }

        let past = PayloadSpec {
            expires: 1_000_000_000,
            ..spec(BASE)
        };
        assert!(build(&[linux], &past, &release()).is_err());
    }
}
