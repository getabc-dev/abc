//! Which tailnet node a request came from, asked of abc's own tailscaled.
//!
//! Serve tells the server the person (`Tailscale-User-Login`) and the address it accepted the
//! connection from (`X-Forwarded-For`, the last entry). The node is not in a header: the server
//! asks its own tailscaled's LocalAPI, `GET /localapi/v0/whois?addr=<ip>:0`, over the Unix socket
//! `abc server setup` passes (`--tailscaled-socket`), so the node name is the tailnet's own and
//! no client can claim another's. The answer's login must be the one Serve asserted.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

use abc_core::identity::{TailnetLogin, TailnetNode};

/// How long one LocalAPI call may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The largest answer read: a whois answer is a few KiB.
const MAX_ANSWER: u64 = 256 * 1024;

/// How the server finds out which node a request came from.
#[derive(Debug, Clone)]
pub(crate) enum Whois {
    /// Ask the tailscaled that listens on this socket.
    Tailscaled(PathBuf),
    /// No socket was given: nodes cannot be named, so browser enrolment is off.
    Off,
}

/// Why a node could not be named.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WhoisError {
    /// The server has no tailscaled socket.
    #[error("the server was started without --tailscaled-socket")]
    Off,
    /// The forwarded address is not an IP address.
    #[error("the forwarded source address is not an IP address")]
    BadAddress,
    /// tailscaled did not answer, or answered with an error.
    #[error("abc's tailscaled could not identify the node: {0}")]
    Failed(String),
    /// tailscaled named a person other than the one Serve asserted.
    #[error("abc's tailscaled says the node belongs to another login than Serve asserted")]
    LoginMismatch,
}

/// The part of tailscaled's whois answer that matters.
#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "Node")]
    node: Option<NodeAnswer>,
    #[serde(rename = "UserProfile")]
    user: Option<UserAnswer>,
}

#[derive(Deserialize)]
struct NodeAnswer {
    #[serde(rename = "Name")]
    name: String,
}

#[derive(Deserialize)]
struct UserAnswer {
    #[serde(rename = "LoginName")]
    login: String,
}

impl Whois {
    /// The node `source` (an IP address as Serve forwarded it) is, if it belongs to `login`.
    pub(crate) async fn node(
        &self,
        source: &str,
        login: &TailnetLogin,
    ) -> Result<TailnetNode, WhoisError> {
        let Self::Tailscaled(socket) = self else {
            return Err(WhoisError::Off);
        };
        let ip: IpAddr = source.parse().map_err(|_| WhoisError::BadAddress)?;
        let target = match ip {
            IpAddr::V4(ip) => format!("{ip}:0"),
            IpAddr::V6(ip) => format!("[{ip}]:0"),
        };
        let body = tokio::time::timeout(TIMEOUT, local_api(socket, &target))
            .await
            .map_err(|_| WhoisError::Failed("tailscaled did not answer in time".to_owned()))?
            .map_err(WhoisError::Failed)?;
        let answer: Answer = serde_json::from_slice(&body)
            .map_err(|_| WhoisError::Failed("tailscaled's answer was not understood".to_owned()))?;
        let name = answer
            .node
            .ok_or_else(|| WhoisError::Failed("tailscaled knows no such node".to_owned()))?
            .name;
        let node = TailnetNode::parse(&name)
            .map_err(|_| WhoisError::Failed("tailscaled named the node oddly".to_owned()))?;
        let asserted = answer.user.map(|user| user.login);
        if asserted.as_deref().map(str::to_ascii_lowercase) == Some(login.as_str().to_owned()) {
            Ok(node)
        } else {
            Err(WhoisError::LoginMismatch)
        }
    }
}

