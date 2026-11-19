//! HTTPS for the downloader: [`HttpTransport`], ck-core's
//! [`Transport`](ck_core::download::transport::Transport) on ureq and rustls.
//!
//! It holds no policy of its own. ck-core checks every URL and redirect, caps
//! the bytes and checks the hashes; this crate sends one request at a time
//! and returns the response as received. It is the only crate that links TLS
//! (`ring` compiles C).
//!
//! The agent follows no redirect, uses no proxy, keeps no cookies, asks for
//! `Accept-Encoding: identity` and decodes nothing. Certificates are checked
//! against the OS trust store (`rustls-platform-verifier`), and the `ring`
//! provider is set on this agent only, never installed process-wide.
//!
//! No error carries ureq's own message: some print the whole URL, query
//! included, and others the server's text. A failed request gives a fixed
//! reason with the URL redacted, and a failed body read keeps only its
//! [`io::ErrorKind`]. A TLS failure keeps rustls's text, escaped: it can
//! quote names from the server's certificate.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use ck_core::download::policy::{TransportPolicy, redact};
use ck_core::download::transport::{
    CONNECT_TIMEOUT, HEADERS_TIMEOUT, READ_TIMEOUT, RESOLVE_TIMEOUT, Request, Response, Transport,
};
use ck_core::error::escape_untrusted;
use ck_core::{Error, Result};
use ureq::http::HeaderMap;
use ureq::http::header::LOCATION;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, BodyReader};

/// [`Transport`] over HTTPS, and plain HTTP to loopback when the policy
/// allows it.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    agent: Agent,
}

impl HttpTransport {
    /// An agent with the timeouts of `ck_core::download::transport`, and
    /// `https_only` unless `policy` allows loopback HTTP.
    pub fn new(policy: &TransportPolicy) -> Self {
        Self::with_read_timeout(policy, READ_TIMEOUT)
    }

    /// [`HttpTransport::new`] with another limit on each read. Private, so
    /// only the tests change it: they stall a body without waiting
    /// [`READ_TIMEOUT`].
    fn with_read_timeout(policy: &TransportPolicy, read_timeout: Duration) -> Self {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = Agent::config_builder()
            .tls_config(tls)
            .https_only(!policy.allows_loopback_http())
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .accept_encoding("identity")
            .timeout_resolve(Some(RESOLVE_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(HEADERS_TIMEOUT))
            .timeout_per_read(Some(read_timeout))
            .build();
        Self {
            agent: Agent::new_with_config(config),
        }
    }
}

impl Transport for HttpTransport {
    /// One GET, as received: a redirect comes back as it is, and the body is
    /// streamed undecoded. `total_timeout` is ureq's global timeout, so it
    /// covers reading the body too.
    ///
    /// `content_length` is the length ureq frames the body by. ureq parses
    /// the header strictly (digits with optional spaces around them, and
    /// repeats must agree), and a malformed one fails the request. A chunked
    /// body, or one that ends when the connection closes, gives `None`. More
    /// than one `Location`, or one that is not visible ASCII, fails too.
    fn get(&self, request: &Request<'_>) -> Result<Response> {
        let url = request.url;
        let failed = |why: &str| Error::DownloadFailed {
            url: redact(url),
            reason: why.to_owned(),
        };
        // ureq sends a user name or password in the URL as an `Authorization`
        // header. The policy refuses such URLs before they get here.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(failed("the URL carries a user name or password"));
        }

        let mut builder = self.agent.get(url.as_str());
        if let Some(total) = request.total_timeout {
            builder = builder.config().timeout_global(Some(total)).build();
        }
        let response = builder.call().map_err(|e| failed(&reason(&e)))?;
        let status = response.status().as_u16();
        let location = location(response.headers()).map_err(failed)?;
        let body = response.into_body();
        Ok(Response {
            status,
            location,
            content_length: body.content_length(),
            body: Box::new(ResponseBody(body.into_reader())),
        })
    }
}

/// The `Location` header: at most one, in visible ASCII.
fn location(headers: &HeaderMap) -> std::result::Result<Option<String>, &'static str> {
    let mut values = headers.get_all(LOCATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("the server sent more than one Location");
    }
    value
        .to_str()
        .map(|value| Some(value.to_owned()))
        .map_err(|_| "the server sent a Location that is not visible ASCII")
}

/// Why a request failed, in fixed words. `RequireHttpsOnly` and `BadUri`
/// print the whole URL, and some `Protocol` errors print header values.
fn reason(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(phase) => format!("it timed out ({phase})"),
        ureq::Error::Io(error) => match tls_error(error) {
            Some(tls) => tls_failed(tls),
            None => error.kind().to_string(),
        },
        ureq::Error::Rustls(tls) => tls_failed(tls),
        ureq::Error::Tls(what) => tls_failed(what),
        ureq::Error::HostNotFound => "the host name did not resolve".to_owned(),
        ureq::Error::RequireHttpsOnly(_) => "only HTTPS is allowed".to_owned(),
        ureq::Error::Http(_) | ureq::Error::BadUri(_) => "the URL cannot be sent".to_owned(),
        ureq::Error::Protocol(_) | ureq::Error::LargeResponseHeader(..) => {
            "the response is not valid HTTP/1.1".to_owned()
        }
        _ => "the request failed".to_owned(),
    }
}

/// A certificate name in the text is the server's choice.
fn tls_failed(error: &dyn std::fmt::Display) -> String {
    format!("TLS failed: {}", escape_untrusted(&error.to_string()))
}

/// The rustls error inside an io error: rustls reports a failed handshake as
/// `InvalidData` wrapping its own error, which names the TLS problem and
/// never the URL.
fn tls_error(error: &io::Error) -> Option<&rustls::Error> {
    error.get_ref()?.downcast_ref::<rustls::Error>()
}

/// The body as ck-core reads it. A failed read keeps only its kind, and a
/// ureq timeout, which ureq reports as `Other`, becomes `TimedOut`.
struct ResponseBody(BodyReader<'static>);

impl Read for ResponseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .read(buf)
            .map_err(|error| io::Error::from(read_error_kind(error)))
    }
}

fn read_error_kind(error: io::Error) -> io::ErrorKind {
    // Unwraps a ureq error that ureq put inside the io error.
    match ureq::Error::from(error) {
        ureq::Error::Timeout(_) => io::ErrorKind::TimedOut,
        ureq::Error::Io(error) => error.kind(),
        _ => io::ErrorKind::InvalidData,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;
