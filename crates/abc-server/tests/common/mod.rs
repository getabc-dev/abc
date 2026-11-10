//! The test harness: a real server (`abc_server::run`) on a fresh database, a Unix socket in a
//! private temp directory, and clients that play the Tailscale sidecar by setting the identity
//! headers themselves. HTTP and SSE go over hyper, the worker link over tokio-tungstenite, all on
//! the real socket.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use abc_core::credential::WorkerKey;
use abc_core::secret::Secret;
use abc_protocol::url::PublicUrl;
use abc_protocol::{Hello, ServerMessage, WorkerMessage};
use abc_server::{Limits, LinkTimings, ServeError, ServerConfig, WebApp};
use abc_store::testing::{Fixture, database};
use abc_store::{Store, StoreOptions};
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use semver::Version;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// The public URL every test server answers on.
pub(crate) const HOST: &str = "abc.test.ts.net";

/// The `Host` Serve sends to a `unix:` target.
pub(crate) const PROXIED_HOST: &str = "localhost";

/// A running server.
pub(crate) struct Server {
    _dir: tempfile::TempDir,
    pub(crate) socket: PathBuf,
    pub(crate) ops: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
    /// The test's own store on the server's database: what `abc server setup-code` uses, and a
    /// window for inspecting the log.
    pub(crate) store: Store,
    pub(crate) pool: PgPool,
}

/// A free loopback port for the ops listener.
fn free_port() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// A private directory for the socket: 0700, as the server demands.
pub(crate) fn socket_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// The password in the server's database URL. The test container trusts local connections and
/// ignores it; it is there so the log test can look for it.
pub(crate) const DATABASE_PASSWORD: &str = "db-pass-7f3e9a";

/// The page of the test servers' web app.
pub(crate) const INDEX: &[u8] =
    b"<!doctype html><title>abc</title><script src=\"/assets/index-0123abcd.js\"></script>";

/// The script of the test servers' web app.
pub(crate) const SCRIPT: &[u8] = b"document.title = 'abc';";

/// A web app as Vite builds one: the page and a hashed script.
pub(crate) fn web_app() -> WebApp {
    WebApp::new(&[("index.html", INDEX), ("assets/index-0123abcd.js", SCRIPT)]).unwrap()
}

/// The configuration a test server runs with.
pub(crate) fn config(pool: &PgPool, socket: PathBuf, link: LinkTimings) -> ServerConfig {
    let url = abc_store::testing::database_url_text(pool, Some(DATABASE_PASSWORD));
    ServerConfig {
        database_url: abc_store::DatabaseUrl::new(url),
        database_wait: Duration::from_secs(5),
        sidecar_socket: socket,
        ops_addr: free_port(),
        public_url: PublicUrl::parse(&format!("https://{HOST}")).unwrap(),
        link,
        limits: Limits::default(),
        web: web_app(),
        tailscaled_socket: None,
    }
}

/// Alice's MacBook, as the tailnet knows it.
pub(crate) const ALICE_MAC: &str = "100.64.0.1";
/// Alice's other machine.
pub(crate) const ALICE_OTHER: &str = "100.64.0.2";
/// Bob's laptop.
pub(crate) const BOB_LAPTOP: &str = "100.64.0.3";

/// What abc's tailscaled answers for an address: the node and its person.
fn whois_answer(addr: &str) -> Option<&'static str> {
    Some(match addr.rsplit_once(':')?.0 {
        ALICE_MAC => {
            r#"{"Node":{"Name":"alice-mbp.tail1234.ts.net."},"UserProfile":{"LoginName":"alice@example.com"}}"#
        }
        ALICE_OTHER => {
            r#"{"Node":{"Name":"alice-pc.tail1234.ts.net."},"UserProfile":{"LoginName":"alice@example.com"}}"#
        }
        BOB_LAPTOP => {
            r#"{"Node":{"Name":"bob-laptop.tail1234.ts.net."},"UserProfile":{"LoginName":"bob@example.com"}}"#
        }
        _ => return None,
    })
}

/// A stand-in for abc's tailscaled: a Unix socket answering LocalAPI's `whois` from a fixed
/// table, as the real one does over HTTP.
pub(crate) fn fake_tailscaled(dir: &std::path::Path) -> PathBuf {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let socket = dir.join("tailscaled.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0; 2048];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let line = head.lines().next().unwrap_or_default();
                let addr = line
                    .split("addr=")
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .unwrap_or_default();
                let reply = match whois_answer(addr) {
                    Some(body) => format!("HTTP/1.0 200 OK\r\n\r\n{body}"),
                    None => "HTTP/1.0 404 Not Found\r\n\r\nno such node".to_owned(),
                };
                let _written = stream.write_all(reply.as_bytes()).await;
                let _closed = stream.shutdown().await;
            });
        }
    });
    socket
}

