//! T16: [`HttpTransport`] against the loopback test server.
//!
//! In the crate, not in `tests/`, so a stalled body can use a short read
//! timeout through the private constructor. Debug builds only: the policy of
//! a release build refuses plain HTTP to loopback.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use ck_core::download::fetch::fetch_bytes;
use ck_core::download::policy::TransportPolicy;
use ck_core::download::transport::{CATALOG_TIMEOUT, READ_TIMEOUT, Request, Response, Transport};
use ck_core::paths::Profile;
use ck_core::{Error, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use url::Url;

use super::HttpTransport;

#[path = "../tests/support/server.rs"]
mod server;

use self::server::{Reply, TestServer};

/// A stall this long fails a body; far below [`READ_TIMEOUT`].
const SHORT: Duration = Duration::from_secs(1);

/// The catalog cap for `fetch_bytes`; every body here is smaller.
const CAP: usize = 1024;

/// The debug test profile's rules, which allow plain HTTP to loopback.
fn test_policy() -> TransportPolicy {
    TransportPolicy::current(Profile::Isolated)
}

fn client() -> HttpTransport {
    HttpTransport::new(&test_policy())
}

fn get(transport: &HttpTransport, url: &str, total_timeout: Option<Duration>) -> Result<Response> {
    let url = Url::parse(url).unwrap();
    transport.get(&Request {
        url: &url,
        total_timeout,
    })
}

/// The error of a request that should have failed. `Response` has no
/// `Debug`, so `unwrap_err` is not available.
fn failure(result: Result<Response>) -> Error {
    match result {
        Ok(response) => panic!("the request succeeded with {}", response.status),
        Err(error) => error,
    }
}

fn read_all(mut response: Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    response.body.read_to_end(&mut bytes).unwrap();
    bytes
}

/// Every query and user name here holds SECRET, which no error may show.
fn assert_hides_secret(error: &Error) {
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains("SECRET"), "{text}");
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn a_redirect_comes_back_unfollowed_and_ck_core_refuses_one_off_loopback() {
    let server = TestServer::start();
    let transport = client();
    // ureq's default reads a proxy from the environment, which a test cannot
    // set safely; the agent must have none, and follow no redirect.
    let config = transport.agent.config();
    assert!(config.proxy().is_none());
    assert_eq!(config.max_redirects(), 0);
    // On the same server, so a redirect that ureq followed would be recorded.
    server.reply("/start", Reply::redirect(302, "/next"));
    server.reply("/next", Reply::ok(b"next".to_vec()));

    let response = get(&transport, &server.url("/start"), None).unwrap();
    assert_eq!(response.status, 302);
    assert_eq!(response.location.as_deref(), Some("/next"));
    assert_eq!(server.request_lines(), ["GET /start HTTP/1.1"]);

    // Through ck-core, each hop is checked before it is requested. These
    // never leave the machine even if that check broke: `localhost` is this
    // server, and `.invalid` never resolves.
    let port = server.port();
    for (target, location) in [
        (
            "/to-name",
            format!("http://localhost:{port}/next?token=SECRET"),
        ),
        (
            "/to-other",
            format!("http://example.invalid:{port}/next?token=SECRET"),
        ),
    ] {
        server.reply(target, Reply::redirect(302, &location));
        let error = fetch_bytes(
            &transport,
            &test_policy(),
            &server.url(target),
            CAP,
            CATALOG_TIMEOUT,
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::UrlRefused { .. }),
            "{location}: {error:?}"
        );
        assert_hides_secret(&error);
    }

    // A redirect to the literal loopback address is followed, by ck-core.
    server.reply("/hop", Reply::redirect(307, &server.url("/next")));
    let bytes = fetch_bytes(
        &transport,
        &test_policy(),
        &server.url("/hop"),
        CAP,
        CATALOG_TIMEOUT,
    )
    .unwrap();
    assert_eq!(bytes, b"next");
    assert_eq!(
        server.request_lines(),
        [
            "GET /start HTTP/1.1",
            "GET /to-name HTTP/1.1",
            "GET /to-other HTTP/1.1",
            "GET /hop HTTP/1.1",
            "GET /next HTTP/1.1",
        ]
    );
}

