//! The real thing on a real tailnet (decision 39, opt-in): `abc server setup --auth-key-file`
//! runs abc's own unprivileged tailscaled with Tailscale's own programs, logs it in as a node of
//! its own, points Serve at abc's socket with the machine's own `sudo`, and starts the pinned
//! Postgres and the real `abc server` (under the fake service manager of
//! `crates/abc-host/tests/fakes`, since a CI runner has no user systemd). From this machine's own
//! node, the test then claims the server with the code setup opened the claim page with, issues
//! an enrolment token, runs `abc worker enrol` and `abc worker` against the node's URL, watches
//! the server's live stream report the worker online, then offline once it stops, checks
//! `abc server status`, and uninstalls, which logs the node out. The worker verifies Serve's
//! Let's Encrypt certificate with the operating system's trust (decision W4), so this is also the
//! proof that the real chain verifies under the platform verifier, and the claim, the worker's
//! WebSocket and the live stream prove Serve's identity header, upgrade and streaming.
//!
//! It needs a real tailnet, so it is ignored unless asked for (`--run-ignored only`), and then
//! fails unless these are set:
//!
//! * `ABC_TAILSCALE_AUTHKEY`: an ephemeral auth key owned by a user (not tagged: tagged devices
//!   carry no identity), approved in advance, on a tailnet with `MagicDNS` and HTTPS certificates.
//!   Ephemeral, so a node a killed run leaves behind disappears by itself;
//! * `ABC_TAILSCALE_BIN`: the absolute path of a directory holding the `tailscaled` and
//!   `tailscale` to run;
//! * `ABC_TEST_POSTGRES_BIN`, as for every test that runs Postgres (`cargo xtask setup
//!   --for-tests` installs it).
//!
//! The machine running it must itself be a node of that tailnet whose resolver answers the
//! tailnet's names, as a worker's machine is, and `sudo` must not ask for a password: the
//! `Tailscale` workflow joins the runner as a node of its own and points systemd-resolved at
//! `MagicDNS` first.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this test needs only part of it"
)]
mod common;

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Output};
    use std::sync::Arc;
    use std::time::Duration;

    use http_body_util::{BodyExt as _, Full};
    use hyper::body::Bytes;
    use hyper::{Request, StatusCode};
    use hyper_util::rt::TokioIo;
    use rustls_platform_verifier::BuilderVerifierExt as _;
    use serde_json::{Value, json};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;

    use crate::common::{Machine, Tailscale, programs_dir};

    /// The text a process wrote to the file `log`, its last 8000 bytes at most.
    fn tail(log: &Path) -> String {
        let bytes = std::fs::read(log).unwrap_or_default();
        bytes.rchunks(8000).next().map(text).unwrap_or_default()
    }

    /// Prints the tails of the processes' logs when the test fails, since their output goes to
    /// files that are removed with the test's directory.
    struct Logs(Vec<(&'static str, PathBuf)>);

    impl Drop for Logs {
        fn drop(&mut self) {
            if std::thread::panicking() {
                for (name, log) in &self.0 {
                    let _shown = writeln!(
                        std::io::stderr(),
                        "--- the end of the {name}'s output ---\n{}",
                        tail(log)
                    );
                }
            }
        }
    }
    /// Kills a process the test started if the test ends before stopping it. Its output goes to a
    /// file, so a process nobody reads from never blocks on a full pipe, and the test can show
    /// the end of it.
    struct Running(Option<Child>, PathBuf);

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _killed = child.kill();
                let _reaped = child.wait();
            }
        }
    }

    impl Running {
        fn new(command: &mut Command, log: &Path) -> Self {
            let file = std::fs::File::create(log).unwrap();
            command.stdout(file.try_clone().unwrap()).stderr(file);
            Self(Some(command.spawn().unwrap()), log.to_owned())
        }

        /// Sends SIGTERM and waits for the exit, with the end of what the process wrote.
        async fn terminate(mut self) -> (ExitStatus, String) {
            let mut child = self.0.take().unwrap();
            let killed = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap();
            assert!(killed.success(), "SIGTERM was sent");
            let log = self.1.clone();
            tokio::task::spawn_blocking(move || {
                let status = child.wait().unwrap();
                (status, tail(&log))
            })
            .await
            .unwrap()
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// `abc` with `args` and a clean environment.
    fn abc(args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abc"));
        command.args(args).env_clear();
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    /// `abc worker` with `args` and the worker home `home`.
    fn worker(args: &[&str], home: &Path) -> Command {
        let mut command = abc(&["worker"]);
        command.args(args).arg("--home").arg(home);
        command
    }

    /// A TLS connection to `host:443` over the tailnet, verified as the worker verifies it: by
    /// what the operating system trusts.
    async fn connect(host: &str) -> Result<TlsStream<TcpStream>, String> {
        let stream = TcpStream::connect((host, 443))
            .await
            .map_err(|error| format!("no connection to {host}:443 yet: {error}"))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_platform_verifier()
            .unwrap()
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap();
        TlsConnector::from(Arc::new(tls))
            .connect(name, stream)
            .await
            .map_err(|error| format!("no TLS with {host} yet: {error}"))
    }

    /// One HTTP request to `host` through Serve, or why it could not be sent.
    async fn try_call(
        host: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value), String> {
        let io = TokioIo::new(connect(host).await?);
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|error| format!("no HTTP handshake: {error}"))?;
        tokio::spawn(connection);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", host);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let body = body.map_or_else(Bytes::new, |body| Bytes::from(body.to_string()));
        let response = sender
            .send_request(request.body(Full::new(body)).unwrap())
            .await
            .map_err(|error| format!("no response: {error}"))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("the response broke off: {error}"))?
            .to_bytes();
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    /// One HTTP request to `host` through Serve, once Serve answers.
    async fn call(
        host: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        try_call(host, method, path, body).await.unwrap()
    }

    /// `GET /api/session`, retried while the Serve node joins, DNS settles and the first
    /// certificate is issued; the last answer, or why there was none.
    async fn first_session(host: &str) -> Result<(StatusCode, Value), String> {
        let mut session = Err("no attempt yet".to_owned());
        for _ in 0..90 {
            let attempt = tokio::time::timeout(
                Duration::from_secs(10),
                try_call(host, "GET", "/api/session", None),
            )
            .await;
            session = attempt.unwrap_or_else(|_| Err("the attempt timed out".to_owned()));
            if session
                .as_ref()
                .is_ok_and(|(status, _)| *status == StatusCode::OK)
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        session
    }

    /// The machines in the server's live snapshot for `workspace`: the first event of a fresh
    /// `GET /api/workspaces/{workspace}/live`.
    async fn machines(host: &str, workspace: &str) -> Vec<Value> {
        let io = TokioIo::new(connect(host).await.unwrap());
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(connection);
        let live = Request::get(format!("/api/workspaces/{workspace}/live"))
            .header("host", host)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = sender.send_request(live).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "the owner may watch");
        let mut body = response.into_body();
        let mut received = String::new();
        let event = loop {
            if let Some((event, _)) = received.split_once("\n\n") {
                break event.to_owned();
            }
            let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
                .await
                .expect("the snapshot arrives")
                .expect("the stream stays open")
                .unwrap();
            if let Ok(data) = frame.into_data() {
                received.push_str(&text(&data));
            }
        };
        let data = event
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_else(|| panic!("an event with data: {event}"));
        let snapshot: Value = serde_json::from_str(data).unwrap();
        assert_eq!(snapshot["type"], "snapshot", "{snapshot}");
        snapshot["machines"].as_array().unwrap().clone()
    }

    /// Waits up to a minute for the server to report the one machine `online` or not.
    async fn until_online(host: &str, workspace: &str, online: bool) -> Vec<Value> {
        let mut seen = Vec::new();
        for _ in 0..120 {
            seen = machines(host, workspace).await;
            if seen.len() == 1 && seen[0]["online"] == online {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("the server never reported the worker online = {online}: {seen:?}");
    }

    /// Uninstalls the server however the test ends, which logs abc's node out of the tailnet.
    struct Uninstall<'a>(&'a Machine);

    impl Drop for Uninstall<'_> {
        fn drop(&mut self) {
            let _removed = self.0.abc(&["uninstall", "--purge"]);
        }
    }

    fn run(command: &mut Command) -> Output {
        command.output().unwrap()
    }

    /// The claim link `abc server setup` printed: the node's host and the setup code.
    fn claim_link(said: &str) -> (String, String) {
        said.lines()
            .find_map(|line| {
                let link = line.trim().strip_prefix("https://")?;
                let (host, code) = link.split_once("/#code=")?;
                Some((host.to_owned(), code.to_owned()))
            })
            .unwrap_or_else(|| panic!("setup printed the claim link: {said}"))
    }

    #[tokio::test]
    #[ignore = "needs a tailnet this machine is on: ABC_TAILSCALE_AUTHKEY and ABC_TAILSCALE_BIN; run with --run-ignored only"]
    async fn abc_server_setup_serves_the_real_binaries_through_tailscale() {
        let Ok(key) = std::env::var("ABC_TAILSCALE_AUTHKEY") else {
            panic!("the real-tailnet check needs ABC_TAILSCALE_AUTHKEY (see this file's header)");
        };
        let bin = programs_dir(
            "ABC_TAILSCALE_BIN",
            std::env::var_os("ABC_TAILSCALE_BIN"),
            &["tailscaled", "tailscale"],
        )
        .unwrap_or_else(|wrong| {
            panic!("the real-tailnet check needs Tailscale's programs: {wrong} (see this file's header)")
        });
        let machine = Machine::new(&Tailscale::Real(bin));
        let key_file = machine.root().join("authkey");
        std::fs::write(&key_file, key).unwrap();
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let worker_log = machine.root().join("worker.log");
        let state = machine.root().join("state");
        let _tails = Logs(
            ["postgres", "tailscaled", "server"]
                .into_iter()
                .map(|part| (part, state.join(format!("abc-{part}.service.log"))))
                .chain(std::iter::once(("worker", worker_log.clone())))
                .collect(),
        );
        let uninstall = Uninstall(&machine);

        let setup = machine.abc(&["setup", "--auth-key-file", key_file.to_str().unwrap()]);
        let said = text(&setup.stdout);
        assert!(
            setup.status.success(),
            "abc server setup: {said}\n{}",
            text(&setup.stderr)
        );
        let (host, code) = claim_link(&said);
        let session = first_session(&host).await.unwrap();
        assert_eq!(session.1["state"], "unclaimed", "{}", session.1);

        let claim = json!({ "code": code, "display_name": "Alice", "workspace_name": "Home" });
        let (claim_status, claimed) = call(&host, "POST", "/api/owner/claim", Some(claim)).await;
        assert_eq!(claim_status, StatusCode::OK, "{claimed}");
        let workspace = claimed["workspace"].as_str().unwrap().to_owned();
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let (issue_status, issued) = call(&host, "POST", &tokens, Some(json!({}))).await;
        assert_eq!(issue_status, StatusCode::OK, "{issued}");
        let token = issued["token"].as_str().unwrap();

        let home = machine.root().join("worker");
        let enrolled = run(worker(
            &[
                "enrol",
                &format!("https://{host}"),
                "--hostname",
                "ci-runner",
            ],
            &home,
        )
        .env("ABC_ENROLMENT_TOKEN", token));
        assert!(
            enrolled.status.success(),
            "abc worker enrol verifies Serve's certificate and enrols: {}",
            text(&enrolled.stderr)
        );

        let running = Running::new(worker(&[], &home).env("ABC_LOG", "info"), &worker_log);
        let online = until_online(&host, &workspace, true).await;
        assert_eq!(online[0]["hostname"], "ci-runner", "{online:?}");
        let (worker_exit, worker_output) = running.terminate().await;
        assert!(
            worker_exit.success(),
            "the worker exits cleanly on SIGTERM: {worker_output}"
        );
        until_online(&host, &workspace, false).await;

        let status = machine.abc(&["status"]);
        assert!(
            status.status.success(),
            "every part is healthy, Serve's real target among them: {}",
            text(&status.stdout)
        );
        drop(uninstall);
        assert!(!machine.home().exists(), "uninstalled and purged");
    }
}