impl Server {
    pub(crate) async fn start(pool: PgPool) -> Self {
        Self::start_with(pool, LinkTimings::default()).await
    }

    pub(crate) async fn start_with(pool: PgPool, link: LinkTimings) -> Self {
        Self::start_limited(pool, link, Limits::default()).await
    }

    pub(crate) async fn start_limited(pool: PgPool, link: LinkTimings, limits: Limits) -> Self {
        let dir = socket_dir();
        let socket = dir.path().join("abc.sock");
        let mut config = config(&pool, socket.clone(), link);
        config.limits = limits;
        config.tailscaled_socket = Some(fake_tailscaled(dir.path()));
        let ops = config.ops_addr;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(abc_server::run(config, async move {
            let _stopped = stopped.await;
        }));
        // The CLI's store: the real clock, like the server's.
        let store = Store::open(database(pool.clone()), StoreOptions::production())
            .await
            .unwrap();
        let server = Self {
            _dir: dir,
            socket,
            ops,
            stop: Some(stop),
            task: Some(task),
            store,
            pool,
        };
        for _ in 0..200 {
            if server.socket.exists() && server.ops_get("/readyz").await.0 == StatusCode::OK {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the server did not become ready");
    }

    /// Stops the server gracefully and returns what `run` returned.
    pub(crate) async fn stop(mut self) -> Result<(), ServeError> {
        if let Some(stop) = self.stop.take() {
            stop.send(()).unwrap();
        }
        let task = self.task.take().unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .unwrap()
            .unwrap()
    }

    /// A GET on the ops listener.
    pub(crate) async fn ops_get(&self, path: &str) -> (StatusCode, Value) {
        let Ok(stream) = TcpStream::connect(self.ops).await else {
            return (StatusCode::SERVICE_UNAVAILABLE, Value::Null);
        };
        let request = Request::get(path)
            .header("host", "localhost")
            .header("tailscale-user-login", "alice@example.com")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = send(TokioIo::new(stream), request).await;
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// Issues a setup code as `abc server setup-code` does, and returns its display form.
    pub(crate) async fn setup_code(&self) -> String {
        let issued = abc_store::testing::issue_setup_code(&self.store)
            .await
            .unwrap();
        issued.code.expose_secret().display_form().to_string()
    }

    /// Claims the server as alice and returns the workspace id.
    pub(crate) async fn claimed(&self) -> String {
        let code = self.setup_code().await;
        let (status, body) = self
            .call(
                Call::post("/api/owner/claim")
                    .alice()
                    .json(&claim_body(&code)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "alice claims: {body}");
        body["workspace"].as_str().unwrap().to_owned()
    }

    /// Issues a token as alice and returns (id, token).
    pub(crate) async fn token(&self, workspace: &str) -> (String, String) {
        let (status, body) = self
            .call(
                Call::post(&format!("/api/workspaces/{workspace}/enrolment-tokens"))
                    .alice()
                    .json(&json!({ "label": null })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "alice issues a token: {body}");
        (
            body["id"].as_str().unwrap().to_owned(),
            body["token"].as_str().unwrap().to_owned(),
        )
    }

    /// Enrols a fresh key with `token` and returns (key, worker id).
    pub(crate) async fn enrol(&self, token: &str, keys: &Fixture) -> (Secret<WorkerKey>, String) {
        let key = keys.worker_key();
        let (status, body) = self
            .call(Call::post("/api/enrol").json(&enrol_body(token, &key)))
            .await;
        assert_eq!(status, StatusCode::OK, "enrolment: {body}");
        (key, body["worker"].as_str().unwrap().to_owned())
    }

    /// Sends `call` over the sidecar socket and returns the status and JSON body.
    pub(crate) async fn call(&self, call: Call) -> (StatusCode, Value) {
        let response = self.raw(call).await;
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// Sends `call`, asserts its status, and returns its body.
    pub(crate) async fn expect(&self, call: Call, status: StatusCode, why: &str) -> Value {
        let (got, body) = self.call(call).await;
        assert_eq!(got, status, "{why}: {body}");
        body
    }

    /// Sends `call`, asserts that it is refused with `status` and the API error `code`, and
    /// returns the body.
    pub(crate) async fn refused(
        &self,
        call: Call,
        status: StatusCode,
        code: &str,
        why: &str,
    ) -> Value {
        let body = self.expect(call, status, why).await;
        assert_eq!(body["error"]["code"], code, "{why}: {body}");
        body
    }

    /// Sends `call` and returns the response with its body unread.
    pub(crate) async fn raw(&self, call: Call) -> Response<Incoming> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        send(TokioIo::new(stream), call.request()).await
    }

    /// Opens the live stream of `workspace` as alice.
    pub(crate) async fn live(&self, workspace: &str, last_event_id: Option<&str>) -> Sse {
        let mut call = Call::get(&format!("/api/workspaces/{workspace}/live")).alice();
        if let Some(id) = last_event_id {
            call = call.header("last-event-id", id);
        }
        let response = self.raw(call).await;
        assert_eq!(response.status(), StatusCode::OK, "the live stream opens");
        Sse {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    /// Connects the worker link with `key`, without the handshake.
    pub(crate) async fn connect(&self, key: &Secret<WorkerKey>) -> Link {
        self.try_connect(Some(key.expose_secret().as_str()))
            .await
            .unwrap()
    }

    /// Connects with an `Authorization: Bearer` value, or none; an HTTP refusal is `Err`.
    pub(crate) async fn try_connect(&self, bearer: Option<&str>) -> Result<Link, (u16, Value)> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let mut request = format!("ws://{PROXIED_HOST}{}", abc_protocol::link::CONNECT_PATH)
            .into_client_request()
            .unwrap();
        for (name, value) in [("x-forwarded-host", HOST), ("x-forwarded-proto", "https")] {
            request.headers_mut().insert(name, value.parse().unwrap());
        }
        if let Some(bearer) = bearer {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
        }
        match tokio_tungstenite::client_async(request, stream).await {
            Ok((socket, _)) => Ok(Link { socket }),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let status = response.status().as_u16();
                let body = response
                    .body()
                    .as_ref()
                    .and_then(|body| serde_json::from_slice(body).ok())
                    .unwrap_or(Value::Null);
                Err((status, body))
            }
            Err(error) => panic!("the upgrade failed: {error}"),
        }
    }

    /// The `session_ended` reasons the log holds, oldest first.
    pub(crate) async fn session_ends(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT payload->>'reason' FROM event WHERE event_type = 'worker.session_ended'
             ORDER BY position",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Waits until the log holds `count` session ends.
    pub(crate) async fn wait_for_ends(&self, count: usize) -> Vec<String> {
        for _ in 0..200 {
            let ends = self.session_ends().await;
            if ends.len() >= count {
                return ends;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "expected {count} session ends, have {:?}",
            self.session_ends().await
        );
    }
}

/// Sends `call` over the socket at `socket` and returns the status and JSON body.
pub(crate) async fn send_on(socket: &std::path::Path, call: Call) -> (StatusCode, Value) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let response = send(TokioIo::new(stream), call.request()).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn send(
    io: TokioIo<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>,
    request: Request<Full<Bytes>>,
) -> Response<Incoming> {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(connection);
    sender.send_request(request).await.unwrap()
}

/// A request to build.
pub(crate) struct Call {
    method: &'static str,
    path: String,
    headers: Vec<(String, String)>,
    /// Serve's headers this call leaves out.
    without: Vec<&'static str>,
    body: Option<Vec<u8>>,
}

impl Call {
    pub(crate) fn get(path: &str) -> Self {
        Self::new("GET", path)
    }

    pub(crate) fn post(path: &str) -> Self {
        Self::new("POST", path)
    }

    pub(crate) fn delete(path: &str) -> Self {
        Self::new("DELETE", path)
    }

    pub(crate) fn new(method: &'static str, path: &str) -> Self {
        Self {
            method,
            path: path.to_owned(),
            headers: Vec::new(),
            without: Vec::new(),
            body: None,
        }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Leaves out `name`, one of the headers Serve sends.
    pub(crate) fn without(mut self, name: &'static str) -> Self {
        self.without.push(name);
        self
    }

    /// The request as Tailscale Serve sends it to a `unix:` target: `Host: localhost`, the public
    /// host in `X-Forwarded-Host` and `X-Forwarded-Proto: https`, unless the call sets or leaves
    /// out one of them.
    fn request(self) -> Request<Full<Bytes>> {
        let mut builder = Request::builder().method(self.method).uri(&self.path);
        for (name, value) in [
            ("host", PROXIED_HOST),
            ("x-forwarded-host", HOST),
            ("x-forwarded-proto", "https"),
        ] {
            let set = self.headers.iter().any(|(header, _)| header == name);
            if !set && !self.without.contains(&name) {
                builder = builder.header(name, value);
            }
        }
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = self.body.map_or_else(Bytes::new, Bytes::from);
        builder.body(Full::new(body)).unwrap()
    }

    /// As `login`, as the sidecar asserts it.
    pub(crate) fn login(self, login: &str) -> Self {
        self.header("tailscale-user-login", login)
    }

    pub(crate) fn alice(self) -> Self {
        self.login("alice@example.com")
    }

    /// From the tailnet address Serve forwarded, as the last `X-Forwarded-For` entry.
    pub(crate) fn from_node(self, address: &str) -> Self {
        self.header("x-forwarded-for", address)
    }

    pub(crate) fn json(self, body: &Value) -> Self {
        self.body_text("application/json", &body.to_string())
    }

    pub(crate) fn body_text(mut self, content_type: &str, body: &str) -> Self {
        self.body = Some(body.as_bytes().to_vec());
        self.header("content-type", content_type)
    }
}

pub(crate) fn claim_body(code: &str) -> Value {
    json!({ "code": code, "display_name": "Alice", "workspace_name": "Home" })
}

pub(crate) fn enrol_body(token: &str, key: &Secret<WorkerKey>) -> Value {
    json!({
        "token": token,
        "key": key.expose_secret().digest(),
        "machine": { "hostname": "mbp", "os": "macos", "arch": "aarch64" },
        "build": "0.1.0",
    })
}

/// One server-sent event.
#[derive(Debug)]
pub(crate) struct Event {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) data: Value,
}

/// A live stream being read.
pub(crate) struct Sse {
    body: Incoming,
    buffer: String,
}

impl Sse {
    /// The next event, skipping keep-alive comments; `None` when the stream ends.
    pub(crate) async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end.saturating_add(2)).collect();
                let mut event = Event {
                    name: String::new(),
                    id: String::new(),
                    data: Value::Null,
                };
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event.name = value.trim().to_owned();
                    } else if let Some(value) = line.strip_prefix("id:") {
                        event.id = value.trim().to_owned();
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push_str(value.trim());
                    }
                }
                if event.name.is_empty() {
                    continue;
                }
                event.data = serde_json::from_str(&data).unwrap();
                return Some(event);
            }
            let frame = tokio::time::timeout(Duration::from_secs(10), self.body.frame())
                .await
                .expect("the live stream sends within ten seconds");
            match frame {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buffer.push_str(std::str::from_utf8(&data).unwrap());
                    }
                }
                Some(Err(_)) | None => return None,
            }
        }
    }

    /// The next `machines` event's cards.
    pub(crate) async fn machines(&mut self) -> (String, Vec<Value>) {
        let event = self.next().await.expect("a machines event");
        assert_eq!(event.name, "machines", "{event:?}");
        (event.id, event.data["changed"].as_array().unwrap().clone())
    }
}

