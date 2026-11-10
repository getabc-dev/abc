//! The harness for running the real server and the real worker together.
//!
//! The server (`abc_server::run`) serves its sidecar Unix socket on a fresh database. In front of
//! it, [`Serve`] stands in for Tailscale Serve: it terminates TLS on a loopback port with a
//! certificate for `localhost` from a CA made for the test, and proxies each connection to the
//! socket the way Serve does for a `unix:` target (`Host: localhost`, the public host in
//! `X-Forwarded-Host`, `X-Forwarded-Proto: https`, the peer in `X-Forwarded-For`, and any copies a
//! client sent replaced). The worker dials `https://localhost:<port>` and trusts only the test CA.
//! The owner's side (claim, tokens, revocation) goes to the socket as Serve would send it.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use abc_protocol::url::PublicUrl;
use abc_server::{Limits, LinkTimings, ServeError, ServerConfig};
use abc_store::testing::database;
use abc_store::{Store, StoreOptions};
use abc_worker::testing::ManualWallClock;
use abc_worker::{Env, Timings, WallClock, WorkerHome};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

/// A CA and a certificate it issued for `localhost`.
pub(crate) struct Pki {
    pub(crate) root: CertificateDer<'static>,
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
}

impl Pki {
    pub(crate) fn new() -> Self {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "abc test CA");
        let issuer = rcgen::CertifiedIssuer::self_signed(ca, ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        Self {
            root: issuer.der().clone(),
            chain: vec![leaf.der().clone(), issuer.der().clone()],
            key: leaf_key.serialize_der(),
        }
    }

    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()));
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(self.chain.clone(), key)
        .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        TlsAcceptor::from(Arc::new(config))
    }
}

/// The stand-in for Tailscale Serve.
/// The login Serve asserts for a machine in tests that approve in the browser.
pub(crate) const MACHINE_LOGIN: &str = "alice@example.com";

/// The node abc's (stand-in) tailscaled names for every loopback address, since a test's machine
/// and its Serve share one.
pub(crate) const MACHINE_NODE: &str = "mbp.tail1234.ts.net";

/// A stand-in for abc's tailscaled at `socket`: LocalAPI's `whois`, answering that every address
/// is [`MACHINE_NODE`] of [`MACHINE_LOGIN`].
fn fake_tailscaled(socket: &Path) -> PathBuf {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0; 2048];
                let _read = stream.read(&mut buffer).await;
                let reply = format!(
                    "HTTP/1.0 200 OK\r\n\r\n{{\"Node\":{{\"Name\":\"{MACHINE_NODE}.\"}},\
                     \"UserProfile\":{{\"LoginName\":\"{MACHINE_LOGIN}\"}}}}"
                );
                let _written = stream.write_all(reply.as_bytes()).await;
                let _closed = stream.shutdown().await;
            });
        }
    });
    socket.to_path_buf()
}

pub(crate) struct Serve {
    /// The login asserted from now on; none plays a tagged device.
    pub(crate) login: Arc<std::sync::Mutex<Option<&'static str>>>,
    pub(crate) url: PublicUrl,
    /// How many requests it has passed on.
    pub(crate) requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Serve {
    /// Serves on a free loopback port, proxying to the socket at `target`.
    ///
    /// `login` is asserted in `Tailscale-User-Login` for every request, as Serve does for a
    /// machine signed in as that person.
    pub(crate) async fn start(pki: &Pki, target: PathBuf, login: Option<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = pki.acceptor();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let asserted = Arc::new(std::sync::Mutex::new(login));
        let shared = Arc::clone(&asserted);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let Ok((tcp, peer)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let target = target.clone();
                let counted = Arc::clone(&counted);
                let asserted = Arc::clone(&shared);
                connections.spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let login = *asserted.lock().unwrap();
                    proxy(tls, &target, &peer.ip().to_string(), login, &counted).await;
                });
            }
        });
        Self {
            login: asserted,
            url: PublicUrl::parse(&format!("https://localhost:{port}")).unwrap(),
            requests,
            task,
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        // Ends every proxied connection with it.
        self.task.abort();
    }
}

