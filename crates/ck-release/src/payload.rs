//! The unsigned catalog payload (`ck-core`'s schema 1): built from the
//! downloaded release files, and read back for signing.
//!
//! `--releases <dir>` holds one folder per runtime release, named by its tag
//! `<name>-<version>` (D25), as `gh release download <tag> -D <dir>/<tag>`
//! writes it: the archives, their descriptors and `SHA256SUMS`. [`build`]
//! measures each archive's SHA256 and size, which must equal its descriptor
//! and `SHA256SUMS`, then [`assemble`]s the payload.
//!
//! [`assemble`] groups descriptors by name and version into one runtime each,
//! with one artifact per OS and CPU. Each artifact URL is [`artifact_url`]:
//! `<prefix><name>-<version>/<file>`, where the prefix ends in
//! `/releases/download/`. Before it returns, [`assemble`] parses its own
//! output again with `ck_core::catalog::schema::parse` and checks every URL
//! with the given transport policy; the CLI always passes the release rules
//! (`TransportPolicy::current(Profile::Standard)`) and
//! [`release_prefix`](crate::release_prefix).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ck_core::catalog::schema::{
    self, ArchiveFormat, Artifact, Payload, Platform, Runtime, RuntimeName, RuntimeVersion,
    Sha256Digest,
};
use ck_core::download::policy::TransportPolicy;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::{
    Descriptor, Error, Result, archive_suffix, asset_file_name, check_archive_format, lock,
    to_pretty_json,
};

/// What every artifact URL prefix ends with.
const RELEASE_DOWNLOAD: &str = "/releases/download/";

/// The inputs besides the release files.
#[derive(Debug, Clone)]
pub struct PayloadSpec<'a> {
    /// The release download folder, ending in `/releases/download/`, for
    /// example `https://github.com/ckm-php/ckm-dev/releases/download/`.
    pub prefix: &'a str,
    /// Higher than every catalog published before.
    pub sequence: u64,
    /// Unix seconds.
    pub expires: u64,
}

/// Measures the release files under `releases` (see the module notes) and
/// [`assemble`]s the payload from what was measured.
///
/// Refuses, besides what [`assemble`] refuses: a folder whose name is not
/// the tag of its descriptors, an archive whose SHA256 or size differs from
/// its descriptor or from `SHA256SUMS`, and any file or folder that is not an
/// archive, a descriptor or `SHA256SUMS`. `SHA256SUMS` lists every archive
/// once and nothing else, in `sha256sum`'s text form with LF line ends.
pub fn build(releases: &Path, spec: &PayloadSpec<'_>, policy: &TransportPolicy) -> Result<Payload> {
    let descriptors = read_releases(releases)?;
    assemble(&descriptors, spec, policy)
}

/// The descriptors found under `releases`, each with the SHA256 and size
/// measured from its archive, sorted by name, version, OS and CPU.
fn read_releases(releases: &Path) -> Result<Vec<Descriptor>> {
    let metadata = fs::symlink_metadata(releases).map_err(|e| Error::io("read", releases, e))?;
    if !metadata.is_dir() {
        return Err(Error::Invalid(format!(
            "{} is not a folder (a link to one is refused)",
            releases.display()
        )));
    }
    let mut descriptors = Vec::new();
    for (tag, folder, metadata) in entries(releases)? {
        if !metadata.is_dir() {
            return Err(Error::Invalid(format!(
                "{} is not a release folder: `--releases` holds only one folder per tag \
                 (links are refused)",
                folder.display()
            )));
        }
        descriptors.extend(read_release(&tag, &folder)?);
    }
    if descriptors.is_empty() {
        return Err(Error::Invalid(format!(
            "{} holds no release folder",
            releases.display()
        )));
    }
    descriptors.sort_by(|a, b| {
        (
            a.name.as_str(),
            a.version.as_str(),
            a.os.as_str(),
            a.cpu.as_str(),
        )
            .cmp(&(
                b.name.as_str(),
                b.version.as_str(),
                b.os.as_str(),
                b.cpu.as_str(),
            ))
    });
    Ok(descriptors)
}

