//! Fetching: the redirect loop, the size caps, and hashing to a `.part` file.
//!
//! Redirects are followed here, not by the transport: each hop is checked by
//! the policy before it is requested. 301, 302, 303, 307 and 308 are
//! redirects; only 200 succeeds, and any other status fails.
//!
//! Errors name URLs only through [`policy::redact`](super::policy::redact):
//!
//! - a URL or redirect the policy refuses: [`Error::UrlRefused`];
//! - another status, a redirect without `Location`, more than
//!   [`MAX_REDIRECTS`](super::policy::MAX_REDIRECTS) redirects, a transport
//!   or read error, or more bytes than a `max_bytes` cap:
//!   [`Error::DownloadFailed`];
//! - an artifact that is not its pinned size: [`Error::SizeMismatch`]; not
//!   its pinned digest: [`Error::ChecksumMismatch`].
//!
//! URLs are parsed strictly: a URL that the `url` crate parses only by
//! correcting it (a backslash, a space, a tab, credentials) is refused. What
//! it still normalises (host case, IDNA, short IPv4 forms, a `:443` port) is
//! checked, and requested, in its normalised form.

use std::cell::Cell;
use std::fs::File;
use std::hint::black_box;
use std::io::{self, ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use url::{SyntaxViolation, Url};

use crate::catalog::schema::{Artifact, RuntimeName, RuntimeVersion, Sha256Digest};
use crate::download::policy::{MAX_REDIRECTS, TransportPolicy, redact};
use crate::download::transport::{Request, Response, Transport};
use crate::download::verified::{PartFile, VerifiedFile};
use crate::error::{Error, Result};
use crate::layout;
use crate::paths::Paths;

/// Bytes read from a body at a time.
const CHUNK_BYTES: usize = 64 * 1024;

/// Downloads `artifact` of `name` `version` into a new
/// [`Paths::new_part_file`] (created 0600), hashing as it streams, and
/// returns it once its SHA-256 and size match the pins.
///
/// The file is created with `create_new` and opened for reading as well as
/// writing: the returned [`VerifiedFile`] reads the checked bytes back
/// through that same handle.
///
/// A `Content-Length` other than the pinned size fails before the body is
/// read; one byte over the size, or a body cut short, fails too
/// ([`Error::SizeMismatch`]). Another digest gives [`Error::ChecksumMismatch`].
/// Any failure removes the file. On Unix, `downloads/` must be a private
/// folder ([`Error::NotPrivate`]).
///
/// The caller holds the version's lock (`Paths::runtime_lock_file`).
pub fn fetch_artifact(
    transport: &dyn Transport,
    policy: &TransportPolicy,
    paths: &Paths,
    name: &RuntimeName,
    version: &RuntimeVersion,
    artifact: &Artifact,
) -> Result<VerifiedFile> {
    // Before anything is requested or written. On Windows `downloads/`
    // inherits the data folder's protected DACL.
    #[cfg(unix)]
    layout::check_private_dir(&paths.downloads_dir())?;

    let first = parse_first(&artifact.url)?;
    let shown = redact(&first);
    let (_, response) = follow(transport, policy, &first, None)?;
    if let Some(announced) = response.content_length
        && announced != artifact.size
    {
        return Err(Error::SizeMismatch {
            url: shown,
            expected: artifact.size,
            detail: format!("the server announces {announced} bytes"),
        });
    }

    let path = paths.new_part_file(name, version);
    let mut file = layout::private_file_options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| Error::io("create", &path, e))?;
    // Only once `create_new` has succeeded, so the guard never removes a
    // file this call did not create.
    let part = PartFile { path };
    match write_checked(response.body, &mut file, &part, artifact, &shown) {
        Ok(()) => Ok(VerifiedFile {
            file,
            part,
            format: artifact.format,
            size: artifact.size,
        }),
        Err(error) => {
            // The handle closes before the file is removed, which Windows needs.
            drop(file);
            drop(part);
            Err(error)
        }
    }
}