/// Reads one request head, rewrites it as Serve does, and joins the client to the socket.
async fn proxy(
    mut client: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    target: &Path,
    peer: &str,
    login: Option<&str>,
    requests: &AtomicUsize,
) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 64 * 1024 || client.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.extend_from_slice(&byte);
    }
    let text = String::from_utf8(head).unwrap();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap().to_owned();
    let mut host = String::new();
    let mut rewritten = format!("{request_line}\r\n");
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        let name = name.trim().to_ascii_lowercase();
        if name == "host" {
            value.trim().clone_into(&mut host);
        } else if !name.starts_with("x-forwarded-") && !name.starts_with("tailscale-") {
            rewritten.push_str(line);
            rewritten.push_str("\r\n");
        }
    }
    rewritten.push_str("host: localhost\r\nx-forwarded-host: ");
    rewritten.push_str(&host);
    rewritten.push_str("\r\nx-forwarded-proto: https\r\nx-forwarded-for: ");
    rewritten.push_str(peer);
    rewritten.push_str("\r\n");
    if let Some(login) = login {
        rewritten.push_str("tailscale-user-login: ");
        rewritten.push_str(login);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("\r\n");
    let Ok(mut server) = UnixStream::connect(target).await else {
        // The server is down: Serve answers 502.
        let _sent = client
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await;
        return;
    };
    requests.fetch_add(1, Ordering::SeqCst);
    if server.write_all(rewritten.as_bytes()).await.is_err() {
        return;
    }
    let _ended = tokio::io::copy_bidirectional(&mut client, &mut server).await;
}

/// The worker link's timings for tests: pings every 100 ms.
pub(crate) const LINK: LinkTimings = LinkTimings {
    handshake: Duration::from_secs(2),
    ping: Duration::from_millis(100),
    idle: Duration::from_secs(2),
};

/// A running server on a database, with its socket in a private directory.
pub(crate) struct Server {
    pub(crate) socket: PathBuf,
    /// The public host, which Serve forwards in `X-Forwarded-Host`.
    host: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

/// A private directory for the socket: 0700, as the server demands.
pub(crate) fn socket_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

impl Server {
    /// Starts a server on `pool` at `socket`, answering on `url`.
    pub(crate) async fn start(
        pool: &PgPool,
        socket: PathBuf,
        url: &PublicUrl,
        link: LinkTimings,
    ) -> Self {
        let ops = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ops_addr = ops.local_addr().unwrap();
        drop(ops);
        let config = ServerConfig {
            database_url: abc_store::testing::database_url(pool),
            database_wait: Duration::from_secs(5),
            sidecar_socket: socket.clone(),
            ops_addr,
            public_url: url.clone(),
            link,
            limits: Limits::default(),
            tailscaled_socket: Some(fake_tailscaled(&socket.with_file_name("tailscaled.sock"))),
            // The worker's tests reach only the API.
            web: abc_server::WebApp::new(&[("index.html", b"<!doctype html>")]).unwrap(),
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(abc_server::run(config, async move {
            let _stopped = stopped.await;
        }));
        let server = Self {
            socket,
            host: url.host().to_owned(),
            stop: Some(stop),
            task: Some(task),
        };
        for _ in 0..400 {
            if server.call(Request::get("/api/session")).await.0 == StatusCode::OK {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the server did not start");
    }

    /// Stops the server gracefully.
    pub(crate) async fn stop(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// Sends `request` over the socket as Serve would, with `body`, and returns the status and
    /// JSON body; a socket that is not there yet is a 503.
    pub(crate) async fn call(&self, request: hyper::http::request::Builder) -> (StatusCode, Value) {
        self.call_with(request, None).await
    }

    async fn call_with(
        &self,
        request: hyper::http::request::Builder,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let Ok(stream) = UnixStream::connect(&self.socket).await else {
            return (StatusCode::SERVICE_UNAVAILABLE, Value::Null);
        };
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut request = request
            .header("host", "localhost")
            .header("x-forwarded-host", self.host.as_str())
            .header("x-forwarded-proto", "https")
            .header("tailscale-user-login", "alice@example.com");
        let bytes = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Bytes::from(json.to_string())
            }
            None => Bytes::new(),
        };
        let Ok(response) = sender
            .send_request(request.body(Full::new(bytes)).unwrap())
            .await
        else {
            return (StatusCode::SERVICE_UNAVAILABLE, Value::Null);
        };
        let status = response.status();
        let answer = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&answer).unwrap_or(Value::Null),
        )
    }

