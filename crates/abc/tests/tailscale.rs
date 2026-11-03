//! The real binaries behind real Tailscale Serve (decision 39, opt-in): `abc server` serves its
//! sidecar socket through the pinned `tailscale/tailscale` image's Serve on
//! `https://<node>.<tailnet>`; the test claims it, issues an enrolment token, runs `abc worker
//! enrol` and `abc worker` against that URL, and watches the server report the worker online, then
//! offline once the worker is stopped. The worker verifies Serve's Let's Encrypt certificate for
//! the `ts.net` name with the operating system's trust (decision W4), so this is also the proof that
//! the real chain verifies under the platform verifier.
//!
//! It needs a real tailnet, so it is ignored unless asked for (`--run-ignored only`), and then
//! fails unless these are set:
//!
//! * `ABC_TAILSCALE_AUTHKEY`: an auth key owned by a user (not tagged: tagged devices carry no
//!   identity), reusable, ideally ephemeral and approved in advance;
//! * `ABC_TAILSCALE_TAILNET`: the tailnet's DNS name, such as `tail1234.ts.net`, with `MagicDNS` and
//!   HTTPS certificates enabled.
//!
//! The machine running it must itself be a node of that tailnet whose resolver answers the
//! tailnet's names, as the worker's machine is in production: the `Tailscale` workflow joins the
//! runner with the same image in kernel mode and points systemd-resolved at `MagicDNS` first. It
//! also needs Docker on a Linux host, to share the sidecar socket with the Serve container.

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Output, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    use abc_store::testing::TestDatabase;
    use http_body_util::{BodyExt as _, Full};
    use hyper::body::Bytes;
    use hyper::{Request, StatusCode};
    use hyper_util::rt::TokioIo;
    use rustls_platform_verifier::BuilderVerifierExt as _;
    use serde_json::{Value, json};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;

    /// The Tailscale image, pinned by digest in the one file the Tailscale workflow and both
    /// Tailscale tests read.
    const IMAGE: &str = include_str!("../../../.github/tailscale-image").trim_ascii();

    /// Removes the Serve container however the test ends.
    struct Container(String);

    impl Drop for Container {
        fn drop(&mut self) {
            let _removed = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }

    /// Kills a process the test started if the test ends before stopping it.
    struct Running(Option<Child>);

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _killed = child.kill();
                let _reaped = child.wait();
            }
        }
    }

    impl Running {
        fn new(command: &mut Command) -> Self {
            Self(Some(command.spawn().unwrap()))
        }

        /// Sends SIGTERM and waits for the exit, with what the process wrote on stderr.
        async fn terminate(mut self) -> (ExitStatus, String) {
            let mut child = self.0.take().unwrap();
            let killed = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap();
            assert!(killed.success(), "SIGTERM was sent");
            tokio::task::spawn_blocking(move || {
                let status = child.wait().unwrap();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    std::io::Read::read_to_string(&mut pipe, &mut stderr).unwrap();
                }
                (status, stderr)
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

    fn docker(args: &[&str]) {
        let output = Command::new("docker").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "docker {args:?}: {}",
            text(&output.stderr)
        );
    }

    /// The Serve node: Tailscale Serve on `https://<node>.<tailnet>` in front of the sidecar socket
    /// in `sockets`, configured by `serve.json` in `config`.
    fn start_serve(key: &str, node: &str, sockets: &Path, config: &Path) -> Container {
        let container = Container(node.to_owned());
        docker(&[
            "run",
            "--detach",
            "--name",
            node,
            "--env",
            &format!("TS_AUTHKEY={key}"),
            "--env",
            &format!("TS_HOSTNAME={node}"),
            "--env",
            "TS_USERSPACE=true",
            "--env",
            "TS_SERVE_CONFIG=/config/serve.json",
            "--volume",
            &format!("{}:/run/abc-sidecar", sockets.display()),
            "--volume",
            &format!("{}:/config:ro", config.display()),
            IMAGE,
        ]);
        container
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

    /// Starts `abc server` on the database at `database` behind `host`, and waits until it
    /// reports ready.
    async fn start_server(database: &str, host: &str, socket: &Path) -> Running {
        let ops = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let server = Running::new(
            abc(&["server"])
                .env("ABC_DATABASE_URL", database)
                .env("ABC_PUBLIC_URL", format!("https://{host}"))
                .env("ABC_SIDECAR_SOCKET", socket)
                .env("ABC_OPS_ADDR", ops.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::piped()),
        );
        for _ in 0..1200 {
            if socket.exists() {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("abc server never bound {}", socket.display());
    }

    fn run(command: &mut Command) -> Output {
        command.output().unwrap()
    }

    #[tokio::test]
    #[ignore = "needs a tailnet this machine is on: ABC_TAILSCALE_AUTHKEY and ABC_TAILSCALE_TAILNET; run with --run-ignored only"]
    async fn the_real_binaries_connect_through_tailscale_serve() {
        let (Ok(key), Ok(tailnet)) = (
            std::env::var("ABC_TAILSCALE_AUTHKEY"),
            std::env::var("ABC_TAILSCALE_TAILNET"),
        ) else {
            panic!(
                "the real-app check needs ABC_TAILSCALE_AUTHKEY and ABC_TAILSCALE_TAILNET (see \
                 this file's header)"
            );
        };
        let database = TestDatabase::migrated().await;
        let url = abc_store::testing::database_url_text(&database.pool(), None);
        let node = format!("abc-app-{}", std::process::id());
        let host = format!("{node}.{tailnet}");

        let sockets = tempfile::tempdir().unwrap();
        // The Serve container's root reaches the socket through the group bits.
        std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        let socket = sockets.path().join("abc.sock");
        let serve_config = tempfile::tempdir().unwrap();
        let serve = json!({
            "TCP": { "443": { "HTTPS": true } },
            "Web": { "${TS_CERT_DOMAIN}:443": { "Handlers": { "/": {
                "Proxy": "unix:/run/abc-sidecar/abc.sock"
            } } } }
        });
        std::fs::write(serve_config.path().join("serve.json"), serve.to_string()).unwrap();

        let server = start_server(&url, &host, &socket).await;
        let container = start_serve(&key, &node, sockets.path(), serve_config.path());
        let session = first_session(&host).await.unwrap();
        assert_eq!(session.1["state"], "unclaimed", "{}", session.1);

        let setup = run(abc(&["server", "setup-code"]).env("ABC_DATABASE_URL", &url));
        assert!(setup.status.success(), "{}", text(&setup.stderr));
        let printed = text(&setup.stdout);
        let code = printed
            .lines()
            .next()
            .and_then(|line| line.rsplit(' ').next())
            .unwrap_or_else(|| panic!("a setup code: {printed}"));
        let claim = json!({ "code": code, "display_name": "Alice", "workspace_name": "Home" });
        let (claim_status, claimed) = call(&host, "POST", "/api/owner/claim", Some(claim)).await;
        assert_eq!(claim_status, StatusCode::OK, "{claimed}");
        let workspace = claimed["workspace"].as_str().unwrap().to_owned();
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let (issue_status, issued) = call(&host, "POST", &tokens, Some(json!({}))).await;
        assert_eq!(issue_status, StatusCode::OK, "{issued}");
        let token = issued["token"].as_str().unwrap();

        let homes = tempfile::tempdir().unwrap();
        let home = homes.path().join("worker");
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

        let running = Running::new(
            worker(&[], &home)
                .env("ABC_LOG", "info")
                .stdout(Stdio::null())
                .stderr(Stdio::piped()),
        );
        let online = until_online(&host, &workspace, true).await;
        assert_eq!(online[0]["hostname"], "ci-runner", "{online:?}");

        let (worker_exit, worker_log) = running.terminate().await;
        assert!(
            worker_exit.success(),
            "the worker exits cleanly on SIGTERM: {worker_log}"
        );
        until_online(&host, &workspace, false).await;

        let (server_exit, server_log) = server.terminate().await;
        assert!(
            server_exit.success(),
            "the server exits cleanly on SIGTERM: {server_log}"
        );
        drop(container);
    }
}
