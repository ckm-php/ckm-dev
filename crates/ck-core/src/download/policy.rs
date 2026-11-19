//! Which URLs a download may request (D3, D6).
//!
//! - **Release, and debug in the standard profile:** HTTPS only, with no
//!   userinfo and no explicit port. The first request goes to
//!   [`FIRST_HOP_HOSTS`]; each redirect goes to [`REDIRECT_HOSTS`]. At most
//!   [`MAX_REDIRECTS`] redirects.
//! - **Debug with an isolated profile:** also `http://127.0.0.1:<port>` and
//!   `http://[::1]:<port>`, for the loopback test server. Literal addresses
//!   only, never `localhost`.
//!
//! Hosts compare exactly, never by suffix: `github.com.evil.com` is refused.
//! A redirect from HTTPS to HTTP is always refused.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

use crate::error::{Error, Result};
use crate::paths::{BuildKind, Profile};

/// The most redirects one download follows.
pub const MAX_REDIRECTS: usize = 5;

/// Hosts a download may start at: GitHub Releases (D6).
pub const FIRST_HOP_HOSTS: &[&str] = &["github.com"];

/// Hosts GitHub Releases redirects to.
pub const REDIRECT_HOSTS: &[&str] = &[
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// The URL rules for this run. [`TransportPolicy::current`] is the only way
/// app code gets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPolicy {
    loopback_http: bool,
}

impl TransportPolicy {
    /// The rules for this binary and `profile`: the single gate.
    pub fn current(profile: Profile) -> Self {
        Self::for_build(BuildKind::current(), profile)
    }

    /// [`TransportPolicy::current`] with the build kind as an input, so unit
    /// tests can check the release rules from a debug build.
    pub(crate) fn for_build(build: BuildKind, profile: Profile) -> Self {
        Self {
            loopback_http: build == BuildKind::Debug && profile == Profile::Isolated,
        }
    }

    /// Whether plain HTTP to the loopback addresses is allowed: debug builds
    /// in an isolated profile only. ck-net sets `https_only` from it.
    pub fn allows_loopback_http(&self) -> bool {
        self.loopback_http
    }

    /// Checks the first URL of a download, before it is requested.
    /// [`Error::UrlRefused`] when it breaks a rule.
    pub fn check_first(&self, url: &Url) -> Result<()> {
        self.check(url, FIRST_HOP_HOSTS)
    }

    /// Checks a redirect from `from` to `to`, before `to` is requested.
    /// [`Error::UrlRefused`] when it breaks a rule, including HTTPS to HTTP.
    pub fn check_redirect(&self, from: &Url, to: &Url) -> Result<()> {
        if from.scheme() == "https" && to.scheme() != "https" {
            return Err(refused(
                to,
                format!("a redirect from {} must stay on HTTPS", redact(from)),
            ));
        }
        self.check(to, REDIRECT_HOSTS)
    }

    /// The rules every hop shares; `hosts` is the HTTPS allow-list for this hop.
    fn check(&self, url: &Url, hosts: &[&str]) -> Result<()> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refused(
                url,
                "it carries a user name or password".to_owned(),
            ));
        }
        match url.scheme() {
            "https" => {
                // `Url` drops the default port, so `:443` is not seen here.
                if url.port().is_some() {
                    return Err(refused(url, "it names a port other than 443".to_owned()));
                }
                match url.host() {
                    Some(Host::Domain(host)) if hosts.contains(&host) => Ok(()),
                    _ => Err(refused(
                        url,
                        format!("its host is not one of: {}", hosts.join(", ")),
                    )),
                }
            }
            "http" if self.loopback_http => {
                if !is_loopback_literal(url) {
                    return Err(refused(
                        url,
                        "plain HTTP goes only to 127.0.0.1 or [::1]".to_owned(),
                    ));
                }
                if url.port().is_none() {
                    return Err(refused(
                        url,
                        "plain HTTP to loopback needs an explicit port".to_owned(),
                    ));
                }
                Ok(())
            }
            _ => Err(refused(url, "only HTTPS is allowed".to_owned())),
        }
    }
}

/// `url` as `scheme://host[:port]/path`, without userinfo, query or fragment,
/// which can carry signed tokens. Every error and log line names a URL this
/// way.
pub fn redact(url: &Url) -> String {
    let mut redacted = format!("{}:", url.scheme());
    if url.has_authority() {
        redacted.push_str("//");
        redacted.push_str(url.host_str().unwrap_or_default());
        if let Some(port) = url.port() {
            let _ = write!(redacted, ":{port}");
        }
    }
    // The opaque path of a URL such as `data:` is its content, not a path.
    if !url.cannot_be_a_base() {
        redacted.push_str(url.path());
    }
    redacted
}

/// The literal loopback addresses only: never a name such as `localhost`, and
/// never another 127.x address.
fn is_loopback_literal(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(address)) => address == Ipv6Addr::LOCALHOST,
        _ => false,
    }
}

