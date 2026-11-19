//! The one HTTP call the downloader makes, as a trait. `ck-net` implements it
//! over HTTPS; unit tests script it with [`FakeTransport`].
//!
//! The transport sends a request and returns what came back. Every decision
//! (which URLs, which redirects, how many bytes, which statuses) is made in
//! `ck-core`, by [`fetch`](super::fetch) under the
//! [`TransportPolicy`](super::policy::TransportPolicy).

use std::io::Read;
use std::time::Duration;

use url::Url;

use crate::error::Result;

/// Resolving a host name.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connecting, TLS handshake included.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// From sending the request to the end of the response headers.
pub const HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

/// Waiting for each read of the body: a stalled body fails after this long.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The whole catalog fetch, every redirect hop included.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// One GET. No `Debug`: the URL may carry a signed token, so it is printed
/// only through `policy::redact`.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub url: &'a Url,
    /// A limit on the whole request, body included. `None` for artifacts,
    /// whose pinned size bounds them; the phase timeouts above always apply.
    pub total_timeout: Option<Duration>,
}

/// The response as received: the body is not decoded and redirects are not
/// followed.
pub struct Response {
    pub status: u16,
    /// The raw `Location` header, which may be relative.
    pub location: Option<String>,
    pub content_length: Option<u64>,
    /// Reads fail with [`std::io::ErrorKind::TimedOut`] after
    /// [`READ_TIMEOUT`] without data.
    pub body: Box<dyn Read>,
}

/// Sends GET requests.
///
/// An implementation follows no redirect, never decodes the body (it sends
/// `Accept-Encoding: identity`), sends no cookie or credentials, uses no
/// proxy, and applies the timeouts above. Its errors are
/// [`Error::DownloadFailed`](crate::Error::DownloadFailed) with the URL
/// written by [`policy::redact`](super::policy::redact).
pub trait Transport: Send + Sync {
    fn get(&self, request: &Request<'_>) -> Result<Response>;
}

#[cfg(test)]
pub(crate) use self::fake::{FakeReply, FakeRequest, FakeTransport};

#[cfg(test)]
mod fake {
    use std::collections::HashMap;
    use std::io::{self, Cursor, Read};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use super::{Request, Response, Transport};
    use crate::download::policy::redact;
    use crate::error::{Error, Result};

    /// Replies scripted by URL. A URL with no reply gets a 404. Every request
    /// is recorded, so a test can tell what was never requested.
    #[derive(Debug, Default)]
    pub(crate) struct FakeTransport {
        replies: Mutex<HashMap<String, FakeReply>>,
        requests: Mutex<Vec<FakeRequest>>,
    }

    /// What [`FakeTransport`] answers for one URL.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct FakeReply {
        pub(crate) status: u16,
        pub(crate) location: Option<String>,
        pub(crate) content_length: Option<u64>,
        pub(crate) body: Vec<u8>,
        /// After `body`, the reader fails with this instead of ending.
        pub(crate) body_error: Option<io::ErrorKind>,
        /// When set, the request itself fails with this reason.
        pub(crate) fail: Option<String>,
    }

    impl FakeReply {
        /// 200 with `body` and its exact `Content-Length`.
        pub(crate) fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                location: None,
                content_length: Some(body.len() as u64),
                body,
                body_error: None,
                fail: None,
            }
        }

        /// A redirect `status` to `location`, with no body.
        pub(crate) fn redirect(status: u16, location: &str) -> Self {
            Self {
                status,
                location: Some(location.to_owned()),
                ..Self::ok(Vec::new())
            }
        }
    }

    /// One request as the fake received it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct FakeRequest {
        pub(crate) url: String,
        pub(crate) total_timeout: Option<Duration>,
    }

    impl FakeTransport {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Answers `url` with `reply` from now on.
        pub(crate) fn reply(&self, url: &str, reply: FakeReply) {
            self.replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(url.to_owned(), reply);
        }

        /// The requests so far, oldest first.
        pub(crate) fn requests(&self) -> Vec<FakeRequest> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Transport for FakeTransport {
        fn get(&self, request: &Request<'_>) -> Result<Response> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(FakeRequest {
                    url: request.url.as_str().to_owned(),
                    total_timeout: request.total_timeout,
                });
            let reply = self
                .replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(request.url.as_str())
                .cloned()
                .unwrap_or(FakeReply {
                    status: 404,
                    ..FakeReply::ok(Vec::new())
                });
            if let Some(reason) = reply.fail {
                return Err(Error::DownloadFailed {
                    url: redact(request.url),
                    reason,
                });
            }
            Ok(Response {
                status: reply.status,
                location: reply.location,
                content_length: reply.content_length,
                body: Box::new(FakeBody {
                    data: Cursor::new(reply.body),
                    error: reply.body_error,
                }),
            })
        }
    }

    struct FakeBody {
        data: Cursor<Vec<u8>>,
        error: Option<io::ErrorKind>,
    }

    impl Read for FakeBody {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match (self.data.read(buf)?, self.error) {
                (0, Some(kind)) if !buf.is_empty() => Err(io::Error::from(kind)),
                (read, _) => Ok(read),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read};

    use super::*;

    fn get(transport: &FakeTransport, url: &str) -> Response {
        let url = Url::parse(url).unwrap();
        transport
            .get(&Request {
                url: &url,
                total_timeout: Some(CATALOG_TIMEOUT),
            })
            .unwrap()
    }

    #[test]
    fn the_fake_replays_its_script_and_records_each_request() {
        let transport = FakeTransport::new();
        transport.reply("https://github.com/a", FakeReply::ok(b"abc".to_vec()));
        transport.reply(
            "https://github.com/b",
            FakeReply::redirect(302, "https://objects.githubusercontent.com/c"),
        );
        transport.reply(
            "https://github.com/stall",
            FakeReply {
                body_error: Some(ErrorKind::TimedOut),
                ..FakeReply::ok(b"x".to_vec())
            },
        );
        transport.reply(
            "https://github.com/down",
            FakeReply {
                fail: Some("connection refused".to_owned()),
                ..FakeReply::ok(Vec::new())
            },
        );

        let mut ok = get(&transport, "https://github.com/a");
        assert_eq!((ok.status, ok.content_length), (200, Some(3)));
        let mut body = Vec::new();
        ok.body.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"abc");

        let redirect = get(&transport, "https://github.com/b");
        assert_eq!(redirect.status, 302);
        assert_eq!(
            redirect.location.as_deref(),
            Some("https://objects.githubusercontent.com/c")
        );

        let mut stall = get(&transport, "https://github.com/stall");
        let mut first = [0; 8];
        assert_eq!(stall.body.read(&mut first).unwrap(), 1);
        let error = stall.body.read(&mut first).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);

        assert_eq!(get(&transport, "https://github.com/none").status, 404);

        let url = Url::parse("https://github.com/down").unwrap();
        let failed = transport.get(&Request {
            url: &url,
            total_timeout: None,
        });
        assert!(matches!(failed, Err(crate::Error::DownloadFailed { .. })));

        let request = |url: &str, total_timeout| FakeRequest {
            url: url.to_owned(),
            total_timeout,
        };
        assert_eq!(
            transport.requests(),
            [
                request("https://github.com/a", Some(CATALOG_TIMEOUT)),
                request("https://github.com/b", Some(CATALOG_TIMEOUT)),
                request("https://github.com/stall", Some(CATALOG_TIMEOUT)),
                request("https://github.com/none", Some(CATALOG_TIMEOUT)),
                request("https://github.com/down", None),
            ]
        );
    }
}