#[test]
fn a_stalled_body_fails_after_the_read_timeout_or_the_total_timeout() {
    let server = TestServer::start();
    server.reply(
        "/stall",
        Reply::ok(b"0123456789".to_vec()).stalling_after(3),
    );

    // A short limit on each read; then the standard one, under a short
    // total timeout.
    let cases = [
        (
            HttpTransport::with_read_timeout(&test_policy(), SHORT),
            None,
        ),
        (client(), Some(SHORT)),
    ];
    for (transport, total_timeout) in cases {
        let started = Instant::now();
        let mut response = get(&transport, &server.url("/stall"), total_timeout).unwrap();
        assert_eq!(response.content_length, Some(10));

        let mut bytes = Vec::new();
        let error = response.body.read_to_end(&mut bytes).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut, "{total_timeout:?}");
        assert_eq!(bytes, b"012", "{total_timeout:?}");
        // The short limit fired, not READ_TIMEOUT.
        assert!(
            started.elapsed() < READ_TIMEOUT / 2,
            "{total_timeout:?}: {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn the_body_and_its_length_come_back_as_sent() {
    let server = TestServer::start();
    let transport = client();
    let gzipped = gzip(b"catalog bytes");
    server.reply(
        "/gzip",
        Reply::ok(gzipped.clone()).header("Content-Encoding", "gzip"),
    );
    server.reply(
        "/no-length",
        Reply::ok(b"until the end".to_vec()).without("Content-Length"),
    );
    server.reply(
        "/bad-length",
        Reply::ok(b"abc".to_vec())
            .without("Content-Length")
            .header("Content-Length", "3x"),
    );
    server.reply(
        "/two-lengths",
        Reply::ok(b"abc".to_vec()).header("Content-Length", "4"),
    );

    // Never decoded.
    let response = get(&transport, &server.url("/gzip"), None).unwrap();
    assert_eq!(response.content_length, Some(gzipped.len() as u64));
    assert_eq!(read_all(response), gzipped);

    // No length: the body runs to the end of the connection.
    let response = get(&transport, &server.url("/no-length"), None).unwrap();
    assert_eq!(response.content_length, None);
    assert_eq!(read_all(response), b"until the end");

    for target in ["/bad-length", "/two-lengths"] {
        let error = failure(get(&transport, &server.url(target), None));
        assert!(
            matches!(error, Error::DownloadFailed { .. }),
            "{target}: {error:?}"
        );
    }
}

#[test]
fn requests_ask_for_identity_and_carry_no_cookie_or_credentials() {
    let server = TestServer::start();
    let transport = client();
    server.reply(
        "/first",
        Reply::ok(b"one".to_vec()).header("Set-Cookie", "session=SECRET; Path=/"),
    );
    server.reply("/second", Reply::ok(b"two".to_vec()));

    for (target, body) in [("/first", b"one"), ("/second", b"two")] {
        let response = get(&transport, &server.url(target), None).unwrap();
        assert_eq!(read_all(response), body);
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.header("accept-encoding"),
            Some("identity"),
            "{request:?}"
        );
        assert_eq!(request.header("cookie"), None, "{request:?}");
        assert_eq!(request.header("authorization"), None, "{request:?}");
    }

    // ureq would send these as an `Authorization` header, so the URL is
    // refused before anything is sent.
    let with_user = format!("http://user:SECRET@127.0.0.1:{}/third", server.port());
    let error = failure(get(&transport, &with_user, None));
    assert!(matches!(error, Error::DownloadFailed { .. }), "{error:?}");
    assert_hides_secret(&error);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn transport_errors_are_errors_that_never_show_the_query() {
    let server = TestServer::start();

    // HTTPS to the plain listener: the ring provider and the platform
    // verifier are set up at run time, then the handshake fails. On a
    // computer with no CA certificates the set-up fails instead.
    let https = format!("https://127.0.0.1:{}/catalog?token=SECRET", server.port());
    let error = failure(get(&client(), &https, None));
    assert!(matches!(error, Error::DownloadFailed { .. }), "{error:?}");
    assert_hides_secret(&error);

    // The standard profile's agent is HTTPS only, and ureq's own error for
    // this names the whole URL.
    let https_only = HttpTransport::new(&TransportPolicy::current(Profile::Standard));
    let error = failure(get(&https_only, &server.url("/x?token=SECRET"), None));
    assert!(matches!(error, Error::DownloadFailed { .. }), "{error:?}");
    assert_hides_secret(&error);

    assert!(server.requests().is_empty());
}