/// The release file names besides the archives and their descriptors.
const SUMS: &str = "SHA256SUMS";
const TAR_GZ_SUFFIX: &str = archive_suffix(ArchiveFormat::TarGz);
const ZIP_SUFFIX: &str = archive_suffix(ArchiveFormat::Zip);
const DESCRIPTOR_SUFFIX: &str = ".json";
/// `SHA256SUMS` lists a few archives; a larger file is not one.
const MAX_SUMS_BYTES: u64 = 64 * 1024;

fn archive_stem(name: &str) -> Option<&str> {
    name.strip_suffix(TAR_GZ_SUFFIX)
        .or_else(|| name.strip_suffix(ZIP_SUFFIX))
}

/// One release folder, named by its tag: the archives, a descriptor
/// `<stem>.json` for each `<stem>.tar.gz` or `<stem>.zip`, and `SHA256SUMS`.
fn read_release(tag: &str, folder: &Path) -> Result<Vec<Descriptor>> {
    // Strip the complete suffix, including both extensions of `.tar.gz`.
    let mut archives = BTreeMap::new();
    let mut described = BTreeSet::new();
    let mut has_sums = false;
    for (name, path, metadata) in entries(folder)? {
        if !metadata.is_file() {
            return Err(Error::Invalid(format!(
                "{} is not a regular file (folders and links are refused)",
                path.display()
            )));
        }
        if name == SUMS {
            has_sums = true;
        } else if let Some(stem) = archive_stem(&name) {
            if let Some(other) = archives.get(stem) {
                return Err(Error::Invalid(format!(
                    "{}: {other:?} and {name:?} share descriptor {stem}{DESCRIPTOR_SUFFIX}",
                    folder.display()
                )));
            }
            archives.insert(stem.to_owned(), name);
        } else if let Some(stem) = name.strip_suffix(DESCRIPTOR_SUFFIX) {
            described.insert(stem.to_owned());
        } else {
            return Err(Error::Invalid(format!(
                "{} is not an archive (`{TAR_GZ_SUFFIX}` or `{ZIP_SUFFIX}`), a descriptor \
                 (`{DESCRIPTOR_SUFFIX}`) or {SUMS}",
                path.display()
            )));
        }
    }
    if archives.is_empty() {
        return Err(Error::Invalid(format!(
            "{} holds no archive",
            folder.display()
        )));
    }
    if let Some((stem, file)) = archives.iter().find(|(stem, _)| !described.contains(*stem)) {
        return Err(Error::Invalid(format!(
            "{} has no descriptor {stem}{DESCRIPTOR_SUFFIX}",
            folder.join(file).display()
        )));
    }
    if let Some(stem) = described.iter().find(|stem| !archives.contains_key(*stem)) {
        return Err(Error::Invalid(format!(
            "{} has no archive {stem}{TAR_GZ_SUFFIX} or {stem}{ZIP_SUFFIX}",
            folder.join(format!("{stem}{DESCRIPTOR_SUFFIX}")).display()
        )));
    }
    let sums_path = folder.join(SUMS);
    if !has_sums {
        return Err(Error::Invalid(format!(
            "{} is missing",
            sums_path.display()
        )));
    }

    let mut listed = BTreeMap::new();
    for (line, name, digest) in read_sums(&sums_path)? {
        let known = archive_stem(&name).and_then(|stem| archives.get(stem)) == Some(&name);
        if !known {
            return Err(Error::Invalid(format!(
                "{}: line {line} lists a file that is not an archive in this folder",
                sums_path.display()
            )));
        }
        if listed.insert(name, digest).is_some() {
            return Err(Error::Invalid(format!(
                "{}: line {line} lists an archive a second time",
                sums_path.display()
            )));
        }
    }

    let mut descriptors = Vec::new();
    for (stem, file) in archives {
        let archive = folder.join(&file);
        let descriptor_path = folder.join(format!("{stem}{DESCRIPTOR_SUFFIX}"));
        let mut descriptor = Descriptor::read(&descriptor_path)?;
        if descriptor.file != file {
            return Err(Error::Invalid(format!(
                "{}: it describes {:?}, not {file:?}",
                descriptor_path.display(),
                descriptor.file
            )));
        }
        let expected_tag = format!("{}-{}", descriptor.name, descriptor.version);
        if tag != expected_tag {
            return Err(Error::Invalid(format!(
                "{}: {} {} belongs in a folder named {expected_tag:?}, not {tag:?}",
                descriptor_path.display(),
                descriptor.name,
                descriptor.version
            )));
        }
        let Some(&summed) = listed.get(&file) else {
            return Err(Error::Invalid(format!(
                "{} does not list {file}",
                sums_path.display()
            )));
        };

        let (sha256, size) = measure(&archive)?;
        if size != descriptor.size {
            return Err(Error::Invalid(format!(
                "{}: it has {size} bytes, but its descriptor says {}",
                archive.display(),
                descriptor.size
            )));
        }
        if sha256 != descriptor.sha256 {
            return Err(Error::Invalid(format!(
                "{}: its SHA256 is {sha256}, but its descriptor says {}",
                archive.display(),
                descriptor.sha256
            )));
        }
        if sha256 != summed {
            return Err(Error::Invalid(format!(
                "{}: its SHA256 is {sha256}, but {SUMS} says {summed}",
                archive.display()
            )));
        }
        descriptor.sha256 = sha256;
        descriptor.size = size;
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

/// The lines of a `SHA256SUMS` file as `sha256sum` writes them in text mode:
/// `<64 lower-case hex>  <file>`, each ending in LF. Returns each line's
/// number, file name and digest. Errors give the line number only, never the
/// line.
fn read_sums(path: &Path) -> Result<Vec<(usize, String, Sha256Digest)>> {
    let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", path.display()));
    let size = fs::symlink_metadata(path)
        .map_err(|e| Error::io("read", path, e))?
        .len();
    if size > MAX_SUMS_BYTES {
        return Err(invalid(format!("it is larger than {MAX_SUMS_BYTES} bytes")));
    }
    let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
    let text = String::from_utf8(bytes).map_err(|_| invalid("it is not UTF-8".to_owned()))?;
    let Some(body) = text.strip_suffix('\n') else {
        return Err(invalid("it must end in a line feed".to_owned()));
    };
    let mut lines = Vec::new();
    for (index, line) in body.split('\n').enumerate() {
        let number = index + 1;
        let malformed = || {
            invalid(format!(
                "line {number} is not `<64 lower-case hex>  <file>`"
            ))
        };
        if line.contains('\r') {
            return Err(invalid(format!(
                "line {number} holds a carriage return (lines end in LF only)"
            )));
        }
        let Some((hex, name)) = line.split_once("  ") else {
            return Err(malformed());
        };
        let digest = Sha256Digest::from_hex(hex).map_err(|_| malformed())?;
        if name.is_empty() {
            return Err(malformed());
        }
        lines.push((number, name.to_owned(), digest));
    }
    Ok(lines)
}

/// The entries of `folder` with their own metadata (links are not
/// followed), sorted by name. Refuses a name that is not UTF-8.
fn entries(folder: &Path) -> Result<Vec<(String, PathBuf, fs::Metadata)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(folder).map_err(|e| Error::io("list", folder, e))? {
        let entry = entry.map_err(|e| Error::io("list", folder, e))?;
        let path = entry.path();
        let Ok(name) = entry.file_name().into_string() else {
            return Err(Error::Invalid(format!(
                "{}: the name is not UTF-8",
                path.display()
            )));
        };
        let metadata = fs::symlink_metadata(&path).map_err(|e| Error::io("read", &path, e))?;
        entries.push((name, path, metadata));
    }
    entries.sort_by(|(a, ..), (b, ..)| a.cmp(b));
    Ok(entries)
}

/// The SHA256 and size of the file at `path`, measured by streaming it.
pub(crate) fn measure(path: &Path) -> Result<(Sha256Digest, u64)> {
    let mut file = File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::io("read", path, error)),
        };
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((Sha256Digest::from_bytes(hasher.finalize().into()), size))
}

