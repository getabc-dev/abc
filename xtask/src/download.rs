//! Downloads that never hang and lose nothing to a reset, for `cargo xtask setup`, `pin` and the
//! release's Go archive.
//!
//! A network that drops mid-download hung `setup` forever, and an agent VM's network resets long
//! GitHub release downloads. So every download here has a connect timeout and a stall limit: a
//! read that waits longer than [`Patience::stall`], or less than [`Patience::floor`] bytes in
//! [`Patience::window`], ends the attempt. A failed attempt that may pass on its own (a timeout, a
//! reset, a body that ended early, or a 408, 429 or 5xx answer) is tried again after the next
//! delay of [`Patience::retries`], resuming from the bytes already received with a `Range`
//! request. An answer that does not resume where asked starts over. Any other failure, such as a
//! 404, ends the download at once.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::anyhow;

/// How long a download may wait, and how often it tries again.
#[derive(Debug, Clone)]
pub(crate) struct Patience {
    /// The longest a connection may take to open.
    pub(crate) connect: Duration,
    /// The longest one read of the answer may wait for a byte.
    pub(crate) stall: Duration,
    /// The fewest bytes each [`Patience::window`] must bring once the body is flowing.
    pub(crate) floor: u64,
    pub(crate) window: Duration,
    /// The wait before each retry; their number is the number of retries.
    pub(crate) retries: Vec<Duration>,
}

/// The patience of real downloads: release archives of up to a few hundred megabytes from GitHub
/// and the other hosts `xtask/tools.toml` names. Five retries, doubling from 2 s, ride out about a
/// minute of outage.
pub(crate) fn patience() -> Patience {
    Patience {
        connect: Duration::from_secs(15),
        stall: Duration::from_secs(30),
        floor: 16 * 1024,
        window: Duration::from_secs(60),
        retries: [2, 4, 8, 16, 32]
            .into_iter()
            .map(Duration::from_secs)
            .collect(),
    }
}

/// Why one attempt failed.
enum Failure {
    /// It may pass when tried again.
    Transient(anyhow::Error),
    /// It will not.
    Final(anyhow::Error),
}

/// Downloads `url`, at most `limit` bytes, with `patience`, waiting between attempts with `sleep`
/// and saying each retry in `notes`.
pub(crate) fn fetch(
    url: &str,
    limit: u64,
    patience: &Patience,
    sleep: &mut dyn FnMut(Duration),
    notes: &mut dyn Write,
) -> anyhow::Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(patience.connect))
        .timeout_per_read(Some(patience.stall))
        .build()
        .into();
    let mut body = Vec::new();
    // Retries in a row that brought nothing new: an attempt that adds to the bytes kept starts
    // the count again, so a long download on a network that resets often still finishes.
    let mut idle = 0_usize;
    loop {
        let kept = body.len();
        let error = match attempt(&agent, url, limit, patience, &mut body) {
            Ok(()) => return Ok(body),
            Err(Failure::Final(error)) => {
                return Err(error.context(format!("could not download {url}")));
            }
            Err(Failure::Transient(error)) => error,
        };
        if body.len() > kept {
            idle = 0;
        }
        let Some(&delay) = patience.retries.get(idle) else {
            return Err(error.context(format!(
                "could not download {url}: gave up after {} attempts in a row that brought \
                 nothing new",
                patience.retries.len().saturating_add(1)
            )));
        };
        idle = idle.saturating_add(1);
        writeln!(
            notes,
            "download: {url}: {error:#}; {} bytes kept, resuming in {} s",
            body.len(),
            delay.as_secs_f64()
        )?;
        sleep(delay);
    }
}

