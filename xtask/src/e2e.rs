//! `cargo xtask e2e`: the web app in a real browser, served by the real binary.
//!
//! It builds `abc` as `cargo xtask build` does, creates a fresh database on the test Postgres
//! server (`ABC_TEST_DATABASE_URL`, else the warm container), starts `abc server` on it with its
//! sidecar socket in a private directory, and runs Playwright (`web/e2e/`) in Chromium. The
//! browser reaches the server through `web/e2e/sidecar.ts`, a stand-in for the Tailscale sidecar
//! that sets the headers Serve sets, as the server's own integration tests do; the server has no
//! test-only way in. Afterwards the server is stopped and the database dropped.
//!
//! The topology is production's: the browser loads the app from the origin the stand-in answers
//! on over HTTPS, with a certificate made for the run, and that origin is the server's public URL.
//! So the browser's `Origin` is the server's own, as it is behind Serve, and the server's checks
//! of forwarded hosts and origins run as they do in production.

use std::fs;
use std::io::{Read as _, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, ensure};

use crate::web::{self, Toolchain};

/// The origin the stand-in answers on and the browser loads the app from: the server's public
/// URL, as Serve answers on the public URL in production.
const ORIGIN: &str = "https://localhost:4173";

/// The host name the stand-in's certificate names.
const HOST_NAME: &str = "localhost";

/// How often to ask whether the server is ready, a tenth of a second apart: a minute for it to
/// migrate its database and start.
const READY_ATTEMPTS: u32 = 600;

/// Runs the end-to-end tests from `root`, with cargo-sqlx at `sqlx` making and dropping the
/// database, and reports whether they passed.
pub(crate) fn run(
    root: &Path,
    toolchain: &Toolchain,
    sqlx: &Path,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !web::build_binary(root, toolchain, out)? {
        return Ok(false);
    }
    let mut browsers = toolchain.pnpm(root)?;
    browsers.args(["exec", "playwright", "install", "chromium"]);
    if !browsers
        .status()
        .context("could not run playwright install")?
        .success()
    {
        writeln!(
            out,
            "xtask e2e: Playwright could not install Chromium; on Linux, install its system \
             libraries with `cargo xtask pnpm exec playwright install --with-deps chromium`; in \
             an agent VM, which downloads no browsers, rebuild the image with `cargo xtask vm \
             image` so it holds the Chromium of web/package.json's @playwright/test"
        )?;
        return Ok(false);
    }
    crate::postgres::warm(out)?;
    let server = std::env::var(crate::postgres::TEST_DATABASE_URL)
        .unwrap_or_else(|_| crate::postgres::URL.to_owned());
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let run = run_id();
    // Named as the test databases are, so a later test run sweeps it if this run is killed.
    let url = crate::postgres::with_database(&server, &format!("abc_test_{created}_e2e{run}"))?;
    let database = |action: &str| -> anyhow::Result<bool> {
        let mut command = Command::new(sqlx);
        command.args(["sqlx", "database", action, "--database-url", &url]);
        if action == "drop" {
            command.arg("-y");
        }
        Ok(command
            .status()
            .context("could not run cargo-sqlx")?
            .success())
    };
    ensure!(
        database("create")?,
        "could not create the end-to-end test's database"
    );
    let passed = serve_and_test(root, toolchain, &url, &run, out);
    let dropped = database("drop");
    let passed = passed?;
    ensure!(
        dropped?,
        "could not drop the end-to-end test's database; a later test run sweeps it"
    );
    Ok(passed)
}

/// This run's name, unique on the machine while it runs: the process's id and how many runs the
/// process started before it.
fn run_id() -> String {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let earlier = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}_{earlier}", std::process::id())
}