/// Fetches `url` into memory: at most `max_bytes`, and within
/// `total_timeout` for all hops together. For the catalog, whose signature is
/// checked afterwards instead of a pin. More bytes than `max_bytes` fails.
pub fn fetch_bytes(
    transport: &dyn Transport,
    policy: &TransportPolicy,
    url: &str,
    max_bytes: usize,
    total_timeout: Duration,
) -> Result<Vec<u8>> {
    let first = parse_first(url)?;
    let budget = Budget {
        started: Instant::now(),
        total: total_timeout,
    };
    let (last, response) = follow(transport, policy, &first, Some(budget))?;
    let shown = redact(&last);
    let too_big = || Error::DownloadFailed {
        url: shown.clone(),
        reason: format!("it is over the {max_bytes}-byte limit"),
    };

    let limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    if response
        .content_length
        .is_some_and(|announced| announced > limit)
    {
        return Err(too_big());
    }
    // One byte over the cap is enough to refuse the body.
    let mut body = response.body.take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes)
        .map_err(|e| read_failed(&shown, &e))?;
    if bytes.len() > max_bytes {
        return Err(too_big());
    }
    Ok(bytes)
}

/// Requests `first`, then each redirect the policy allows, and returns the
/// 200 response with the URL that gave it. Every URL is checked before it is
/// requested. With a `budget`, each request gets what is left of it.
fn follow(
    transport: &dyn Transport,
    policy: &TransportPolicy,
    first: &Url,
    budget: Option<Budget>,
) -> Result<(Url, Response)> {
    policy.check_first(first)?;
    let mut current = first.clone();
    let mut redirects = 0;
    loop {
        let total_timeout = budget.map(|budget| budget.left(first)).transpose()?;
        let response = transport.get(&Request {
            url: &current,
            total_timeout,
        })?;
        let status = response.status;
        if status == 200 {
            return Ok((current, response));
        }
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(failed(&current, format!("the server answered {status}")));
        }
        if redirects == MAX_REDIRECTS {
            return Err(failed(
                first,
                format!("it redirects more than {MAX_REDIRECTS} times"),
            ));
        }
        let Some(location) = response.location.as_deref() else {
            return Err(failed(
                &current,
                format!("the server answered {status} without a Location"),
            ));
        };
        let next = parse_location(&current, location)?;
        policy.check_redirect(&current, &next)?;
        redirects += 1;
        current = next;
    }
}

/// Streams `body` into `file` while hashing it, then checks the pinned size
/// and digest. `shown` is the redacted URL for errors.
fn write_checked(
    body: Box<dyn Read>,
    file: &mut File,
    part: &PartFile,
    artifact: &Artifact,
    shown: &str,
) -> Result<()> {
    let expected = artifact.size;
    let size_mismatch = |detail: String| Error::SizeMismatch {
        url: shown.to_owned(),
        expected,
        detail,
    };

    // One byte over the pin is enough to refuse the body.
    let mut body = body.take(expected.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; CHUNK_BYTES];
    let mut received: u64 = 0;
    loop {
        let read = match body.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_failed(shown, &e)),
        };
        received += read as u64;
        if received > expected {
            return Err(size_mismatch("the body is longer".to_owned()));
        }
        let bytes = &chunk[..read];
        hasher.update(bytes);
        file.write_all(bytes)
            .map_err(|e| Error::io("write", part.path(), e))?;
    }
    if received < expected {
        return Err(size_mismatch(format!(
            "the body ended after {received} bytes"
        )));
    }

    let actual: [u8; 32] = hasher.finalize().into();
    if !digests_match(&actual, artifact.sha256.as_bytes()) {
        return Err(Error::ChecksumMismatch {
            url: shown.to_owned(),
            expected: artifact.sha256.to_string(),
            actual: Sha256Digest::from_bytes(actual).to_string(),
        });
    }
    Ok(())
}

/// Compares every byte whatever differs, so the time taken does not show
/// where two digests part.
fn digests_match(actual: &[u8; 32], expected: &[u8; 32]) -> bool {
    let mut difference = 0;
    for (a, b) in actual.iter().zip(expected) {
        difference |= black_box(a ^ b);
    }
    difference == 0
}

/// A limit on all the hops of one fetch together.
#[derive(Clone, Copy)]
struct Budget {
    started: Instant,
    total: Duration,
}