/// `GET /localapi/v0/whois?addr=<target>` over `socket`, answered with its 200 body.
async fn local_api(socket: &Path, target: &str) -> Result<Vec<u8>, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let request = format!(
        "GET /localapi/v0/whois?addr={target} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut answer = Vec::new();
    stream
        .take(MAX_ANSWER)
        .read_to_end(&mut answer)
        .await
        .map_err(|error| error.to_string())?;
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("the answer has no header")?;
    let head = std::str::from_utf8(&answer[..split]).map_err(|_| "the answer is not text")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("the answer has no status")?;
    if status != 200 {
        return Err(format!("tailscaled answered {status}"));
    }
    Ok(answer.split_off(split + 4))
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    /// A stand-in for tailscaled's LocalAPI that answers every call with `reply`, and returns the
    /// request line it saw.
    async fn serve_once(socket: &Path, reply: &'static str) -> tokio::task::JoinHandle<String> {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = vec![0; 1024];
            let read = stream.read(&mut seen).await.unwrap();
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            String::from_utf8_lossy(&seen[..read])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    }

    fn login() -> TailnetLogin {
        TailnetLogin::parse("alice@example.com").unwrap()
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
        {\"Node\":{\"Name\":\"MacBook.tail1234.ts.net.\"},\
         \"UserProfile\":{\"LoginName\":\"Alice@example.com\"}}";

    #[tokio::test]
    async fn names_the_node_tailscaled_says_the_address_is() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("t.sock");
        let seen = serve_once(&socket, OK).await;
        let node = Whois::Tailscaled(socket)
            .node("100.64.0.7", &login())
            .await
            .unwrap();
        assert_eq!(
            node.as_str(),
            "macbook.tail1234.ts.net",
            "the MagicDNS name"
        );
        assert_eq!(
            seen.await.unwrap(),
            "GET /localapi/v0/whois?addr=100.64.0.7:0 HTTP/1.0",
            "asked about the forwarded address"
        );
    }

    #[tokio::test]
    async fn a_v6_address_is_bracketed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("t.sock");
        let seen = serve_once(&socket, OK).await;
        Whois::Tailscaled(socket)
            .node("fd7a:115c:a1e0::1", &login())
            .await
            .unwrap();
        assert_eq!(
            seen.await.unwrap(),
            "GET /localapi/v0/whois?addr=[fd7a:115c:a1e0::1]:0 HTTP/1.0",
            "ip:port form"
        );
    }

    #[tokio::test]
    async fn refuses_what_it_cannot_attribute() {
        let dir = tempfile::tempdir().unwrap();
        let other = TailnetLogin::parse("mallory@example.com").unwrap();
        let socket = dir.path().join("a.sock");
        serve_once(&socket, OK).await;
        assert_eq!(
            Whois::Tailscaled(socket).node("100.64.0.7", &other).await,
            Err(WhoisError::LoginMismatch),
            "another person's node"
        );
        for (name, reply) in [
            ("404", "HTTP/1.1 404 Not Found\r\n\r\nno"),
            ("no node", "HTTP/1.1 200 OK\r\n\r\n{\"UserProfile\":null}"),
            ("garbage", "HTTP/1.1 200 OK\r\n\r\nnot json"),
            ("no header", "oops"),
        ] {
            let socket = dir.path().join(format!("{name}.sock").replace(' ', "-"));
            serve_once(&socket, reply).await;
            assert!(
                matches!(
                    Whois::Tailscaled(socket).node("100.64.0.7", &login()).await,
                    Err(WhoisError::Failed(_))
                ),
                "{name}"
            );
        }
        assert!(
            matches!(
                Whois::Tailscaled(dir.path().join("none.sock"))
                    .node("100.64.0.7", &login())
                    .await,
                Err(WhoisError::Failed(_))
            ),
            "no tailscaled"
        );
        assert_eq!(
            Whois::Tailscaled(dir.path().join("x.sock"))
                .node("100.64.0.7 HTTP/1.1\r\nX: y", &login())
                .await,
            Err(WhoisError::BadAddress),
            "a source that is no address is never sent"
        );
        assert_eq!(
            Whois::Off.node("100.64.0.7", &login()).await,
            Err(WhoisError::Off),
            "off"
        );
    }
}