/// Builds and checks the payload from descriptors whose hashes and sizes are
/// trusted.
///
/// Refuses no descriptors, an expiry that is not in the future, two
/// descriptors for one name, version (ignoring case, as `ck-core` compares
/// them), OS and CPU, one name and version with two licences, and any URL
/// [`artifact_url`] refuses. Runtimes are sorted by name and version, and
/// artifacts by OS and CPU.
pub fn assemble(
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
        let url = artifact_url(
            spec.prefix,
            &descriptor.name,
            &descriptor.version,
            descriptor.platform(),
            policy,
        )?;
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

/// `<prefix><name>-<version>/<file>`, where `<file>` is
/// [`asset_file_name`]: the URL of one archive on its runtime release.
///
/// Refuses a prefix that does not end in `/releases/download/`, and a URL
/// that is not in normal form, has a query or fragment, or that `policy`
/// refuses as a first hop.
pub fn artifact_url(
    prefix: &str,
    name: &RuntimeName,
    version: &RuntimeVersion,
    platform: Platform,
    policy: &TransportPolicy,
) -> Result<String> {
    if !prefix.ends_with(RELEASE_DOWNLOAD) {
        return Err(Error::Invalid(format!(
            "the release prefix {prefix:?} must end in {RELEASE_DOWNLOAD:?}"
        )));
    }
    let file = asset_file_name(name, version, platform);
    let text = format!("{prefix}{name}-{version}/{file}");
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
    policy.check_first(&url)?;
    Ok(text)
}

/// Checks every artifact's release format and that its URL in `payload`
/// is exactly the [`artifact_url`] of its runtime and platform under `prefix`.
pub fn check_urls(payload: &Payload, prefix: &str, policy: &TransportPolicy) -> Result<()> {
    for runtime in &payload.runtimes {
        for artifact in &runtime.artifacts {
            let platform = Platform {
                os: artifact.os,
                arch: artifact.arch,
            };
            check_archive_format(platform, artifact.format).map_err(|reason| {
                Error::Invalid(format!(
                    "{} {} for {platform}: {reason}",
                    runtime.name, runtime.version
                ))
            })?;
            let expected = artifact_url(prefix, &runtime.name, &runtime.version, platform, policy)?;
            if artifact.url != expected {
                return Err(Error::Invalid(format!(
                    "{} {} for {platform}: the URL {:?} is not {expected:?}",
                    runtime.name, runtime.version, artifact.url
                )));
            }
        }
    }
    Ok(())
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
/// form the owner reviews as a diff and the bytes that get signed.
pub fn write(payload: &Payload, out: &Path) -> Result<()> {
    lock::write_atomic(out, &to_pretty_json(payload)?)
}

/// Reads a payload file for `sign` and `assets verify`, and returns its exact
/// bytes and what they parse to.
///
/// The bytes must parse with `ck_core::catalog::schema::parse` and be in
/// canonical form: byte for byte what [`write`] writes for them, with one
/// entry per name and version, runtimes sorted by name and version, and
/// artifacts by OS and CPU, as [`assemble`] orders them.
pub fn read(path: &Path) -> Result<(Vec<u8>, Payload)> {
    let bytes = fs::read(path).map_err(|e| Error::io("read", path, e))?;
    let payload = schema::parse(&bytes)
        .map_err(|error| Error::Invalid(format!("{}: {error}", path.display())))?;
    let canonical = to_pretty_json(&payload)?;
    let sorted = payload
        .runtimes
        .windows(2)
        .all(|pair| (&pair[0].name, &pair[0].version) < (&pair[1].name, &pair[1].version))
        && payload.runtimes.iter().all(|runtime| {
            runtime
                .artifacts
                .is_sorted_by_key(|artifact| (artifact.os.as_str(), artifact.arch.as_str()))
        });
    if bytes != canonical || !sorted {
        return Err(Error::Invalid(format!(
            "{} is not in the canonical form `payload build` writes",
            path.display()
        )));
    }
    Ok((bytes, payload))
}

/// Shared with `assets`' tests, which read the same release folders.
#[cfg(test)]
pub(crate) mod tests {
    use ck_core::catalog::schema::{Arch, Os};
    use ck_core::paths::Profile;

    use super::*;
    use crate::test_support::TempDir;
    use crate::{DESCRIPTOR_SCHEMA, platform_archive_format};

    pub(crate) const PREFIX: &str = "https://github.com/owner/repo/releases/download/";

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
            format: platform_archive_format(Platform { os, arch: cpu }),
        }
    }

    pub(crate) fn spec(prefix: &str) -> PayloadSpec<'_> {
        PayloadSpec {
            prefix,
            sequence: 3,
            expires: EXPIRES,
        }
    }

    /// The release rules, which a debug build also applies outside a test
    /// profile.
    pub(crate) fn release() -> TransportPolicy {
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
        let payload = assemble(&descriptors, &spec(PREFIX), &release()).unwrap();
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
        let url = |tag: &str, platform: &str| format!("{PREFIX}{tag}/{tag}-{platform}.tar.gz");
        assert_eq!(
            listed,
            [
                (
                    "7.4.33-ck1",
                    "PHP-3.01",
                    url("php-7.4.33-ck1", "linux-x86_64").as_str()
                ),
                (
                    "8.5.12-ck1",
                    "PHP-3.01",
                    url("php-8.5.12-ck1", "linux-x86_64").as_str()
                ),
                (
                    "8.5.12-ck1",
                    "PHP-3.01",
                    url("php-8.5.12-ck1", "macos-aarch64").as_str()
                ),
            ]
        );
        assert_eq!(
            schema::parse(&to_pretty_json(&payload).unwrap()).unwrap(),
            payload
        );

        check_urls(&payload, PREFIX, &release()).unwrap();

        let result = assemble(
            &descriptors,
            &spec("https://example.com/owner/repo/releases/download/"),
            &release(),
        );
        assert!(
            matches!(result, Err(Error::Core(ck_core::Error::UrlRefused { .. }))),
            "{result:?}"
        );
        for prefix in [
            "http://github.com/owner/repo/releases/download/",
            "https://github.com.evil.com/owner/repo/releases/download/",
            "http://127.0.0.1:8080/releases/download/",
            "https://github.com/owner/repo/releases/download",
            "https://github.com/owner/repo/releases/",
            "https://github.com/owner/repo/releases/download/?x=1/releases/download/",
            "https://GitHub.com/owner/repo/releases/download/",
            "https://github.com/owner/repo/../releases/download/",
        ] {
            let result = assemble(&descriptors, &spec(prefix), &release());
            assert!(result.is_err(), "{prefix}: {result:?}");
        }

        // A URL that is not exactly where `assemble` puts it.
        let mut moved = payload.clone();
        moved.runtimes[0].artifacts[0].url =
            format!("{PREFIX}php-8.5.12-ck1/php-7.4.33-ck1-linux-x86_64.tar.gz");
        assert!(check_urls(&moved, PREFIX, &release()).is_err());
        assert!(
            check_urls(
                &payload,
                "https://github.com/other/repo/releases/download/",
                &release()
            )
            .is_err()
        );

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
            let result = assemble(&descriptors, &spec(PREFIX), &release());
            assert!(result.is_err(), "{case}: {result:?}");
        }

        let past = PayloadSpec {
            expires: 1_000_000_000,
            ..spec(PREFIX)
        };
        assert!(assemble(&[linux], &past, &release()).is_err());
    }

    #[test]
    fn only_a_payload_in_canonical_form_reads_back() {
        let temp = TempDir::new();
        let descriptors = [
            descriptor("8.5.12-ck1", Os::Linux, Arch::X86_64),
            descriptor("8.5.12-ck1", Os::Macos, Arch::Aarch64),
            descriptor("7.4.33-ck1", Os::Linux, Arch::X86_64),
        ];
        let payload = assemble(&descriptors, &spec(PREFIX), &release()).unwrap();
        let path = temp.0.join("payload-v1.json");
        write(&payload, &path).unwrap();
        let (bytes, read_back) = read(&path).unwrap();
        assert_eq!(bytes, fs::read(&path).unwrap());
        assert_eq!(read_back, payload);

        let compact = serde_json::to_vec(&payload).unwrap();
        let mut swapped = payload.clone();
        swapped.runtimes.swap(0, 1);
        let mut artifacts_swapped = payload.clone();
        artifacts_swapped.runtimes[1].artifacts.swap(0, 1);
        let mut no_newline = to_pretty_json(&payload).unwrap();
        no_newline.pop();
        for (case, bytes) in [
            ("compact JSON", compact),
            ("runtimes out of order", to_pretty_json(&swapped).unwrap()),
            (
                "artifacts out of order",
                to_pretty_json(&artifacts_swapped).unwrap(),
            ),
            ("no trailing newline", no_newline),
        ] {
            fs::write(&path, bytes).unwrap();
            let result = read(&path);
            assert!(result.is_err(), "{case}: {result:?}");
        }
    }

    pub(crate) const LINUX_8_5: &[u8] = b"php 8.5 for linux x86_64";
    pub(crate) const MACOS_8_5: &[u8] = b"php 8.5 for macos aarch64";
    const WINDOWS_8_5: &[u8] = b"php 8.5 for windows x86_64";
    pub(crate) const LINUX_7_4: &[u8] = b"php 7.4 for linux x86_64";

    /// Writes the folder `<root>/php-<version>` as `gh release download`
    /// leaves it: each archive, its descriptor and `SHA256SUMS`.
    fn write_release(root: &Path, version: &str, archives: &[(Os, Arch, &[u8])]) -> PathBuf {
        let folder = root.join(format!("php-{version}"));
        fs::create_dir_all(&folder).unwrap();
        let mut sums = String::new();
        for &(os, cpu, bytes) in archives {
            let mut descriptor = descriptor(version, os, cpu);
            let file = &descriptor.file;
            fs::write(folder.join(file), bytes).unwrap();
            descriptor.sha256 = Sha256Digest::of(bytes);
            descriptor.size = bytes.len() as u64;
            let stem = archive_stem(file).unwrap();
            descriptor
                .write(&folder.join(format!("{stem}.json")))
                .unwrap();
            sums.push_str(&format!("{}  {file}\n", descriptor.sha256));
        }
        fs::write(folder.join("SHA256SUMS"), sums).unwrap();
        folder
    }

    /// `<root>/releases` with two releases, 8.5.12-ck1 for three platforms and
    /// 7.4.33-ck1 for one.
    pub(crate) fn good_releases(root: &Path) -> PathBuf {
        let releases = root.join("releases");
        write_release(
            &releases,
            "8.5.12-ck1",
            &[
                (Os::Linux, Arch::X86_64, LINUX_8_5),
                (Os::Macos, Arch::Aarch64, MACOS_8_5),
                (Os::Windows, Arch::X86_64, WINDOWS_8_5),
            ],
        );
        write_release(
            &releases,
            "7.4.33-ck1",
            &[(Os::Linux, Arch::X86_64, LINUX_7_4)],
        );
        releases
    }

    /// Changes one byte of the file and keeps its length.
    pub(crate) fn flip_first_byte(path: &Path) {
        let mut bytes = fs::read(path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
    }

    // T3
    #[test]
    fn the_payload_holds_the_sizes_and_hashes_measured_from_the_release_files() {
        let temp = TempDir::new();
        let releases = good_releases(&temp.0);
        let payload = build(&releases, &spec(PREFIX), &release()).unwrap();
        let listed: Vec<(&str, &str, Sha256Digest, u64)> = payload
            .runtimes
            .iter()
            .flat_map(|runtime| {
                runtime.artifacts.iter().map(|artifact| {
                    (
                        runtime.version.as_str(),
                        artifact.os.as_str(),
                        artifact.sha256,
                        artifact.size,
                    )
                })
            })
            .collect();
        let measured = |bytes: &[u8]| (Sha256Digest::of(bytes), bytes.len() as u64);
        let expected: Vec<(&str, &str, Sha256Digest, u64)> = [
            ("7.4.33-ck1", "linux", LINUX_7_4),
            ("8.5.12-ck1", "linux", LINUX_8_5),
            ("8.5.12-ck1", "macos", MACOS_8_5),
            ("8.5.12-ck1", "windows", WINDOWS_8_5),
        ]
        .into_iter()
        .map(|(version, os, bytes)| {
            let (sha256, size) = measured(bytes);
            (version, os, sha256, size)
        })
        .collect();
        assert_eq!(listed, expected);
        let windows = &payload.runtimes[1].artifacts[2];
        assert_eq!(windows.format, ArchiveFormat::Zip);
        assert_eq!(
            windows.url,
            format!("{PREFIX}php-8.5.12-ck1/php-8.5.12-ck1-windows-x86_64.zip")
        );
        check_urls(&payload, PREFIX, &release()).unwrap();
    }

    // T3
    #[test]
    fn payload_build_refuses_a_mismatch_a_stray_file_and_a_wrong_folder() {
        let tag = "php-8.5.12-ck1";
        let linux = "php-8.5.12-ck1-linux-x86_64";
        refuses(
            "a one-byte change in an archive",
            "but its descriptor says",
            |releases| flip_first_byte(&releases.join(tag).join(format!("{linux}.tar.gz"))),
        );
        refuses(
            "a stray file in a release folder",
            "is not an archive",
            |releases| fs::write(releases.join(tag).join("notes.txt"), b"x").unwrap(),
        );
        refuses(
            "a folder not named by its descriptors' tag",
            "belongs in a folder named",
            |releases| fs::rename(releases.join(tag), releases.join("php-8.5.13-ck1")).unwrap(),
        );
        refuses(
            "a SHA256SUMS that omits an archive",
            "does not list",
            |releases| {
                let macos = "php-8.5.12-ck1-macos-aarch64.tar.gz";
                let kept = format!("{}  {macos}\n", Sha256Digest::of(MACOS_8_5));
                fs::write(releases.join(tag).join("SHA256SUMS"), kept).unwrap();
            },
        );
        refuses(
            "a SHA256SUMS that disagrees with the descriptors",
            "but SHA256SUMS says",
            |releases| {
                let sums = releases.join(tag).join("SHA256SUMS");
                let text = fs::read_to_string(&sums).unwrap();
                let right = Sha256Digest::of(LINUX_8_5).to_string();
                let wrong = Sha256Digest::of(b"another archive").to_string();
                fs::write(&sums, text.replace(&right, &wrong)).unwrap();
            },
        );
        refuses(
            "an archive without its descriptor",
            "has no descriptor",
            |releases| fs::remove_file(releases.join(tag).join(format!("{linux}.json"))).unwrap(),
        );
    }

    #[test]
    fn payload_build_refuses_wrong_archive_suffixes_and_shared_descriptor_stems() {
        let tag = "php-8.5.12-ck1";
        let windows = "php-8.5.12-ck1-windows-x86_64";
        refuses(
            "a ZIP and tar.gz with the same descriptor stem",
            "share descriptor",
            |releases| {
                fs::write(releases.join(tag).join(format!("{windows}.tar.gz")), b"x").unwrap()
            },
        );
        refuses(
            "a Windows archive renamed to tar.gz (also in SHA256SUMS)",
            "not \"php-8.5.12-ck1-windows-x86_64.tar.gz\"",
            |releases| {
                let folder = releases.join(tag);
                let zip = format!("{windows}.zip");
                let tar_gz = format!("{windows}.tar.gz");
                fs::rename(folder.join(&zip), folder.join(&tar_gz)).unwrap();
                let sums = folder.join(SUMS);
                let text = fs::read_to_string(&sums).unwrap();
                fs::write(&sums, text.replace(&zip, &tar_gz)).unwrap();
            },
        );
    }

    #[test]
    fn descriptors_require_the_platforms_release_format_and_name() {
        let linux = descriptor("8.5.12-ck1", Os::Linux, Arch::X86_64);
        let windows = descriptor("8.5.12-ck1", Os::Windows, Arch::X86_64);
        for invalid in [
            Descriptor {
                format: ArchiveFormat::Zip,
                ..linux
            },
            Descriptor {
                format: ArchiveFormat::TarGz,
                ..windows.clone()
            },
            descriptor("8.5.12-ck1", Os::Windows, Arch::Aarch64),
            Descriptor {
                file: windows.file.replace(".zip", ".tar.gz"),
                ..windows
            },
        ] {
            assert!(Descriptor::parse(&to_pretty_json(&invalid).unwrap()).is_err());
        }
    }

    /// Builds from [`good_releases`] after `change`, and expects a refusal
    /// whose message holds `reason`.
    fn refuses(case: &str, reason: &str, change: impl Fn(&Path)) {
        let temp = TempDir::new();
        let releases = good_releases(&temp.0);
        change(&releases);
        let result = build(&releases, &spec(PREFIX), &release());
        assert!(
            matches!(&result, Err(Error::Invalid(message)) if message.contains(reason)),
            "{case}: {result:?}"
        );
    }
}