/// One attempt at `url`, adding to `body` what it receives: from the start when `body` is empty
/// or the server does not resume, and from where `body` ends otherwise.
fn attempt(
    agent: &ureq::Agent,
    url: &str,
    limit: u64,
    patience: &Patience,
    body: &mut Vec<u8>,
) -> Result<(), Failure> {
    let mut request = agent.get(url).header("Accept-Encoding", "identity");
    let from = u64::try_from(body.len()).map_err(|error| Failure::Final(error.into()))?;
    if from > 0 {
        request = request.header("Range", &format!("bytes={from}-"));
    }
    let response = request.call().map_err(|error| {
        // ureq's errors are non-exhaustive: any but these is final.
        let transient = if let ureq::Error::StatusCode(code) = &error {
            retried_status(*code)
        } else {
            matches!(
                error,
                ureq::Error::Timeout(_)
                    | ureq::Error::Io(_)
                    | ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::Protocol(_)
            )
        };
        let error = if let ureq::Error::StatusCode(code) = error {
            anyhow!("the server answered {code}")
        } else {
            error.into()
        };
        if transient {
            Failure::Transient(error)
        } else {
            Failure::Final(error)
        }
    })?;
    let total = match response.status().as_u16() {
        206 if from > 0 => {
            let range = response
                .headers()
                .get("Content-Range")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            match content_range(range) {
                Some((start, total)) if start == from => total,
                _ => {
                    body.clear();
                    return Err(Failure::Transient(anyhow!(
                        "the server resumed with `Content-Range: {range}`, not from byte {from}; \
                         starting over"
                    )));
                }
            }
        }
        200 => {
            body.clear();
            response.body().content_length()
        }
        status => {
            body.clear();
            return Err(Failure::Transient(anyhow!(
                "the server answered {status} to a download; starting over"
            )));
        }
    };
    if let Some(total) = total
        && total > limit
    {
        return Err(Failure::Final(anyhow!(
            "it is {total} bytes, more than the {limit} a download may be"
        )));
    }
    read_body(
        response.into_body().into_reader(),
        limit,
        total,
        patience,
        body,
    )
}

/// Reads `reader` to its end onto `body`, which must stay within `limit` bytes and come to
/// `total` when that is known, and must bring [`Patience::floor`] bytes each window.
fn read_body(
    mut reader: impl std::io::Read,
    limit: u64,
    total: Option<u64>,
    patience: &Patience,
    body: &mut Vec<u8>,
) -> Result<(), Failure> {
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut window = (Instant::now(), body.len());
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                return Err(Failure::Transient(
                    anyhow::Error::new(error).context("the download broke off"),
                ));
            }
        };
        if read == 0 {
            break;
        }
        body.extend_from_slice(buffer.get(..read).unwrap_or_default());
        let received = u64::try_from(body.len()).map_err(|error| Failure::Final(error.into()))?;
        if received > limit {
            return Err(Failure::Final(anyhow!(
                "it is more than the {limit} bytes a download may be"
            )));
        }
        let (since, at) = window;
        if since.elapsed() >= patience.window {
            let brought = u64::try_from(body.len().saturating_sub(at)).unwrap_or(u64::MAX);
            if brought < patience.floor {
                return Err(Failure::Transient(anyhow!(
                    "the download stalled: {brought} bytes in {} s",
                    patience.window.as_secs_f64()
                )));
            }
            window = (Instant::now(), body.len());
        }
    }
    let received = u64::try_from(body.len()).map_err(|error| Failure::Final(error.into()))?;
    match total {
        Some(total) if received < total => Err(Failure::Transient(anyhow!(
            "the download ended at {received} of {total} bytes"
        ))),
        Some(total) if received > total => {
            body.clear();
            Err(Failure::Transient(anyhow!(
                "the download brought {received} bytes, more than the {total} announced; \
                 starting over"
            )))
        }
        Some(_) | None => Ok(()),
    }
}

/// Whether an answer with HTTP status `code` may pass when asked again: a timeout, too many
/// requests, or a server error.
fn retried_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500..=599)
}

