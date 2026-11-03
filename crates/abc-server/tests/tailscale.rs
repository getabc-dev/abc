//! The real Tailscale passthrough (open question 10, opt-in): the pinned `tailscale/tailscale`
//! image serves `https://<node>.<tailnet>` to `unix:` on the sidecar socket, and a second node
//! reaches it over the tailnet. It checks that Serve asserts the identity header and forwards the
//! public host (`Host: localhost` with `X-Forwarded-Host`), upgrades the worker WebSocket, and
//! streams SSE without buffering.
//!
//! It needs a real tailnet, so it is ignored unless asked for (`--run-ignored only`), and then
//! fails unless these are set:
//!
//! * `ABC_TAILSCALE_AUTHKEY`: an auth key owned by a user (not tagged: tagged devices carry no
//!   identity), reusable so both nodes can join, ideally ephemeral and approved in advance;
//! * `ABC_TAILSCALE_TAILNET`: the tailnet's DNS name, such as `tail1234.ts.net`, with `MagicDNS` and
//!   HTTPS certificates enabled.
//!
//! It also needs Docker on a Linux host: a Unix socket in a bind mount is shared with the
//! container only when the host and the container share a kernel. CI runs it in the `Tailscale`
//! workflow on a pull request labelled `tailscale-check`.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; this file needs only part of it"
)]
mod common;

#[cfg(test)]
mod tests {
    use abc_store::testing::TestDatabase;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use abc_core::credential::WorkerKey;
    use abc_core::secret::Secret;
    use abc_protocol::url::PublicUrl;
    use abc_protocol::{Hello, ServerMessage, WorkerMessage};
    use abc_server::LinkTimings;
    use abc_store::testing::Fixture;
    use futures::{SinkExt as _, StreamExt as _};
    use http_body_util::{BodyExt as _, Full};
    use hyper::body::Bytes;
    use hyper::{Request, StatusCode};
    use hyper_util::rt::TokioIo;
    use semver::Version;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    use crate::common::{claim_body, config, enrol_body, socket_dir};

    /// The Tailscale image, pinned by digest.
    const IMAGE: &str = "tailscale/tailscale:v1.104.4@sha256:ed6480b33c6eb67c9cbd203ed42b9fb57e361986725b6856def71b7a3d9e276c";

    /// Removes the test's containers however the test ends.
    struct Containers(Vec<String>);

    impl Drop for Containers {
        fn drop(&mut self) {
            for name in &self.0 {
                let _removed = Command::new("docker").args(["rm", "-f", name]).output();
            }
        }
    }

