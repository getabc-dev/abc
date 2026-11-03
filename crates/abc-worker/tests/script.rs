//! The worker against a scripted server: each connection plays the next step of a script, so the
//! handshake, every close code and every refusal can be shown exactly, including the ones the real
//! server cannot be made to send (a `Reject`, a version nobody offered, a message after `Welcome`).
//! Every script ends in a `Reject`, which stops the worker, so each test ends deterministically.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this file needs only its certificates"
)]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use abc_protocol::{
        ProtocolVersion, Reject, RejectReason, SUPPORTED_VERSIONS, ServerMessage, VersionRange,
        Welcome, WorkerMessage,
    };
    use abc_worker::{RunError, Terminal, WorkerHome};
    use futures::{SinkExt as _, StreamExt as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    use crate::common::Pki;

    /// What the server does with one connection.
    #[derive(Debug, Clone)]
    enum Step {
        /// Answers the upgrade with this status and body, in one write.
        Refuse(u16, &'static str),
        /// The same, with the head and the body in separate writes and a pause between them, as
        /// Tailscale Serve delivers them.
        RefuseSplit(u16, &'static str),
        /// Upgrades, reads `Hello`, then plays these.
        Upgrade(Vec<Act>),
    }

    #[derive(Debug, Clone)]
    enum Act {
        Send(Message),
        Wait(Duration),
        /// Reads the worker's frames until it closes, and records its close code.
        AwaitClose,
    }

    /// What the scripted server saw.
    #[derive(Debug, Default)]
    struct Seen {
        connections: usize,
        hellos: Vec<String>,
        bearers: Vec<String>,
        closes: Vec<u16>,
    }

    fn text(message: &ServerMessage) -> Message {
        Message::Text(message.encode().unwrap().into())
    }

    fn reject() -> Step {
        Step::Upgrade(vec![Act::Send(text(&ServerMessage::Reject(Reject {
            reason: RejectReason::UnsupportedVersion {
                supported: VersionRange::single(ProtocolVersion::new(9)),
            },
        })))])
    }

    fn welcome() -> Act {
        Act::Send(text(&ServerMessage::Welcome(Welcome {
            protocol: abc_protocol::PROTOCOL_VERSION,
        })))
    }

    fn close(code: u16) -> Act {
        Act::Send(Message::Close(Some(CloseFrame {
            code: code.into(),
            reason: "scripted".into(),
        })))
    }

    /// Serves `script` on a TLS port, one step per connection, and returns its URL and what it
    /// saw.
    async fn serve(pki: &Pki, script: Vec<Step>) -> (String, Arc<Mutex<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let acceptor = pki.acceptor();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            for step in script {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(tcp).await.unwrap();
                {
                    let mut counted = record.lock().unwrap();
                    counted.connections = counted.connections.saturating_add(1);
                }
                match step {
                    Step::Refuse(status, body) => refuse(&mut tls, status, body, false).await,
                    Step::RefuseSplit(status, body) => {
                        refuse(&mut tls, status, body, true).await;
                    }
                    Step::Upgrade(acts) => {
                        let bearer = Arc::new(Mutex::new(String::new()));
                        let seen_bearer = Arc::clone(&bearer);
                        #[expect(
                            clippy::result_large_err,
                            reason = "the signature tungstenite's upgrade callback must have"
                        )]
                        let callback =
                            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                  response| {
                                if let Some(value) = request.headers().get("authorization") {
                                    *seen_bearer.lock().unwrap() =
                                        value.to_str().unwrap().to_owned();
                                }
                                Ok(response)
                            };
                        let mut link = tokio_tungstenite::accept_hdr_async(tls, callback)
                            .await
                            .unwrap();
                        record
                            .lock()
                            .unwrap()
                            .bearers
                            .push(bearer.lock().unwrap().clone());
                        if let Some(Ok(Message::Text(hello))) = link.next().await {
                            record.lock().unwrap().hellos.push(hello.to_string());
                        }
                        for act in acts {
                            match act {
                                Act::Send(message) => {
                                    let _gone = link.send(message).await;
                                }
                                Act::Wait(duration) => tokio::time::sleep(duration).await,
                                Act::AwaitClose => loop {
                                    match link.next().await {
                                        Some(Ok(Message::Close(frame))) => {
                                            record.lock().unwrap().closes.push(
                                                frame.map_or(1005, |frame| frame.code.into()),
                                            );
                                            break;
                                        }
                                        Some(Ok(_)) => {}
                                        Some(Err(_)) | None => {
                                            record.lock().unwrap().closes.push(1006);
                                            break;
                                        }
                                    }
                                },
                            }
                        }
                        // Holds the link a moment so the worker reads what was sent.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
        (url, seen)
    }

    /// Reads the upgrade request's head, then answers `status` with `body`: in one write, or
    /// `split` into the head and then, after a pause, the body.
    async fn refuse(
        tls: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        status: u16,
        body: &str,
        split: bool,
    ) {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            tls.read_exact(&mut byte).await.unwrap();
            head.extend_from_slice(&byte);
        }
        let answer = format!(
            "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        if split {
            tls.write_all(answer.as_bytes()).await.unwrap();
            tls.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            tls.write_all(body.as_bytes()).await.unwrap();
        } else {
            tls.write_all(format!("{answer}{body}").as_bytes())
                .await
                .unwrap();
        }
        tls.shutdown().await.unwrap();
    }

    /// A home enrolled with `url` under a fixed key.
    fn enrolled_home(url: &str) -> (tempfile::TempDir, WorkerHome, String) {
        let dir = tempfile::tempdir().unwrap();
        let home = WorkerHome::at(dir.path().join("worker"));
        std::fs::create_dir_all(home.dir()).unwrap();
        std::fs::set_permissions(
            home.dir(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let key = abc_worker::testing::key(9);
        let text = key.expose_secret().as_str().to_owned();
        let record = format!(
            "format = 1\n\n[current.key]\nserver = \"{url}\"\ntoken = \"{}\"\nkey = \"{text}\"\n\n[current.worker]\nworker = \"{}\"\nworkspace = \"{}\"\nname = \"scripted\"\n",
            "0".repeat(64),
            uuid::Uuid::from_u128(1),
            uuid::Uuid::from_u128(2),
        );
        let path = home.dir().join("worker.toml");
        std::fs::write(&path, record).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        (dir, home, text)
    }

    /// Runs a worker against `script` until it stops by itself.
    async fn play(script: Vec<Step>) -> (RunError, Seen, String) {
        let pki = Pki::new();
        let (url, seen) = serve(&pki, script).await;
        let (_dir, home, key) = enrolled_home(&url);
        let env = abc_worker::testing::env(
            abc_worker::testing::trust(pki.root.clone()).unwrap(),
            abc_worker::testing::fast(),
            Arc::new(abc_worker::SystemClock),
        );
        let stopped = tokio::time::timeout(
            Duration::from_secs(20),
            abc_worker::run(&home, &env, std::future::pending()),
        )
        .await
        .expect("the script stops the worker")
        .unwrap_err();
        let seen = std::mem::take(&mut *seen.lock().unwrap());
        (stopped, seen, key)
    }

    fn mismatch() -> Terminal {
        Terminal::VersionMismatch {
            server: VersionRange::single(ProtocolVersion::new(9)),
            worker: SUPPORTED_VERSIONS,
        }
    }

    #[tokio::test]
    async fn the_worker_says_hello_with_its_key_and_stops_on_a_reject() {
        let (stopped, seen, key) = play(vec![reject()]).await;
        assert!(
            matches!(&stopped, RunError::Terminal(terminal) if *terminal == mismatch()),
            "{stopped:?}"
        );
        assert_eq!(
            seen.bearers,
            [format!("Bearer {key}")],
            "the key, as a bearer"
        );
        assert_eq!(seen.hellos.len(), 1, "one hello");
        let hello = WorkerMessage::decode(seen.hellos.first().unwrap()).unwrap();
        assert_eq!(
            hello,
            WorkerMessage::Hello(abc_protocol::Hello::new(semver::Version::new(0, 1, 0))),
            "this build's versions and release"
        );
    }

    #[tokio::test]
    async fn the_server_breaking_the_protocol_is_closed_with_4400_and_retried() {
        let unoffered = Act::Send(text(&ServerMessage::Welcome(Welcome {
            protocol: ProtocolVersion::new(7),
        })));
        let (stopped, seen, _) = play(vec![
            Step::Upgrade(vec![unoffered, Act::AwaitClose]),
            Step::Upgrade(vec![
                Act::Send(Message::Binary(vec![1].into())),
                Act::AwaitClose,
            ]),
            Step::Upgrade(vec![Act::Send(Message::Text("{".into())), Act::AwaitClose]),
            Step::Upgrade(vec![
                Act::Send(Message::Ping(Vec::new().into())),
                Act::AwaitClose,
            ]),
            Step::Upgrade(vec![
                welcome(),
                Act::Send(text(&ServerMessage::Welcome(Welcome {
                    protocol: abc_protocol::PROTOCOL_VERSION,
                }))),
                Act::AwaitClose,
            ]),
            Step::Upgrade(vec![
                welcome(),
                Act::Send(Message::Binary(vec![2].into())),
                Act::AwaitClose,
            ]),
            reject(),
        ])
        .await;
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(seen.connections, 7, "each violation was retried");
        assert_eq!(seen.closes, [4400; 6], "and closed as a protocol violation");
    }

    #[tokio::test]
    async fn closes_that_are_not_final_are_retried() {
        let (stopped, seen, _) = play(vec![
            Step::Upgrade(vec![welcome(), close(4408)]),
            Step::Upgrade(vec![welcome(), close(4400)]),
            Step::Upgrade(vec![welcome(), close(4503)]),
            Step::Upgrade(vec![welcome(), close(1000)]),
            Step::Upgrade(vec![welcome(), close(1011)]),
            Step::Upgrade(vec![close(4503)]),
            reject(),
        ])
        .await;
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(seen.connections, 7, "every one was dialled again");
    }

    #[tokio::test]
    async fn final_closes_stop_the_worker() {
        for (code, expected) in [(4403, Terminal::Revoked), (4409, Terminal::Superseded)] {
            let (stopped, seen, _) = play(vec![Step::Upgrade(vec![welcome(), close(code)])]).await;
            assert!(
                matches!(&stopped, RunError::Terminal(terminal) if *terminal == expected),
                "{code}: {stopped:?}"
            );
            assert_eq!(seen.connections, 1, "{code} is not retried");
        }
    }

    #[tokio::test]
    async fn a_server_that_does_not_answer_hello_is_dialled_again() {
        let (stopped, seen, _) = play(vec![
            Step::Upgrade(vec![Act::Wait(Duration::from_secs(3))]),
            reject(),
        ])
        .await;
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(seen.connections, 2, "after the handshake timing");
    }

    #[tokio::test]
    async fn refusals_are_retried_or_final_by_their_code() {
        let (stopped, seen, _) = play(vec![
            Step::Refuse(429, r#"{"error":{"code":"rate_limited","message":"m"}}"#),
            Step::Refuse(503, r#"{"error":{"code":"unavailable","message":"m"}}"#),
            Step::Refuse(502, "<html>bad gateway</html>"),
            Step::Refuse(404, r#"{"error":{"code":"not_found","message":"m"}}"#),
            reject(),
        ])
        .await;
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(seen.connections, 5, "each refusal was retried");
        for (body, expected) in [
            (
                r#"{"error":{"code":"invalid_worker_key","message":"m"}}"#,
                Terminal::InvalidKey,
            ),
            (
                r#"{"error":{"code":"worker_revoked","message":"m"}}"#,
                Terminal::Revoked,
            ),
        ] {
            for step in [Step::Refuse(401, body), Step::RefuseSplit(401, body)] {
                let (final_stop, final_seen, _) = play(vec![step.clone()]).await;
                assert!(
                    matches!(&final_stop, RunError::Terminal(terminal) if *terminal == expected),
                    "{step:?}: {final_stop:?}"
                );
                assert_eq!(final_seen.connections, 1, "not retried: {step:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_refusal_whose_body_arrives_after_its_head_is_still_read() {
        let (stopped, seen, _) = play(vec![
            Step::RefuseSplit(429, r#"{"error":{"code":"rate_limited","message":"m"}}"#),
            Step::RefuseSplit(502, "<html>bad gateway</html>"),
            reject(),
        ])
        .await;
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(seen.connections, 3, "each refusal was retried");
    }

    #[tokio::test]
    async fn a_sleep_during_the_backoff_dials_again_at_once() {
        let pki = Pki::new();
        let (url, seen) = serve(
            &pki,
            vec![
                Step::Refuse(503, r#"{"error":{"code":"unavailable","message":"m"}}"#),
                reject(),
            ],
        )
        .await;
        let (_dir, home, _) = enrolled_home(&url);
        // A backoff of at least 30 s after the first refusal: far longer than the test waits.
        let timings = abc_worker::Timings {
            backoff_base: Duration::from_secs(60),
            backoff_cap: Duration::from_secs(60),
            ..abc_worker::testing::fast()
        };
        let wall = Arc::new(abc_worker::testing::ManualWallClock::default());
        let clock: Arc<dyn abc_worker::WallClock> =
            Arc::<abc_worker::testing::ManualWallClock>::clone(&wall);
        let env = abc_worker::testing::env(
            abc_worker::testing::trust(pki.root.clone()).unwrap(),
            timings,
            clock,
        );
        let running =
            tokio::spawn(async move { abc_worker::run(&home, &env, std::future::pending()).await });
        // The worker takes its first clock reading as the wait begins; a jump before that is not
        // a sleep it can see. So the clock jumps an hour at a time until it dials again.
        let mut jumps = 0;
        while seen.lock().unwrap().connections < 2 {
            assert!(jumps < 40, "the worker never noticed the sleep");
            wall.sleep(Duration::from_secs(3600));
            jumps += 1;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let stopped = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the second dial plays the reject")
            .unwrap()
            .unwrap_err();
        assert!(matches!(stopped, RunError::Terminal(_)), "{stopped:?}");
        assert_eq!(
            seen.lock().unwrap().connections,
            2,
            "dialled again on waking, long before the backoff ran out"
        );
    }

    #[tokio::test]
    async fn a_stopping_worker_closes_its_link_cleanly() {
        let pki = Pki::new();
        let (url, seen) = serve(&pki, vec![Step::Upgrade(vec![welcome(), Act::AwaitClose])]).await;
        let (_dir, home, _) = enrolled_home(&url);
        let env = abc_worker::testing::env(
            abc_worker::testing::trust(pki.root.clone()).unwrap(),
            abc_worker::testing::fast(),
            Arc::new(abc_worker::SystemClock),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            abc_worker::run(&home, &env, async move {
                let _stopped = stopped.await;
            })
            .await
        });
        for _ in 0..200 {
            if !seen.lock().unwrap().hellos.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.send(()).unwrap();
        running.await.unwrap().unwrap();
        for _ in 0..200 {
            if !seen.lock().unwrap().closes.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(seen.lock().unwrap().closes, [1000], "a normal close");
    }

    #[tokio::test]
    async fn a_worker_stops_at_once_while_it_dials_or_waits() {
        // Nothing listens here: every dial fails, and the worker backs off between them.
        let pki = Pki::new();
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let (_dir, home, _) = enrolled_home(&format!("https://localhost:{port}"));
        let env = abc_worker::testing::env(
            abc_worker::testing::trust(pki.root.clone()).unwrap(),
            abc_worker::testing::fast(),
            Arc::new(abc_worker::SystemClock),
        );
        let stopped = tokio::time::timeout(
            Duration::from_secs(5),
            abc_worker::run(&home, &env, tokio::time::sleep(Duration::from_millis(300))),
        )
        .await
        .expect("it stops when asked");
        stopped.unwrap();
    }
}
