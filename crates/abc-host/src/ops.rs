//! Asking abc server whether it is ready, on its ops listener (`GET /readyz`).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{HostError, Timing};

/// What `/readyz` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// 200: serving.
    Ready,
    /// It answered, but is not serving: its phase, and why if it said.
    NotReady(String),
    /// Nothing answered.
    Unreachable(String),
}

/// The body `/readyz` answers with.
#[derive(Debug, Deserialize)]
struct Body {
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Asks `addr`'s `/readyz` once.
pub(crate) fn probe(addr: SocketAddr) -> Readiness {
    let timeout = Duration::from_secs(2);
    let answer = (|| -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream
            .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer)?;
        Ok(answer)
    })();
    match answer {
        Ok(answer) => read_answer(&String::from_utf8_lossy(&answer)),
        Err(error) => Readiness::Unreachable(error.to_string()),
    }
}

/// Reads an HTTP answer to `/readyz`.
fn read_answer(answer: &str) -> Readiness {
    let ok = answer
        .lines()
        .next()
        .is_some_and(|status| status.split(' ').nth(1) == Some("200"));
    if ok {
        return Readiness::Ready;
    }
    let body = answer
        .find('{')
        .zip(answer.rfind('}'))
        .and_then(|(start, end)| answer.get(start..=end))
        .and_then(|json| serde_json::from_str::<Body>(json).ok());
    Readiness::NotReady(match body {
        Some(Body {
            status,
            reason: Some(reason),
        }) => format!("{status}: {reason}"),
        Some(Body {
            status,
            reason: None,
        }) => status,
        None => answer.lines().next().unwrap_or_default().to_owned(),
    })
}

/// Waits until `/readyz` answers 200.
pub(crate) fn wait_ready(addr: SocketAddr, timing: &Timing, fix: &str) -> Result<(), HostError> {
    let start = Instant::now();
    loop {
        let readiness = probe(addr);
        if readiness == Readiness::Ready {
            return Ok(());
        }
        if start.elapsed() >= timing.server {
            let last = match readiness {
                Readiness::NotReady(said) => format!("it said {said}"),
                Readiness::Unreachable(error) => format!("nothing answered on {addr}: {error}"),
                Readiness::Ready => String::new(),
            };
            return Err(HostError::Timeout {
                what: format!("abc server reporting ready ({last})"),
                seconds: timing.server.as_secs(),
                fix: fix.to_owned(),
            });
        }
        std::thread::sleep(timing.poll);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::{Readiness, probe, read_answer, wait_ready};
    use crate::{HostError, Timing};

    #[test]
    fn answers_are_read_for_status_and_reason() {
        assert_eq!(
            read_answer("HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ready\"}"),
            Readiness::Ready,
            "200"
        );
        assert_eq!(
            read_answer(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 9\r\n\r\n{\"status\":\"degraded\",\"reason\":\"no db\"}"
            ),
            Readiness::NotReady("degraded: no db".to_owned()),
            "the phase and reason"
        );
        assert_eq!(
            read_answer("HTTP/1.1 503 Service Unavailable\r\n\r\n{\"status\":\"migrating\"}"),
            Readiness::NotReady("migrating".to_owned()),
            "the phase"
        );
        assert_eq!(
            read_answer("HTTP/1.1 404 Not Found\r\n\r\nnope"),
            Readiness::NotReady("HTTP/1.1 404 Not Found".to_owned()),
            "something else"
        );
    }

    fn answering(answer: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0_u8; 512];
                let read = stream.read(&mut request).unwrap();
                assert!(
                    request[..read].starts_with(b"GET /readyz HTTP/1.1\r\n"),
                    "asks /readyz"
                );
                stream.write_all(answer.as_bytes()).unwrap();
            }
        });
        addr
    }

    fn quick() -> Timing {
        Timing {
            poll: Duration::from_millis(10),
            server: Duration::from_millis(100),
            ..Timing::production()
        }
    }

    #[test]
    fn a_ready_server_is_waited_for_and_an_unready_one_times_out_saying_why() {
        let ready = answering("HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ready\"}");
        assert_eq!(probe(ready), Readiness::Ready, "ready");
        wait_ready(ready, &quick(), "f").unwrap();
        let starting = answering("HTTP/1.1 503 x\r\n\r\n{\"status\":\"starting\"}");
        let error = wait_ready(starting, &quick(), "read its log").unwrap_err();
        assert!(
            matches!(&error, HostError::Timeout { what, fix, .. }
                if what.contains("it said starting") && fix == "read its log"),
            "{error:?}"
        );
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(
            matches!(probe(closed), Readiness::Unreachable(_)),
            "nothing listening"
        );
        let unreachable = wait_ready(closed, &quick(), "f").unwrap_err();
        assert!(
            unreachable.to_string().contains("nothing answered on"),
            "{unreachable}"
        );
    }
}