fn refused(url: &Url, reason: String) -> Error {
    Error::UrlRefused {
        url: redact(url),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_RUN: [(BuildKind, Profile); 4] = [
        (BuildKind::Release, Profile::Standard),
        (BuildKind::Release, Profile::Isolated),
        (BuildKind::Debug, Profile::Standard),
        (BuildKind::Debug, Profile::Isolated),
    ];

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn assert_refused(result: Result<()>, case: &str) {
        assert!(
            matches!(result, Err(Error::UrlRefused { .. })),
            "{case} should be refused, got {result:?}"
        );
    }

    #[test]
    fn the_first_hop_goes_to_github_and_loopback_http_only_in_a_debug_test_profile() {
        let allowed = [
            "https://github.com/o/r/releases/download/v1/php.tar.gz",
            "https://GitHub.COM/o/r/releases/download/v1/php.tar.gz",
            "https://github.com/o/r/releases/download/v1/php.tar.gz?x=1#y",
            // `Url` drops the default port: this is the same URL as above.
            "https://github.com:443/o/r/releases/download/v1/php.tar.gz",
        ];
        let refused = [
            "https://github.com.evil.com/o/r",
            "https://evil.com/github.com/o/r",
            "https://evilgithub.com/o/r",
            "https://api.github.com/o/r",
            "https://github.com./o/r",
            "https://github.com@evil.com/o/r",
            "https://user@github.com/o/r",
            "https://user:secret@github.com/o/r",
            "https://:secret@github.com/o/r",
            "https://github.com:8443/o/r",
            "http://github.com/o/r",
            "ftp://github.com/o/r",
            "file:///etc/passwd",
            // A redirect host is not a first hop.
            "https://release-assets.githubusercontent.com/o/r",
            "https://objects.githubusercontent.com/o/r",
            // Loopback is plain HTTP to a literal address with a port, or nothing.
            "http://localhost:8080/c",
            "http://LOCALHOST:8080/c",
            "http://127.0.0.2:8080/c",
            "http://0.0.0.0:8080/c",
            "http://[::ffff:127.0.0.1]:8080/c",
            "https://127.0.0.1:8080/c",
            "https://[::1]:8080/c",
            "http://user@127.0.0.1:8080/c",
            "http://127.0.0.1/c",
        ];
        let loopback = ["http://127.0.0.1:8080/c", "http://[::1]:8080/c"];

        for (build, profile) in EVERY_RUN {
            let policy = TransportPolicy::for_build(build, profile);
            let test_profile = build == BuildKind::Debug && profile == Profile::Isolated;
            assert_eq!(
                policy.allows_loopback_http(),
                test_profile,
                "{build:?} {profile:?}"
            );
            for case in allowed {
                assert!(
                    policy.check_first(&url(case)).is_ok(),
                    "{case} in {build:?} {profile:?}"
                );
            }
            for case in refused {
                assert_refused(
                    policy.check_first(&url(case)),
                    &format!("{case} in {build:?} {profile:?}"),
                );
            }
            for case in loopback {
                let result = policy.check_first(&url(case));
                if test_profile {
                    assert!(
                        result.is_ok(),
                        "{case} in {build:?} {profile:?}: {result:?}"
                    );
                } else {
                    assert_refused(result, &format!("{case} in {build:?} {profile:?}"));
                }
            }
        }
    }

    #[test]
    fn redirects_go_to_the_asset_hosts_and_never_from_https_to_http() {
        let github = url("https://github.com/o/r/releases/download/v1/php.tar.gz");
        let allowed = [
            "https://release-assets.githubusercontent.com/a/1?sig=x",
            "https://objects.githubusercontent.com/a/1?sig=x",
        ];
        let refused = [
            "https://github.com/o/r2/releases/download/v1/php.tar.gz",
            "https://githubusercontent.com/a/1",
            "https://evil-objects.githubusercontent.com/a/1",
            "https://objects.githubusercontent.com.evil.com/a/1",
            "https://user@objects.githubusercontent.com/a/1",
            "https://objects.githubusercontent.com:8443/a/1",
            "http://objects.githubusercontent.com/a/1",
            // From HTTPS, even to loopback in the debug test profile.
            "http://127.0.0.1:8080/a/1",
            "http://[::1]:8080/a/1",
        ];
        for (build, profile) in EVERY_RUN {
            let policy = TransportPolicy::for_build(build, profile);
            for case in allowed {
                assert!(
                    policy.check_redirect(&github, &url(case)).is_ok(),
                    "{case} in {build:?} {profile:?}"
                );
            }
            for case in refused {
                assert_refused(
                    policy.check_redirect(&github, &url(case)),
                    &format!("{case} in {build:?} {profile:?}"),
                );
            }
        }

        // The loopback test server may redirect to itself, in the debug test
        // profile only.
        let local = url("http://127.0.0.1:8080/catalog");
        for (build, profile) in EVERY_RUN {
            let policy = TransportPolicy::for_build(build, profile);
            for case in ["http://127.0.0.1:9090/c", "http://[::1]:8080/c"] {
                let result = policy.check_redirect(&local, &url(case));
                if build == BuildKind::Debug && profile == Profile::Isolated {
                    assert!(
                        result.is_ok(),
                        "{case} in {build:?} {profile:?}: {result:?}"
                    );
                } else {
                    assert_refused(result, &format!("{case} in {build:?} {profile:?}"));
                }
            }
            assert_refused(
                policy.check_redirect(&local, &url("http://localhost:8080/c")),
                &format!("localhost in {build:?} {profile:?}"),
            );
        }
    }

    #[test]
    fn a_redacted_url_keeps_only_the_scheme_host_port_and_path() {
        let cases = [
            (
                "https://user:secret@objects.githubusercontent.com/a/php.tar.gz?sig=secret#secret",
                "https://objects.githubusercontent.com/a/php.tar.gz",
            ),
            ("http://[::1]:8080/c?token=secret", "http://[::1]:8080/c"),
            ("data:text/plain,secret", "data:"),
        ];
        for (full, redacted) in cases {
            assert_eq!(redact(&url(full)), redacted);
        }

        let error = TransportPolicy::for_build(BuildKind::Release, Profile::Standard)
            .check_first(&url("https://evil.com/c?token=secret#secret"))
            .unwrap_err();
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("secret"), "{text}");
        }
    }
}