/// The first byte and the total length of a `Content-Range: bytes <first>-<last>/<total>` value;
/// the total is none when the server gives `*`.
fn content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let first: u64 = first.parse().ok()?;
    let last: u64 = last.parse().ok()?;
    if last < first {
        return None;
    }
    let total = match total {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    Some((first, total))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use super::{Failure, Patience, content_range, fetch, patience, read_body, retried_status};

    /// What the test server does on one connection.
    enum Serve {
        /// Answers `status` with these headers and body.
        Whole(&'static str, String, Vec<u8>),
        /// Answers with the headers, sends `body`, then waits `pause` before hanging up.
        Stall(String, Vec<u8>, Duration),
        /// Answers with the headers, sends `body`, then hangs up with a reset: it closes the
        /// connection with the request still unread, which makes the kernel send RST.
        Reset(String, Vec<u8>),
    }

    /// Serves one connection per entry of `script`, in order, on a local port; returns its URL
    /// and a handle that yields each request's head.
    fn server(script: Vec<Serve>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/tool.tar.gz", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for step in script {
                let (mut stream, _) = listener.accept().unwrap();
                let head = request_head(&stream);
                requests.push(head);
                match step {
                    Serve::Whole(status, headers, body) => {
                        read_head(&mut stream);
                        write!(
                            stream,
                            "HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n"
                        )
                        .unwrap();
                        stream.write_all(&body).unwrap();
                    }
                    Serve::Stall(headers, body, pause) => {
                        read_head(&mut stream);
                        write!(stream, "HTTP/1.1 200 OK\r\n{headers}\r\n").unwrap();
                        stream.write_all(&body).unwrap();
                        stream.flush().unwrap();
                        // Holds the connection open, silent, while the next one is served.
                        std::thread::spawn(move || {
                            std::thread::sleep(pause);
                            drop(stream);
                        });
                    }
                    Serve::Reset(headers, body) => {
                        // The request stays unread, so closing resets the connection.
                        write!(stream, "HTTP/1.1 206 Partial Content\r\n{headers}\r\n").unwrap();
                        stream.write_all(&body).unwrap();
                        stream.flush().unwrap();
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            requests
        });
        (url, handle)
    }

    /// The request's head, peeked at and left unread.
    fn request_head(stream: &TcpStream) -> String {
        let mut buffer = [0_u8; 4096];
        loop {
            let peeked = stream.peek(&mut buffer).unwrap();
            let text = String::from_utf8_lossy(&buffer[..peeked]).into_owned();
            if let Some((head, _)) = text.split_once("\r\n\r\n") {
                return head.to_owned();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reads the request's head, which [`request_head`] has seen whole.
    fn read_head(stream: &mut TcpStream) {
        let head = request_head(stream);
        let mut buffer = vec![0_u8; head.len().saturating_add(4)];
        stream.read_exact(&mut buffer).unwrap();
    }

    fn quick() -> Patience {
        Patience {
            connect: Duration::from_secs(5),
            stall: Duration::from_millis(300),
            floor: 1,
            window: Duration::from_secs(60),
            retries: vec![Duration::from_millis(1); 3],
        }
    }

    /// The length of the test file, and where the first two attempts break off.
    const LENGTH: usize = 300_000;
    const FIRST: usize = 100_000;
    const SECOND: usize = 200_000;

    fn body() -> Vec<u8> {
        (0..=250_u8).cycle().take(LENGTH).collect()
    }

    fn range_header(head: &str) -> Option<&str> {
        head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range").then(|| value.trim())
        })
    }

    /// A download hung forever when the network stopped mid-body, and a reset lost everything:
    /// now a stalled read ends the attempt, a reset too, and each retry resumes where the bytes
    /// kept end, until the whole file is in.
    #[test]
    fn a_stall_and_a_reset_mid_body_resume_to_the_whole_file() {
        let whole = body();
        let (url, server) = server(vec![
            Serve::Stall(
                format!("Content-Length: {LENGTH}\r\n"),
                whole[..FIRST].to_vec(),
                Duration::from_secs(2),
            ),
            Serve::Reset(
                format!(
                    "Content-Length: 200000\r\nContent-Range: bytes {FIRST}-299999/{LENGTH}\r\n"
                ),
                whole[FIRST..SECOND].to_vec(),
            ),
            Serve::Whole(
                "206 Partial Content",
                format!(
                    "Content-Length: 100000\r\nContent-Range: bytes {SECOND}-299999/{LENGTH}\r\n"
                ),
                whole[SECOND..].to_vec(),
            ),
        ]);
        let mut slept = Vec::new();
        let mut notes = Vec::new();
        let started = std::time::Instant::now();
        let got = fetch(
            &url,
            1 << 20,
            &quick(),
            &mut |delay| slept.push(delay),
            &mut notes,
        )
        .unwrap();
        assert!(got == whole, "the whole file, byte for byte");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the stall ended the attempt at its limit, not the server's"
        );
        let requests = server.join().unwrap();
        let ranges: Vec<Option<&str>> = requests.iter().map(|head| range_header(head)).collect();
        assert_eq!(
            ranges,
            [None, Some("bytes=100000-"), Some("bytes=200000-")],
            "each retry resumes where the kept bytes end"
        );
        assert_eq!(slept.len(), 2, "one wait per retry");
        let notes = String::from_utf8(notes).unwrap();
        assert!(
            notes.contains("100000 bytes kept") && notes.contains("200000 bytes kept"),
            "{notes}"
        );
    }

    /// The review of PR 2b: the retries were one budget for the whole download, so a long one on a
    /// network that reset every few megabytes gave up short of the end though each attempt
    /// brought more. Only attempts in a row that bring nothing count against the retries.
    #[test]
    fn retries_count_only_attempts_that_bring_nothing() {
        let whole = body();
        let mut script = vec![Serve::Stall(
            format!("Content-Length: {LENGTH}\r\n"),
            whole[..50_000].to_vec(),
            Duration::from_secs(1),
        )];
        for from in [50_000, 100_000, 150_000, 200_000] {
            script.push(Serve::Reset(
                format!(
                    "Content-Length: {}\r\nContent-Range: bytes {from}-299999/{LENGTH}\r\n",
                    LENGTH.saturating_sub(from)
                ),
                whole[from..from.saturating_add(50_000)].to_vec(),
            ));
        }
        script.push(Serve::Whole(
            "206 Partial Content",
            format!("Content-Length: 50000\r\nContent-Range: bytes 250000-299999/{LENGTH}\r\n"),
            whole[250_000..].to_vec(),
        ));
        let attempts = script.len();
        let (url, server) = server(script);
        let got = fetch(&url, 1 << 20, &quick(), &mut |_| {}, &mut Vec::new()).unwrap();
        assert!(
            got == whole,
            "five breaks, three retries, and the whole file"
        );
        assert_eq!(server.join().unwrap().len(), attempts);
    }

    /// A server that ignores the range sends the whole file again: the kept bytes are dropped,
    /// not doubled.
    #[test]
    fn a_server_that_does_not_resume_starts_over() {
        let whole = body();
        let length = LENGTH;
        let (url, server) = server(vec![
            Serve::Stall(
                format!("Content-Length: {length}\r\n"),
                whole[..1000].to_vec(),
                Duration::from_secs(1),
            ),
            Serve::Whole(
                "200 OK",
                format!("Content-Length: {length}\r\n"),
                whole.clone(),
            ),
        ]);
        let got = fetch(&url, 1 << 20, &quick(), &mut |_| {}, &mut Vec::new()).unwrap();
        assert!(
            got == whole,
            "the file once, not the kept bytes and the file"
        );
        assert_eq!(server.join().unwrap().len(), 2, "one retry");
    }

    /// A 206 that does not start at the byte asked for would splice the file wrongly, so the
    /// download starts over.
    #[test]
    fn a_resume_from_the_wrong_byte_starts_over() {
        let whole = body();
        let length = LENGTH;
        let (url, server) = server(vec![
            Serve::Stall(
                format!("Content-Length: {length}\r\n"),
                whole[..1000].to_vec(),
                Duration::from_secs(1),
            ),
            Serve::Whole(
                "206 Partial Content",
                format!("Content-Length: 10\r\nContent-Range: bytes 0-9/{length}\r\n"),
                whole[..10].to_vec(),
            ),
            Serve::Whole(
                "200 OK",
                format!("Content-Length: {length}\r\n"),
                whole.clone(),
            ),
        ]);
        let got = fetch(&url, 1 << 20, &quick(), &mut |_| {}, &mut Vec::new()).unwrap();
        assert!(got == whole, "the whole file from a fresh start");
        let requests = server.join().unwrap();
        assert_eq!(range_header(&requests[1]), Some("bytes=1000-"));
        assert_eq!(range_header(&requests[2]), None, "starts over");
    }

    /// A missing file does not appear by asking again.
    #[test]
    fn a_not_found_fails_at_once() {
        let (url, server) = server(vec![Serve::Whole(
            "404 Not Found",
            "Content-Length: 0\r\n".to_owned(),
            Vec::new(),
        )]);
        let mut slept = 0;
        let error = fetch(
            &url,
            1 << 20,
            &quick(),
            &mut |_| slept += 1,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(slept, 0, "no retry");
        let error = format!("{error:#}");
        assert!(
            error.contains("could not download") && error.contains("404"),
            "{error}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// Server errors are retried, and the download gives up once the retries are spent, saying
    /// how many attempts it made.
    #[test]
    fn server_errors_are_retried_then_given_up() {
        let failing = || {
            Serve::Whole(
                "503 Service Unavailable",
                "Content-Length: 0\r\n".to_owned(),
                Vec::new(),
            )
        };
        let (url, server) = server(vec![failing(), failing(), failing(), failing()]);
        let mut slept = 0;
        let mut notes = Vec::new();
        let error = fetch(&url, 1 << 20, &quick(), &mut |_| slept += 1, &mut notes).unwrap_err();
        assert_eq!(slept, 3, "every retry waited");
        let error = format!("{error:#}");
        assert!(
            error.contains("gave up after 4 attempts in a row"),
            "{error}"
        );
        assert!(error.contains("503"), "{error}");
        assert_eq!(server.join().unwrap().len(), 4);
        assert_eq!(
            String::from_utf8(notes).unwrap().lines().count(),
            3,
            "each retry is said"
        );
    }

    /// A body larger than the limit is refused before it is read, and one that grows past it
    /// while read is cut off.
    #[test]
    fn a_body_over_the_limit_is_refused() {
        let (url, announced) = server(vec![Serve::Whole(
            "200 OK",
            "Content-Length: 2000\r\n".to_owned(),
            vec![0; 2000],
        )]);
        let error = fetch(&url, 1000, &quick(), &mut |_| {}, &mut Vec::new()).unwrap_err();
        assert!(
            format!("{error:#}").contains("more than the 1000"),
            "{error:#}"
        );
        announced.join().unwrap();
        let (unlimited, unannounced) =
            server(vec![Serve::Whole("200 OK", String::new(), vec![0; 2000])]);
        let cut = fetch(&unlimited, 1000, &quick(), &mut |_| {}, &mut Vec::new()).unwrap_err();
        assert!(
            format!("{cut:#}").contains("more than the 1000 bytes"),
            "{cut:#}"
        );
        unannounced.join().unwrap();
    }

    /// A trickle that keeps each read alive but brings less than the floor in a window is a
    /// stall too.
    #[test]
    fn a_trickle_below_the_floor_is_a_stall() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_head(&mut stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").unwrap();
            for _ in 0..20 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let patience = Patience {
            floor: 1000,
            window: Duration::from_millis(200),
            retries: Vec::new(),
            ..quick()
        };
        let error = fetch(&url, 1 << 20, &patience, &mut |_| {}, &mut Vec::new()).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("stalled"), "{error}");
        assert!(
            error.contains("gave up after 1 attempts in a row"),
            "{error}"
        );
        server.join().unwrap();
    }

    /// A body of exactly the limit, announced or not, is a whole download.
    #[test]
    fn a_body_of_exactly_the_limit_passes() {
        let (url, announced) = server(vec![Serve::Whole(
            "200 OK",
            "Content-Length: 1000\r\n".to_owned(),
            vec![7; 1000],
        )]);
        assert_eq!(
            fetch(&url, 1000, &quick(), &mut |_| {}, &mut Vec::new()).unwrap(),
            vec![7; 1000]
        );
        announced.join().unwrap();
        let mut body = Vec::new();
        let read = read_body(&[7_u8; 1000][..], 1000, None, &quick(), &mut body);
        assert!(
            read.is_ok() && body.len() == 1000,
            "unannounced, at the limit"
        );
    }

    /// A 206 to a request that asked for no range is no resume: the download starts over.
    #[test]
    fn a_partial_answer_to_a_fresh_request_starts_over() {
        let (url, server) = server(vec![
            Serve::Whole(
                "206 Partial Content",
                "Content-Length: 10\r\nContent-Range: bytes 0-9/20\r\n".to_owned(),
                vec![1; 10],
            ),
            Serve::Whole("200 OK", "Content-Length: 20\r\n".to_owned(), vec![2; 20]),
        ]);
        let got = fetch(&url, 1 << 20, &quick(), &mut |_| {}, &mut Vec::new()).unwrap();
        assert_eq!(got, vec![2; 20]);
        assert_eq!(server.join().unwrap().len(), 2, "asked again");
    }

    /// The body must come to the length announced: short is a break to resume from, long is
    /// a body to start over, and exact is whole.
    #[test]
    fn a_body_must_come_to_its_announced_length() {
        let read = |total: u64| {
            let mut body = vec![9; 5];
            let outcome = read_body(&[1_u8; 10][..], 1 << 20, Some(total), &quick(), &mut body);
            (outcome, body.len())
        };
        let (whole, whole_kept) = read(15);
        assert!(whole.is_ok() && whole_kept == 15, "exact: {whole_kept}");
        let (short, short_kept) = read(16);
        assert!(
            matches!(short, Err(Failure::Transient(ref error)) if error.to_string() == "the download ended at 15 of 16 bytes"),
            "short keeps its bytes"
        );
        assert_eq!(short_kept, 15);
        let (long, long_kept) = read(14);
        assert!(matches!(long, Err(Failure::Transient(_))), "long");
        assert_eq!(long_kept, 0, "a body longer than announced is dropped");
    }

    /// Each window must bring at least the floor: exactly the floor passes, one byte less is a
    /// stall.
    #[test]
    fn a_window_must_bring_the_floor() {
        let every_read = Patience {
            floor: 10,
            window: Duration::ZERO,
            ..quick()
        };
        let mut body = Vec::new();
        assert!(
            read_body(&[1_u8; 10][..], 1 << 20, None, &every_read, &mut body).is_ok(),
            "the floor exactly"
        );
        let mut short = Vec::new();
        let stalled = read_body(&[1_u8; 9][..], 1 << 20, None, &every_read, &mut short);
        assert!(
            matches!(stalled, Err(Failure::Transient(ref error)) if error.to_string().starts_with("the download stalled: 9 bytes")),
            "one byte short of the floor"
        );
    }

    /// The real patience, as AGENTS.md states it.
    #[test]
    fn real_downloads_wait_as_documented() {
        let real = patience();
        assert_eq!(real.connect, Duration::from_secs(15));
        assert_eq!(real.stall, Duration::from_secs(30));
        assert_eq!(real.floor, 16_384, "16 KiB");
        assert_eq!(real.window, Duration::from_secs(60));
        assert_eq!(
            real.retries,
            [2, 4, 8, 16, 32].map(Duration::from_secs),
            "five retries, doubling from 2 s"
        );
    }

    #[test]
    fn reads_content_ranges() {
        assert_eq!(content_range("bytes 10-99/100"), Some((10, Some(100))));
        assert_eq!(content_range(" bytes 0-0/* "), Some((0, None)));
        assert_eq!(content_range("bytes 9-3/100"), None);
        assert_eq!(content_range("bytes */100"), None);
        assert_eq!(content_range("items 0-1/2"), None);
        assert_eq!(content_range("bytes 0-1/x"), None);
    }

    #[test]
    fn retries_only_answers_that_may_change() {
        for code in [408, 429, 500, 502, 503, 599] {
            assert!(retried_status(code), "{code}");
        }
        for code in [400, 401, 403, 404, 410, 416, 600] {
            assert!(!retried_status(code), "{code}");
        }
    }
}