impl Budget {
    /// What is left for the next request; [`Error::DownloadFailed`] once
    /// nothing is. `first` names the fetch in the error.
    fn left(&self, first: &Url) -> Result<Duration> {
        let left = self.total.saturating_sub(self.started.elapsed());
        if left.is_zero() {
            Err(failed(
                first,
                format!("it took longer than {:?}", self.total),
            ))
        } else {
            Ok(left)
        }
    }
}

/// A URL the `url` crate refused, or parsed only by correcting it.
enum Unparsed {
    Invalid(url::ParseError),
    Corrected(Url, SyntaxViolation),
}

impl Unparsed {
    /// The refusal. A URL that does not parse is never echoed, since it may
    /// hold a token: `unreadable` names it instead.
    fn refused(self, unreadable: String) -> Error {
        match self {
            Self::Invalid(error) => Error::UrlRefused {
                url: unreadable,
                reason: format!("it is not a valid URL ({error})"),
            },
            Self::Corrected(url, violation) => Error::UrlRefused {
                url: redact(&url),
                reason: format!("it is not written strictly ({violation})"),
            },
        }
    }
}

/// `text`, resolved against `base` when given, refusing any syntax the
/// `url` crate would have to correct.
fn parse_strict(text: &str, base: Option<&Url>) -> Result<Url, Unparsed> {
    let violation = Cell::new(None);
    let record = |found: SyntaxViolation| {
        if violation.get().is_none() {
            violation.set(Some(found));
        }
    };
    let url = Url::options()
        .base_url(base)
        .syntax_violation_callback(Some(&record))
        .parse(text)
        .map_err(Unparsed::Invalid)?;
    match violation.get() {
        Some(found) => Err(Unparsed::Corrected(url, found)),
        None => Ok(url),
    }
}

fn parse_first(text: &str) -> Result<Url> {
    parse_strict(text, None).map_err(|unparsed| unparsed.refused("the download URL".to_owned()))
}

/// A redirect's `Location`, which may be relative to `current`.
fn parse_location(current: &Url, location: &str) -> Result<Url> {
    parse_strict(location, Some(current))
        .map_err(|unparsed| unparsed.refused(format!("the redirect from {}", redact(current))))
}

fn failed(url: &Url, reason: String) -> Error {
    Error::DownloadFailed {
        url: redact(url),
        reason,
    }
}