    /// The first message of the workspace's live stream as alice sees it: the Machines screen's
    /// snapshot.
    pub(crate) async fn snapshot(&self, workspace: &str) -> Value {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let request = Request::get(format!("/api/workspaces/{workspace}/live"))
            .header("host", "localhost")
            .header("x-forwarded-host", self.host.as_str())
            .header("x-forwarded-proto", "https")
            .header("tailscale-user-login", "alice@example.com")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let mut body = sender.send_request(request).await.unwrap().into_body();
        let mut text = String::new();
        while !text.contains("\n\n") {
            let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
                .await
                .expect("the snapshot comes at once")
                .expect("the stream is open")
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
        let data: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect();
        assert!(text.contains("event: snapshot"), "{text}");
        serde_json::from_str(&data).unwrap()
    }

    /// As alice, with a JSON body; asserts success and returns the answer.
    pub(crate) async fn alice(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, answer) = self
            .call_with(Request::builder().method(method).uri(path), body)
            .await;
        assert!(status.is_success(), "{method} {path}: {status} {answer}");
        answer
    }
}

/// A server behind Serve, claimed by alice, with a database the test can inspect.
pub(crate) struct World {
    pub(crate) pki: Pki,
    pub(crate) serve: Serve,
    pub(crate) server: Option<Server>,
    pub(crate) pool: PgPool,
    pub(crate) workspace: String,
    socket: PathBuf,
    _dir: tempfile::TempDir,
    _database: abc_store::testing::TestDatabase,
}

impl World {
    pub(crate) async fn start() -> Self {
        Self::start_with(LINK).await
    }

    pub(crate) async fn start_with(link: LinkTimings) -> Self {
        let test_database = abc_store::testing::TestDatabase::migrated().await;
        let pool = test_database.pool();
        let dir = socket_dir();
        let socket = dir.path().join("abc.sock");
        let pki = Pki::new();
        let serve = Serve::start(&pki, socket.clone(), Some(MACHINE_LOGIN)).await;
        let server = Server::start(&pool, socket.clone(), &serve.url, link).await;
        let store = Store::open(database(pool.clone()), StoreOptions::production())
            .await
            .unwrap();
        let code = abc_store::testing::issue_setup_code(&store)
            .await
            .unwrap()
            .code
            .expose_secret()
            .display_form()
            .to_string();
        let claimed = server
            .alice(
                "POST",
                "/api/owner/claim",
                Some(json!({ "code": code, "display_name": "Alice", "workspace_name": "Home" })),
            )
            .await;
        Self {
            pki,
            serve,
            server: Some(server),
            pool,
            workspace: claimed["workspace"].as_str().unwrap().to_owned(),
            socket,
            _dir: dir,
            _database: test_database,
        }
    }

    pub(crate) fn server(&self) -> &Server {
        self.server.as_ref().unwrap()
    }

    /// Stops the server, as for an upgrade or a crash-free restart.
    pub(crate) async fn stop_server(&mut self) {
        self.server.take().unwrap().stop().await;
    }

