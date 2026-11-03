//! The server's public address: the https origin Tailscale Serve answers on.
//!
//! Both ends name the same URL. The server checks every forwarded host against it
//! (`ABC_PUBLIC_URL`), and the worker enrols with it, keeps it beside its key, and dials it and no
//! other address, verifying that the certificate names its host before it sends the key.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An `https://host[:port]` origin with no path, query, fragment or credentials, lower-cased, with
/// the default port left out. Serialized as the origin; deserializing parses it again.
///
/// ```
/// use abc_protocol::url::PublicUrl;
///
/// let url = PublicUrl::parse("https://ABC.tail1234.ts.net/")?;
/// assert_eq!(url.origin(), "https://abc.tail1234.ts.net");
/// assert_eq!((url.name(), url.port()), ("abc.tail1234.ts.net", 443));
/// assert_eq!(url.endpoint("/api/enrol"), "https://abc.tail1234.ts.net/api/enrol");
/// # Ok::<(), abc_protocol::url::InvalidPublicUrl>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct PublicUrl {
    /// `https://` + `host`.
    origin: String,
    /// `name`, then `:port` unless the port is 443.
    host: String,
    /// The host name alone, lower-cased.
    name: String,
    /// The port, 443 unless given.
    port: u16,
}

/// Why text is not a public URL. The message names the rule, never the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{reason}")]
pub struct InvalidPublicUrl {
    /// The rule it broke, such as "it must start with https://".
    pub reason: &'static str,
}

/// The port https uses when a URL names none.
const HTTPS_PORT: u16 = 443;

impl PublicUrl {
    /// Parses an https origin. A trailing slash is allowed; the scheme and host are lower-cased.
    ///
    /// # Errors
    ///
    /// [`InvalidPublicUrl`] naming the broken rule.
    pub fn parse(raw: &str) -> Result<Self, InvalidPublicUrl> {
        let refuse = |reason| InvalidPublicUrl { reason };
        let lowered = raw.trim().to_ascii_lowercase();
        let rest = lowered
            .strip_prefix("https://")
            .ok_or_else(|| refuse("it must start with https://"))?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#']) {
            return Err(refuse("it must not have a path, query or fragment"));
        }
        if authority.contains('@') {
            return Err(refuse("it must not carry credentials"));
        }
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) => (
                name,
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| refuse("its port must be a number from 1 to 65535"))?,
            ),
            None => (authority, HTTPS_PORT),
        };
        let valid_name = !name.is_empty()
            && name.len() <= 253
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
        if !valid_name {
            return Err(refuse("it must name a host"));
        }
        let host = if port == HTTPS_PORT {
            name.to_owned()
        } else {
            format!("{name}:{port}")
        };
        Ok(Self {
            origin: format!("https://{host}"),
            host,
            name: name.to_owned(),
            port,
        })
    }

    /// `https://host[:port]`: what browsers send in `Origin`.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `host[:port]`, the port only when it is not 443: what clients send in `Host`, and what
    /// Serve forwards in `X-Forwarded-Host`.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The host name alone: what a client resolves and what the server's certificate must name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The TCP port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The URL of `path` on this server, such as [`crate::enrol::ENROL_PATH`].
    #[must_use]
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
}

impl fmt::Display for PublicUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

impl From<PublicUrl> for String {
    fn from(url: PublicUrl) -> Self {
        url.origin
    }
}

impl TryFrom<String> for PublicUrl {
    type Error = InvalidPublicUrl;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_url_is_an_https_origin() {
        let url = PublicUrl::parse("https://ABC.tail1234.ts.net/").unwrap();
        assert_eq!(
            url.origin(),
            "https://abc.tail1234.ts.net",
            "lower-cased, no slash"
        );
        assert_eq!(url.host(), "abc.tail1234.ts.net", "the Host clients send");
        assert_eq!(url.name(), "abc.tail1234.ts.net", "the name to resolve");
        assert_eq!(url.port(), 443, "https's port");
        assert_eq!(url.to_string(), url.origin(), "shown as its origin");
        let default_port = PublicUrl::parse("HTTPS://abc.ts.net:443").unwrap();
        assert_eq!(default_port.host(), "abc.ts.net", "443 is implied");
        assert_eq!(
            default_port,
            PublicUrl::parse("https://abc.ts.net").unwrap(),
            "and the same URL as leaving it out"
        );
        let port = PublicUrl::parse(" https://abc.ts.net:8443 ").unwrap();
        assert_eq!(port.host(), "abc.ts.net:8443", "another port is kept");
        assert_eq!(port.name(), "abc.ts.net", "apart from the name");
        assert_eq!(port.port(), 8443, "and parsed");
        assert_eq!(
            port.origin(),
            "https://abc.ts.net:8443",
            "in the origin too"
        );
        assert_eq!(
            port.endpoint("/api/worker/connect"),
            "https://abc.ts.net:8443/api/worker/connect",
            "an endpoint is the origin and the path"
        );
    }

    #[test]
    fn anything_else_is_refused_by_rule() {
        for (raw, reason) in [
            ("http://abc.ts.net", "it must start with https://"),
            ("abc.ts.net", "it must start with https://"),
            (
                "https://abc.ts.net/app",
                "it must not have a path, query or fragment",
            ),
            (
                "https://abc.ts.net?x",
                "it must not have a path, query or fragment",
            ),
            (
                "https://abc.ts.net#x",
                "it must not have a path, query or fragment",
            ),
            ("https://me:pw@abc.ts.net", "it must not carry credentials"),
            ("https://", "it must name a host"),
            ("https://:443", "it must name a host"),
            ("https://ab c.ts.net", "it must name a host"),
            ("https://abc_x.ts.net", "it must name a host"),
            (
                "https://abc.ts.net:99999",
                "its port must be a number from 1 to 65535",
            ),
            (
                "https://abc.ts.net:0",
                "its port must be a number from 1 to 65535",
            ),
            (
                "https://abc.ts.net:",
                "its port must be a number from 1 to 65535",
            ),
        ] {
            assert_eq!(
                PublicUrl::parse(raw),
                Err(InvalidPublicUrl { reason }),
                "{raw}"
            );
        }
        let long = format!("https://{}", "a".repeat(254));
        assert!(PublicUrl::parse(&long).is_err(), "a name over 253 bytes");
        assert!(
            PublicUrl::parse(&format!("https://{}", "a".repeat(253))).is_ok(),
            "253 bytes is the limit"
        );
        assert_eq!(
            InvalidPublicUrl {
                reason: "it must name a host"
            }
            .to_string(),
            "it must name a host",
            "the message is the rule"
        );
    }

    #[test]
    fn it_travels_as_its_origin() {
        let url = PublicUrl::parse("https://abc.ts.net:8443").unwrap();
        let json = serde_json::to_string(&url).unwrap();
        assert_eq!(json, r#""https://abc.ts.net:8443""#, "the origin");
        assert_eq!(
            serde_json::from_str::<PublicUrl>(&json).unwrap(),
            url,
            "and reads back"
        );
        let error = serde_json::from_str::<PublicUrl>(r#""http://secret-host""#).unwrap_err();
        assert!(
            !error.to_string().contains("secret-host"),
            "a refusal never quotes the input: {error}"
        );
    }
}