/// The worker end of a link.
pub(crate) struct Link {
    pub(crate) socket: WebSocketStream<UnixStream>,
}

impl Link {
    pub(crate) async fn send_text(&mut self, text: &str) {
        self.socket.send(Message::Text(text.into())).await.unwrap();
    }

    /// Sends this build's `Hello`.
    pub(crate) async fn hello(&mut self) {
        let hello = WorkerMessage::Hello(Hello::new(Version::new(0, 1, 0)));
        self.send_text(&hello.encode().unwrap()).await;
    }

    /// The next server message, skipping pings.
    pub(crate) async fn message(&mut self) -> ServerMessage {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("the server answers")
                .expect("the link is open")
                .unwrap();
            match frame {
                Message::Text(text) => return ServerMessage::decode(&text).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other @ (Message::Binary(_) | Message::Close(_) | Message::Frame(_)) => {
                    panic!("expected a message, got {other:?}")
                }
            }
        }
    }

    /// Completes the handshake and expects `Welcome`.
    pub(crate) async fn handshake(&mut self) {
        self.hello().await;
        assert!(
            matches!(self.message().await, ServerMessage::Welcome(_)),
            "the server welcomes the worker"
        );
    }

    /// Waits for the server's close frame and returns its code.
    pub(crate) async fn closed(&mut self) -> u16 {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("the server closes the link");
            match frame {
                Some(Ok(Message::Close(Some(CloseFrame { code, .. })))) => return code.into(),
                Some(Ok(Message::Close(None)) | Err(_)) | None => return 1005,
                Some(Ok(_)) => {}
            }
        }
    }
}