/// Keeps only the error's kind: the transport's message could hold the full
/// URL.
fn read_failed(shown: &str, error: &io::Error) -> Error {
    Error::DownloadFailed {
        url: shown.to_owned(),
        reason: format!("reading the body failed: {}", error.kind()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::catalog::schema::{Arch, ArchiveFormat, Os};
    use crate::download::transport::{CATALOG_TIMEOUT, FakeReply, FakeTransport};
    use crate::paths::{BuildKind, Profile};
    use crate::test_support::TempDir;

    // Every query and userinfo holds SECRET, which no error may show.
    const CATALOG: &str =
        "https://github.com/o/r/releases/download/catalog/catalog-v1.json?token=SECRET";
    const ASSET: &str = "https://github.com/o/r/releases/download/v8.4.3/php.tar.gz?token=SECRET";
    // Shaped like GitHub's signed asset URLs, which must pass strict parsing.
    const SIGNED: &str = "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sv=2018-11-09&se=2026-11-19T10%3A00%3A00Z&rscd=attachment%3B+filename%3Dphp.tar.gz&sig=SECRET%2Bx%3D&jwt=SECRET.part_two-three";
    const CATALOG_CAP: usize = 64;

    fn release() -> TransportPolicy {
        TransportPolicy::for_build(BuildKind::Release, Profile::Standard)
    }

    fn get_catalog(transport: &FakeTransport, policy: &TransportPolicy) -> Result<Vec<u8>> {
        fetch_bytes(transport, policy, CATALOG, CATALOG_CAP, CATALOG_TIMEOUT)
    }

    fn requested(transport: &FakeTransport) -> Vec<String> {
        transport
            .requests()
            .into_iter()
            .map(|request| request.url)
            .collect()
    }

    fn assert_hides_secrets(error: &Error) {
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("SECRET"), "{text}");
        }
    }

    fn assert_refused(result: Result<Vec<u8>>, case: &str) {
        let error = result.unwrap_err();
        assert!(
            matches!(error, Error::UrlRefused { .. }),
            "{case}: {error:?}"
        );
        assert_hides_secrets(&error);
    }

    #[test]
    fn a_redirect_the_policy_refuses_is_never_requested() {
        let refused = [
            "https://evil.example/catalog-v1.json?token=SECRET",
            "//evil.example/catalog-v1.json?token=SECRET",
            // Relative, so back to github.com: a first hop only.
            "/o/r/releases/download/catalog/other.json?token=SECRET",
            "https://objects.githubusercontent.com:8443/c?sig=SECRET",
            "https://SECRET@objects.githubusercontent.com/c?sig=SECRET",
            // Does not parse: only the hop it came from is named.
            "https://objects.githubusercontent.com:99999/c?sig=SECRET",
            // Parses only by turning `\` into `/`.
            "https://objects.githubusercontent.com\\c?sig=SECRET",
        ];
        for location in refused {
            let transport = FakeTransport::new();
            transport.reply(CATALOG, FakeReply::redirect(302, location));
            assert_refused(get_catalog(&transport, &release()), location);
            assert_eq!(requested(&transport), [CATALOG], "{location}");
        }

        // In the debug test profile the loopback server is allowed, but never
        // as a redirect from HTTPS.
        let test_profile = TransportPolicy::for_build(BuildKind::Debug, Profile::Isolated);
        let local = "http://127.0.0.1:8080/catalog-v1.json?token=SECRET";
        let local_v6 = "http://[::1]:8080/catalog-v1.json?token=SECRET";
        let transport = FakeTransport::new();
        transport.reply(CATALOG, FakeReply::redirect(301, local));
        for direct in [local, local_v6] {
            transport.reply(direct, FakeReply::ok(b"catalog".to_vec()));
            let bytes = fetch_bytes(
                &transport,
                &test_profile,
                direct,
                CATALOG_CAP,
                CATALOG_TIMEOUT,
            );
            assert_eq!(bytes.unwrap(), b"catalog", "{direct}");
        }
        assert_refused(get_catalog(&transport, &test_profile), "HTTPS to HTTP");
        assert_eq!(requested(&transport), [local, local_v6, CATALOG]);
    }

    #[test]
    fn a_first_url_that_needs_correcting_is_refused_unrequested() {
        let transport = FakeTransport::new();
        for url in [
            "https://github.com\\o\\r\\catalog-v1.json?token=SECRET",
            " https://github.com/o/r/catalog-v1.json?token=SECRET",
            "https://github.com/o/r/cat\talog-v1.json?token=SECRET",
            "github.com/o/r/catalog-v1.json?token=SECRET",
        ] {
            let result = fetch_bytes(&transport, &release(), url, CATALOG_CAP, CATALOG_TIMEOUT);
            assert_refused(result, url);
        }
        assert!(transport.requests().is_empty());
    }

    fn hop(n: usize) -> String {
        format!("https://objects.githubusercontent.com/hop/{n}?sig=SECRET")
    }

    /// CATALOG redirects to hop 1, and so on to hop 5, once with each
    /// redirect status; hop 5 answers `last`.
    fn five_redirects(last: FakeReply) -> FakeTransport {
        let statuses: [u16; MAX_REDIRECTS] = [301, 302, 303, 307, 308];
        let transport = FakeTransport::new();
        let mut from = CATALOG.to_owned();
        for (n, status) in (1..).zip(statuses) {
            transport.reply(&from, FakeReply::redirect(status, &hop(n)));
            from = hop(n);
        }
        transport.reply(&from, last);
        transport
    }

    #[test]
    fn five_redirects_are_followed_and_a_sixth_is_not() {
        let mut expected = vec![CATALOG.to_owned()];
        expected.extend((1..=MAX_REDIRECTS).map(hop));

        let transport = five_redirects(FakeReply::ok(b"catalog".to_vec()));
        assert_eq!(get_catalog(&transport, &release()).unwrap(), b"catalog");
        assert_eq!(requested(&transport), expected);
        // Every hop shares the one total timeout.
        assert!(transport.requests().iter().all(|request| {
            request
                .total_timeout
                .is_some_and(|left| left <= CATALOG_TIMEOUT)
        }));

        let transport = five_redirects(FakeReply::redirect(302, &hop(MAX_REDIRECTS + 1)));
        let error = get_catalog(&transport, &release()).unwrap_err();
        assert!(matches!(error, Error::DownloadFailed { .. }), "{error:?}");
        assert_hides_secrets(&error);
        assert_eq!(requested(&transport), expected);
    }

    #[test]
    fn only_200_succeeds_and_only_the_five_redirect_statuses_redirect() {
        let failing = [
            FakeReply {
                status: 404,
                ..FakeReply::ok(b"not found".to_vec())
            },
            FakeReply {
                status: 206,
                ..FakeReply::ok(b"catalog".to_vec())
            },
            FakeReply::redirect(300, SIGNED),
            FakeReply::redirect(304, SIGNED),
            FakeReply {
                location: None,
                ..FakeReply::redirect(302, SIGNED)
            },
        ];
        for reply in failing {
            let transport = FakeTransport::new();
            transport.reply(CATALOG, reply.clone());
            transport.reply(SIGNED, FakeReply::ok(b"catalog".to_vec()));
            let error = get_catalog(&transport, &release()).unwrap_err();
            assert!(
                matches!(error, Error::DownloadFailed { .. }),
                "{reply:?}: {error:?}"
            );
            assert_hides_secrets(&error);
            assert_eq!(requested(&transport), [CATALOG], "{reply:?}");
        }
    }

    #[test]
    fn a_catalog_over_its_cap_fails() {
        let over = vec![b'x'; CATALOG_CAP + 1];
        let cases = [
            (FakeReply::ok(vec![b'x'; CATALOG_CAP]), true),
            (FakeReply::ok(over.clone()), false),
            (
                FakeReply {
                    content_length: None,
                    ..FakeReply::ok(over.clone())
                },
                false,
            ),
            (
                FakeReply {
                    content_length: Some(CATALOG_CAP as u64),
                    ..FakeReply::ok(over)
                },
                false,
            ),
        ];
        for (reply, fits) in cases {
            let transport = FakeTransport::new();
            transport.reply(CATALOG, reply.clone());
            match get_catalog(&transport, &release()) {
                Ok(bytes) => assert!(fits && bytes.len() == CATALOG_CAP, "{reply:?}"),
                Err(error) => {
                    assert!(
                        !fits && matches!(error, Error::DownloadFailed { .. }),
                        "{reply:?}: {error:?}"
                    );
                    assert_hides_secrets(&error);
                }
            }
        }
    }

    fn data_folder(root: &TempDir) -> Paths {
        let paths = Paths {
            home: root.path().to_path_buf(),
            data: root.path().join("data"),
            profile: Profile::Isolated,
        };
        layout::ensure(&paths).unwrap();
        paths
    }

    fn leftovers(paths: &Paths) -> Vec<PathBuf> {
        fs::read_dir(paths.downloads_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    /// Several reads of the copy loop, the last one short.
    fn archive_bytes() -> Vec<u8> {
        (0..2 * CHUNK_BYTES + 123)
            .map(|i| (i % 251) as u8)
            .collect()
    }

    fn pinned(body: &[u8]) -> Artifact {
        Artifact {
            os: Os::Linux,
            arch: Arch::X86_64,
            url: ASSET.to_owned(),
            sha256: Sha256Digest::of(body),
            size: body.len() as u64,
            format: ArchiveFormat::TarGz,
        }
    }

    /// GitHub's shape: ASSET redirects to SIGNED, which answers `reply`.
    fn github(reply: FakeReply) -> FakeTransport {
        let transport = FakeTransport::new();
        transport.reply(ASSET, FakeReply::redirect(302, SIGNED));
        transport.reply(SIGNED, reply);
        transport
    }

    fn download(
        transport: &FakeTransport,
        paths: &Paths,
        artifact: &Artifact,
    ) -> Result<VerifiedFile> {
        let name = RuntimeName::new("php").unwrap();
        let version = RuntimeVersion::new("8.4.3").unwrap();
        fetch_artifact(transport, &release(), paths, &name, &version, artifact)
    }

    #[test]
    fn a_download_off_its_pins_fails_and_leaves_no_file() {
        let root = TempDir::new("fetch");
        let paths = data_folder(&root);
        let body = archive_bytes();
        let size = body.len() as u64;
        let mut longer = body.clone();
        longer.push(0);
        let other_digest = Artifact {
            sha256: Sha256Digest::of(b"other bytes"),
            ..pinned(&body)
        };

        let cases = [
            (
                "one byte over, no Content-Length",
                FakeReply {
                    content_length: None,
                    ..FakeReply::ok(longer.clone())
                },
                pinned(&body),
                "size",
            ),
            (
                "one byte over the Content-Length it announced",
                FakeReply {
                    content_length: Some(size),
                    ..FakeReply::ok(longer)
                },
                pinned(&body),
                "size",
            ),
            (
                "cut short",
                FakeReply {
                    content_length: Some(size),
                    ..FakeReply::ok(body[..body.len() - 1].to_vec())
                },
                pinned(&body),
                "size",
            ),
            (
                "another Content-Length",
                FakeReply {
                    content_length: Some(size - 1),
                    ..FakeReply::ok(body.clone())
                },
                pinned(&body),
                "size",
            ),
            (
                "another digest",
                FakeReply::ok(body.clone()),
                other_digest,
                "checksum",
            ),
            (
                "a body that stalls",
                FakeReply {
                    content_length: Some(size),
                    body_error: Some(ErrorKind::TimedOut),
                    ..FakeReply::ok(body[..CHUNK_BYTES].to_vec())
                },
                pinned(&body),
                "download",
            ),
        ];
        for (case, reply, artifact, kind) in cases {
            let error = download(&github(reply), &paths, &artifact).unwrap_err();
            let actual = match error {
                Error::SizeMismatch { .. } => "size",
                Error::ChecksumMismatch { .. } => "checksum",
                Error::DownloadFailed { .. } => "download",
                _ => "other",
            };
            assert_eq!(actual, kind, "{case}: {error:?}");
            assert_hides_secrets(&error);
            assert_eq!(leftovers(&paths), Vec::<PathBuf>::new(), "{case}");
        }
    }

    #[test]
    fn a_matching_download_is_a_private_file_read_through_its_own_handle() {
        let root = TempDir::new("fetch");
        let paths = data_folder(&root);
        let body = archive_bytes();
        let transport = github(FakeReply::ok(body.clone()));

        let mut verified = download(&transport, &paths, &pinned(&body)).unwrap();
        assert_eq!(
            (verified.format(), verified.size()),
            (ArchiveFormat::TarGz, body.len() as u64)
        );
        // Artifacts have no total timeout: the pinned size bounds them.
        let requests: Vec<_> = transport
            .requests()
            .into_iter()
            .map(|request| (request.url, request.total_timeout))
            .collect();
        assert_eq!(
            requests,
            [(ASSET.to_owned(), None), (SIGNED.to_owned(), None)]
        );
        let path = verified.part.path().to_path_buf();
        assert_eq!(leftovers(&paths), [path.as_path()]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            // A file swapped in at the path is never read: the bytes come
            // through the handle that hashed them.
            fs::remove_file(&path).unwrap();
            fs::write(&path, b"swapped").unwrap();
        }
        for _ in 0..2 {
            let mut read = Vec::new();
            verified.reader().unwrap().read_to_end(&mut read).unwrap();
            assert_eq!(read, body);
        }

        drop(verified);
        assert_eq!(leftovers(&paths), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn a_downloads_folder_that_is_not_private_is_refused_before_any_request() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new("fetch");
        let paths = data_folder(&root);
        fs::set_permissions(paths.downloads_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        let body = archive_bytes();
        let transport = github(FakeReply::ok(body.clone()));

        let error = download(&transport, &paths, &pinned(&body)).unwrap_err();
        assert!(matches!(error, Error::NotPrivate { .. }), "{error:?}");
        assert!(transport.requests().is_empty());
        assert_eq!(leftovers(&paths), Vec::<PathBuf>::new());
    }
}