/// Starts the server on the database at `url`, runs Playwright against it and stops it.
fn serve_and_test(
    root: &Path,
    toolchain: &Toolchain,
    url: &str,
    run: &str,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    // A short path of its own: a Unix socket's path is limited to about a hundred bytes.
    let dir = std::env::temp_dir().join(format!("abc-e2e-{run}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("could not clear {}", dir.display()))?;
    }
    fs::create_dir(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not make {} private", dir.display()))?;
    let socket = dir.join("abc.sock");
    let (certificate, key) = certificate()?;
    let certificate_file = dir.join("tls.crt");
    let key_file = dir.join("tls.key");
    fs::write(&certificate_file, certificate)
        .with_context(|| format!("could not write {}", certificate_file.display()))?;
    fs::write(&key_file, key).with_context(|| format!("could not write {}", key_file.display()))?;
    let log = root.join("target/e2e/server.log");
    let ops = free_port()?;
    let mut server = start(root, url, &socket, ops, &log)?;
    let result = wait_ready(&mut server, ops, &log).and_then(|()| {
        writeln!(out, "xtask e2e: abc server is ready; running Playwright")?;
        let mut playwright = toolchain.pnpm(root)?;
        playwright
            .args(["exec", "playwright", "test"])
            .env("ABC_E2E_SOCKET", &socket)
            .env("ABC_E2E_ORIGIN", ORIGIN)
            .env("ABC_E2E_TLS_CERT", &certificate_file)
            .env("ABC_E2E_TLS_KEY", &key_file);
        Ok(playwright
            .status()
            .context("could not run playwright")?
            .success())
    });
    let stopped = server.kill().and_then(|()| server.wait());
    let removed = fs::remove_dir_all(&dir);
    let passed = result?;
    stopped.context("could not stop abc server")?;
    removed.with_context(|| format!("could not remove {}", dir.display()))?;
    if !passed {
        writeln!(
            out,
            "xtask e2e: Playwright failed: see its report above and the server's log in {}",
            log.display()
        )?;
    }
    Ok(passed)
}

/// A loopback address nothing listens on yet.
fn free_port() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").context("could not find a free port")?;
    listener.local_addr().context("could not read the port")
}

/// A self-signed certificate for the stand-in, made for this run alone, and its key, both as PEM.
/// The browser accepts it because the end-to-end tests ignore certificate errors; the key never
/// leaves the run's private directory, which is deleted afterwards.
fn certificate() -> anyhow::Result<(String, String)> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![HOST_NAME.to_owned()])
            .context("could not make the stand-in's certificate")?;
    Ok((
        pem("CERTIFICATE", cert.der()),
        pem("PRIVATE KEY", &signing_key.serialize_der()),
    ))
}

/// `der` as PEM labelled `label`: base64 in lines of 64 characters (RFC 7468).
fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let mut pem = format!("-----BEGIN {label}-----");
    for (index, char) in base64::engine::general_purpose::STANDARD
        .encode(der)
        .chars()
        .enumerate()
    {
        if index.is_multiple_of(64) {
            pem.push('\n');
        }
        pem.push(char);
    }
    pem.push_str("\n-----END ");
    pem.push_str(label);
    pem.push_str("-----\n");
    pem
}

/// Starts the built `abc server`, logging to `log`.
fn start(
    root: &Path,
    url: &str,
    socket: &Path,
    ops: SocketAddr,
    log: &Path,
) -> anyhow::Result<Child> {
    let binary: PathBuf = web::target_dir(root).join(web::BINARY);
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let file =
        fs::File::create(log).with_context(|| format!("could not create {}", log.display()))?;
    Command::new(&binary)
        .arg("server")
        .env("ABC_DATABASE_URL", url)
        .env("ABC_SIDECAR_SOCKET", socket)
        .env("ABC_OPS_ADDR", ops.to_string())
        .env("ABC_PUBLIC_URL", ORIGIN)
        .stdin(Stdio::null())
        .stdout(file.try_clone().context("could not share the log")?)
        .stderr(file)
        .spawn()
        .with_context(|| format!("could not start {}", binary.display()))
}

/// Waits until the server's ops listener says it is ready.
fn wait_ready(server: &mut Child, ops: SocketAddr, log: &Path) -> anyhow::Result<()> {
    for _ in 0..READY_ATTEMPTS {
        if let Some(status) = server.try_wait().context("could not check on abc server")? {
            anyhow::bail!(
                "abc server exited with {status} before it was ready; its log is in {}",
                log.display()
            );
        }
        if ready(ops) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "abc server was not ready within a minute; its log is in {}",
        log.display()
    )
}