    fn docker(args: &[&str]) {
        let output = Command::new("docker").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "docker {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A TLS connection to `host:443` through the client node's HTTP proxy, or why there is none
    /// yet: until the client node has joined, its proxy resets the connection or refuses the tunnel.
    async fn tunnel(proxy: u16, host: &str) -> Result<TlsStream<TcpStream>, String> {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy))
            .await
            .map_err(|error| format!("the proxy is not listening: {error}"))?;
        stream
            .write_all(
                format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes(),
            )
            .await
            .map_err(|error| format!("the proxy dropped the CONNECT: {error}"))?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .await
            .map_err(|error| format!("the proxy did not answer the CONNECT: {error}"))?;
        if !status.contains(" 200") {
            return Err(format!("the proxy refused the tunnel: {status}"));
        }
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|error| format!("the proxy's answer broke off: {error}"))?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap();
        TlsConnector::from(Arc::new(tls))
            .connect(name, reader.into_inner())
            .await
            .map_err(|error| format!("no TLS through the tunnel yet: {error}"))
    }

    /// One HTTP request to `host` through the tailnet, or why it could not be sent.
    async fn try_call(
        proxy: u16,
        host: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value), String> {
        let io = TokioIo::new(tunnel(proxy, host).await?);
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

    /// One HTTP request to `host` through the tailnet, once the tailnet is up.
    async fn call(
        proxy: u16,
        host: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        try_call(proxy, host, method, path, body).await.unwrap()
    }

    /// The Serve node, proxying to the socket in `sockets` with the config in `serve_config`, and
    /// the client node, whose HTTP proxy listens on `proxy`.
    fn start_nodes(
        key: &str,
        node: &str,
        sockets: &Path,
        serve_config: &Path,
        proxy: u16,
    ) -> Containers {
        let client = format!("{node}-client");
        let containers = Containers(vec![node.to_owned(), client.clone()]);
        let auth = format!("TS_AUTHKEY={key}");
        let socket_volume = format!("{}:/run/abc-sidecar", sockets.display());
        let config_volume = format!("{}:/config:ro", serve_config.display());
        let server_name = format!("TS_HOSTNAME={node}");
        docker(&[
            "run",
            "--detach",
            "--name",
            node,
            "--env",
            &auth,
            "--env",
            &server_name,
            "--env",
            "TS_USERSPACE=true",
            "--env",
            "TS_SERVE_CONFIG=/config/serve.json",
            "--volume",
            &socket_volume,
            "--volume",
            &config_volume,
            IMAGE,
        ]);
        let client_name = format!("TS_HOSTNAME={client}");
        let published = format!("127.0.0.1:{proxy}:1055");
        docker(&[
            "run",
            "--detach",
            "--name",
            &client,
            "--env",
            &auth,
            "--env",
            &client_name,
            "--env",
            "TS_USERSPACE=true",
            "--env",
            "TS_OUTBOUND_HTTP_PROXY_LISTEN=0.0.0.0:1055",
            "--publish",
            &published,
            IMAGE,
        ]);
        containers
    }

    /// `GET /api/session` through the tailnet, retried while the nodes join, DNS settles and the
    /// first certificate is issued; the last answer, or why there was none.
    async fn first_session(proxy: u16, host: &str) -> Result<(StatusCode, Value), String> {
        let mut session = Err("no attempt yet".to_owned());
        for _ in 0..90 {
            let attempt = tokio::time::timeout(
                Duration::from_secs(10),
                try_call(proxy, host, "GET", "/api/session", None),
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

    /// Claims the server and enrols a worker through the tailnet; returns the workspace and key.
    async fn enrolled_worker(pool: &PgPool, proxy: u16, host: &str) -> (String, Secret<WorkerKey>) {
        let cli = abc_store::Store::open(
            abc_store::testing::database(pool.clone()),
            abc_store::StoreOptions::production(),
        )
        .await
        .unwrap();
        let issued = abc_store::testing::issue_setup_code(&cli).await.unwrap();
        let code = issued.code.expose_secret().display_form().to_string();
        let claim = Some(claim_body(&code));
        let (status, claimed) = call(proxy, host, "POST", "/api/owner/claim", claim).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the claim passes through: {claimed}"
        );
        let workspace = claimed["workspace"].as_str().unwrap().to_owned();
        let tokens = format!("/api/workspaces/{workspace}/enrolment-tokens");
        let (_, token) = call(proxy, host, "POST", &tokens, Some(json!({}))).await;
        let key = Fixture::new(pool.clone()).await.worker_key();
        let enrol = Some(enrol_body(token["token"].as_str().unwrap(), &key));
        let (enrol_status, enrolled) = call(proxy, host, "POST", "/api/enrol", enrol).await;
        assert_eq!(enrol_status, StatusCode::OK, "{enrolled}");
        (workspace, key)
    }

    #[tokio::test]
    #[ignore = "needs a tailnet: ABC_TAILSCALE_AUTHKEY and ABC_TAILSCALE_TAILNET; run with --run-ignored only"]
    async fn serve_passes_identity_websockets_and_unbuffered_sse_to_the_unix_socket() {
        let (Ok(key), Ok(tailnet)) = (
            std::env::var("ABC_TAILSCALE_AUTHKEY"),
            std::env::var("ABC_TAILSCALE_TAILNET"),
        ) else {
            panic!(
                "the Tailscale passthrough check needs ABC_TAILSCALE_AUTHKEY and \
                 ABC_TAILSCALE_TAILNET (see this file's header)"
            );
        };
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let node = format!("abc-e2e-{}", std::process::id());
        let host = format!("{node}.{tailnet}");
        let sockets = socket_dir();
        std::fs::set_permissions(sockets.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        let serve_config = tempfile::tempdir().unwrap();
        let serve = json!({
            "TCP": { "443": { "HTTPS": true } },
            "Web": { "${TS_CERT_DOMAIN}:443": { "Handlers": { "/": {
                "Proxy": "unix:/run/abc-sidecar/abc.sock"
            } } } }
        });
        std::fs::write(serve_config.path().join("serve.json"), serve.to_string()).unwrap();
        let mut settings = config(
            &pool,
            sockets.path().join("abc.sock"),
            LinkTimings::default(),
        );
        settings.public_url = PublicUrl::parse(&format!("https://{host}")).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(abc_server::run(settings, async move {
            let _stopped = stopped.await;
        }));
        let proxy = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let containers = start_nodes(&key, &node, sockets.path(), serve_config.path(), proxy);

        let session = first_session(proxy, &host).await.unwrap();
        assert_eq!(
            session.0,
            StatusCode::OK,
            "Serve reaches the socket, forwarding the public host: {}",
            session.1
        );
        assert_eq!(
            session.1["state"], "unclaimed",
            "Serve asserted a login: {}",
            session.1
        );
        let (workspace, worker_key) = enrolled_worker(&pool, proxy, &host).await;

        let io = TokioIo::new(tunnel(proxy, &host).await.unwrap());
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::spawn(connection);
        let live = Request::get(format!("/api/workspaces/{workspace}/live"))
            .header("host", &host)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let mut body = sender.send_request(live).await.unwrap().into_body();
        let first = tokio::time::timeout(Duration::from_secs(10), body.frame()).await;
        assert!(
            matches!(first, Ok(Some(Ok(_)))),
            "the snapshot arrives through Serve"
        );

        let mut request = format!("wss://{host}{}", abc_protocol::link::CONNECT_PATH)
            .into_client_request()
            .unwrap();
        let bearer = format!("Bearer {}", worker_key.expose_secret().as_str());
        request
            .headers_mut()
            .insert("authorization", bearer.parse().unwrap());
        let (mut socket, _) =
            tokio_tungstenite::client_async(request, tunnel(proxy, &host).await.unwrap())
                .await
                .expect("Serve upgrades the WebSocket");
        let hello = WorkerMessage::Hello(Hello::new(Version::new(0, 1, 0)));
        socket
            .send(Message::Text(hello.encode().unwrap().into()))
            .await
            .unwrap();
        let welcome = loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                break ServerMessage::decode(&text).unwrap();
            }
        };
        assert!(
            matches!(welcome, ServerMessage::Welcome(_)),
            "the handshake passes through"
        );
        let change = tokio::time::timeout(Duration::from_secs(5), body.frame()).await;
        assert!(
            matches!(change, Ok(Some(Ok(_)))),
            "the online update arrives at once: Serve does not buffer SSE"
        );
        drop(socket);
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        drop(containers);
    }
}