    /// Starts the server again on the same database, socket and URL.
    pub(crate) async fn restart_server(&mut self, link: LinkTimings) {
        self.server =
            Some(Server::start(&self.pool, self.socket.clone(), &self.serve.url, link).await);
    }

    /// A new enrolment token from alice, labelled `label`.
    pub(crate) async fn token(&self, label: &str) -> String {
        let issued = self
            .server()
            .alice(
                "POST",
                &format!("/api/workspaces/{}/enrolment-tokens", self.workspace),
                Some(json!({ "label": label })),
            )
            .await;
        issued["token"].as_str().unwrap().to_owned()
    }

    /// Alice revokes `worker`.
    pub(crate) async fn revoke(&self, worker: &str) {
        self.server()
            .alice(
                "DELETE",
                &format!("/api/workspaces/{}/workers/{worker}", self.workspace),
                None,
            )
            .await;
    }

    /// A worker environment that trusts this world's Serve, with `timings` and `wall`.
    pub(crate) fn env(&self, timings: Timings, wall: Arc<dyn WallClock>) -> Env {
        abc_worker::testing::env(
            abc_worker::testing::trust(self.pki.root.clone()).unwrap(),
            timings,
            wall,
        )
    }

    /// [`World::env`] with fast timings and the system's wall clock.
    pub(crate) fn fast_env(&self) -> Env {
        self.env(
            abc_worker::testing::fast(),
            Arc::new(abc_worker::SystemClock),
        )
    }

    /// [`World::env`] with fast timings and a wall clock the test moves to play a sleep.
    pub(crate) fn sleepy_env(&self) -> (Env, Arc<ManualWallClock>) {
        let wall = Arc::new(ManualWallClock::default());
        let clock: Arc<dyn WallClock> = Arc::<ManualWallClock>::clone(&wall);
        (self.env(abc_worker::testing::fast(), clock), wall)
    }

    /// How many sessions the log has started.
    pub(crate) async fn starts(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM event WHERE event_type = 'worker.session_started'")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// The reasons of the log's session ends, oldest first.
    pub(crate) async fn ends(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT payload->>'reason' FROM event WHERE event_type = 'worker.session_ended'
             ORDER BY position",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// How many events the log holds.
    pub(crate) async fn events(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM event")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Waits until `check` holds, for ten seconds at most.
    pub(crate) async fn until<F, Fut>(&self, what: &str, check: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..400 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting until {what}");
    }

    /// Waits until the log has started `count` sessions.
    pub(crate) async fn until_starts(&self, count: i64) {
        self.until(&format!("{count} sessions started"), || async {
            self.starts().await >= count
        })
        .await;
    }

    /// Waits until the log has ended `count` sessions.
    pub(crate) async fn until_ends(&self, count: usize) -> Vec<String> {
        self.until(&format!("{count} sessions ended"), || async {
            self.ends().await.len() >= count
        })
        .await;
        self.ends().await
    }
}

/// A fresh worker home in a temporary directory.
pub(crate) fn home() -> (tempfile::TempDir, WorkerHome) {
    let dir = tempfile::tempdir().unwrap();
    let home = WorkerHome::at(dir.path().join("worker"));
    (dir, home)
}

/// A worker running in the background until [`Running::stop`] or until it stops by itself.
pub(crate) struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), abc_worker::RunError>>,
}

impl Running {
    pub(crate) fn start(home: WorkerHome, env: Env) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            abc_worker::run(&home, &env, async move {
                let _stopped = stopped.await;
            })
            .await
        });
        Self {
            stop: Some(stop),
            task,
        }
    }

    /// Asks the worker to stop and returns what `run` returned.
    pub(crate) async fn stop(mut self) -> Result<(), abc_worker::RunError> {
        let _gone = self.stop.take().unwrap().send(());
        self.finished().await
    }

    /// Waits for the worker to stop by itself.
    pub(crate) async fn finished(self) -> Result<(), abc_worker::RunError> {
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the worker stops within ten seconds")
            .unwrap()
    }
}