/// Whether `GET /readyz` on the ops listener at `ops` answers 200.
fn ready(ops: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&ops, Duration::from_secs(1)) else {
        return false;
    };
    let request = "GET /readyz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    let mut answer = String::new();
    stream.write_all(request.as_bytes()).is_ok()
        && stream.read_to_string(&mut answer).is_ok()
        && answer.starts_with("HTTP/1.1 200")
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;

    use super::{certificate, free_port, pem, ready, run, run_id};
    use crate::testing::{abc_tree, fake_web, pnpm_calls, script};

    /// A fake cargo-sqlx in `dir` that logs what it does to the database.
    fn fake_sqlx(dir: &Path) -> std::path::PathBuf {
        let sqlx = dir.join("cargo-sqlx");
        script(
            &sqlx,
            &format!("echo \"$*\" >> {}\n", dir.join("sqlx.log").display()),
        );
        sqlx
    }

    /// Runs the end-to-end tests on a tree whose `abc` answers readiness, with pnpm failing the
    /// call that names `fail`; returns whether they passed, what was said, pnpm's calls and the
    /// database's.
    fn e2e(fail: &str) -> (bool, String, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        let toolchain = fake_web(&tools, fail);
        let root = temp.path().join("tree");
        abc_tree(&root);
        let mut out = Vec::new();
        let passed = run(&root, &toolchain, &fake_sqlx(&tools), &mut out).unwrap();
        let database = std::fs::read_to_string(tools.join("sqlx.log")).unwrap_or_default();
        (
            passed,
            String::from_utf8(out).unwrap(),
            pnpm_calls(&tools),
            database,
        )
    }

    #[test]
    fn serves_the_built_binary_on_a_fresh_database_to_playwright() {
        let (passed, said, calls, database) = e2e("");
        assert!(passed, "{said}");
        assert!(
            said.contains("abc server is ready; running Playwright"),
            "{said}"
        );
        assert_eq!(
            calls,
            "install --frozen-lockfile\nrun --silent build\nexec playwright install chromium\n\
             exec playwright test\n",
            "built, Chromium installed, then the tests"
        );
        let lines: Vec<&str> = database.lines().collect();
        assert_eq!(lines.len(), 2, "{database}");
        assert!(
            lines[0].starts_with("sqlx database create --database-url postgres"),
            "{database}"
        );
        assert!(
            lines[0].contains("/abc_test_"),
            "named for the sweep: {database}"
        );
        assert!(!lines[0].ends_with(" -y"), "{database}");
        assert!(
            lines[1].starts_with("sqlx database drop --database-url ") && lines[1].ends_with(" -y"),
            "dropped afterwards, without a prompt: {database}"
        );
    }

    #[test]
    fn a_failing_playwright_run_fails_and_points_at_the_servers_log() {
        let (passed, said, _, database) = e2e("playwright test");
        assert!(!passed);
        assert!(said.contains("Playwright failed"), "{said}");
        assert!(said.contains("target/e2e/server.log"), "{said}");
        assert_eq!(
            database.lines().count(),
            2,
            "the database is dropped anyway"
        );
    }

    #[test]
    fn pem_is_base64_in_lines_of_64() {
        let pem = pem("THING", &[0_u8; 49]);
        assert_eq!(
            pem,
            format!(
                "-----BEGIN THING-----\n{}\nAA==\n-----END THING-----\n",
                "A".repeat(64)
            ),
            "68 characters of base64, padding included: a full line and four more"
        );
    }

    #[test]
    fn the_stand_in_gets_a_fresh_certificate_and_key_for_each_run() {
        let (first, key) = certificate().unwrap();
        let (second, _) = certificate().unwrap();
        assert!(
            first.starts_with("-----BEGIN CERTIFICATE-----\n"),
            "{first}"
        );
        assert!(
            key.starts_with("-----BEGIN PRIVATE KEY-----\n"),
            "a PKCS #8 key"
        );
        assert_ne!(first, second, "made for the run");
    }

    #[test]
    fn each_run_has_a_name_of_its_own() {
        let (first, second) = (run_id(), run_id());
        assert_ne!(first, second, "two runs in one process");
        for name in [first, second] {
            assert!(
                name.starts_with(&format!("{}_", std::process::id())),
                "named by its process: {name}"
            );
        }
    }

    #[test]
    fn chromium_that_cannot_be_installed_says_how_to_get_its_libraries() {
        let (passed, said, _, database) = e2e("playwright install");
        assert!(!passed);
        assert!(said.contains("--with-deps chromium"), "{said}");
        assert!(said.contains("cargo xtask vm image"), "{said}");
        assert_eq!(database, "", "no database was made");
    }

    #[test]
    fn a_server_that_exits_before_it_is_ready_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        let root = temp.path().join("tree");
        abc_tree(&root);
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let error = run(
            &root,
            &fake_web(&tools, ""),
            &fake_sqlx(&tools),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("exited with exit status: 0 before it was ready"),
            "{error}"
        );
        assert!(
            std::fs::read_to_string(tools.join("sqlx.log"))
                .unwrap()
                .contains("drop "),
            "the database is dropped anyway"
        );
    }

    #[test]
    fn readiness_is_a_200_from_the_ops_listener() {
        assert!(!ready(free_port().unwrap()), "nothing listens");
        for (answer, expected) in [
            ("HTTP/1.1 200 OK\r\n\r\nready", true),
            ("HTTP/1.1 503 Service Unavailable\r\n\r\n", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let serving = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 512];
                let read = stream.read(&mut request).unwrap();
                stream.write_all(answer.as_bytes()).unwrap();
                String::from_utf8_lossy(&request[..read]).into_owned()
            });
            assert_eq!(ready(address), expected, "{answer}");
            let request = serving.join().unwrap();
            assert!(request.starts_with("GET /readyz HTTP/1.1\r\n"), "{request}");
        }
    }
}
