//! The `abc` binary end to end: `abc --version`, `abc server setup-code` against a real database,
//! configuration errors and their exit codes, `abc server` and `abc worker` stopping cleanly on
//! SIGTERM, and `abc worker enrol`'s refusals.

#[cfg(test)]
mod tests {
    use abc_store::testing::TestDatabase;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Output, Stdio};
    use std::time::Duration;

    use abc_core::credential::SetupCode;
    use abc_store::testing::Fixture;
    use sqlx::PgPool;

    /// The test database's URL.
    fn url(pool: &PgPool) -> String {
        abc_store::testing::database_url_text(pool, None)
    }

    /// `abc` with `args`, a clean environment and the database URL.
    fn abc(args: &[&str], database: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abc"));
        command
            .args(args)
            .env_clear()
            .env("ABC_DATABASE_URL", database)
            .env("ABC_DATABASE_WAIT", "2");
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    fn run(command: &mut Command) -> Output {
        command.output().unwrap()
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn version_prints_the_released_version() {
        // The Homebrew formula's test matches this line against the version the release tagged.
        let output = run(&mut abc(&["--version"], "unused"));
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert_eq!(
            text(&output.stdout),
            format!("abc {}\n", env!("CARGO_PKG_VERSION")),
            "the workspace's version, which the release tag must match"
        );
    }

    #[tokio::test]
    async fn setup_code_prints_a_code_that_claims_the_server_once() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let output = run(&mut abc(&["server", "setup-code"], &url(&pool)));
        assert!(output.status.success(), "{}", text(&output.stderr));
        let stdout = text(&output.stdout);
        let line = stdout.lines().next().unwrap();
        assert!(line.starts_with("Setup code (valid until "), "{stdout}");
        let code = line.rsplit(' ').next().unwrap();
        assert_eq!(code.len(), 23, "four groups of five: {code}");
        let fixture = Fixture::new(pool.clone()).await;
        let code = SetupCode::parse(code).unwrap();
        // The CLI's code is valid for an hour of real time; the fixture's clock starts in January.
        let digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM setup_code")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            digest.as_slice(),
            code.expose_secret().digest().as_bytes(),
            "only the digest of the printed code is stored"
        );
        fixture.claim(&Fixture::alice()).await.unwrap();
        let again = run(&mut abc(&["server", "setup-code"], &url(&pool)));
        assert_eq!(again.status.code(), Some(1), "refused once claimed");
        assert_eq!(
            text(&again.stderr),
            "abc: no setup code: this server has already been claimed\n",
            "the denial says why"
        );
    }

    #[tokio::test]
    async fn setup_code_refuses_an_unmigrated_database() {
        let database = TestDatabase::empty().await;
        let pool = database.pool();
        let output = run(&mut abc(&["server", "setup-code"], &url(&pool)));
        assert_eq!(output.status.code(), Some(1), "a failure to run");
        assert!(
            text(&output.stderr).contains("start `abc server`, which migrates it"),
            "{}",
            text(&output.stderr)
        );
    }

    #[test]
    fn configuration_errors_exit_2_and_never_echo_the_database_url() {
        let secret_url = "postgres://abc:hunter2@127.0.0.1:1/abc";
        let missing = run(&mut abc(&["server"], secret_url));
        assert_eq!(missing.status.code(), Some(2), "no public URL");
        assert!(
            text(&missing.stderr).contains("ABC_PUBLIC_URL is required"),
            "{}",
            text(&missing.stderr)
        );
        let mut wrong = abc(&["server"], secret_url);
        wrong.env("ABC_PUBLIC_URL", "http://abc.ts.net");
        let wrong = run(&mut wrong);
        assert_eq!(wrong.status.code(), Some(2), "not https");
        assert!(
            text(&wrong.stderr).contains("https://"),
            "{}",
            text(&wrong.stderr)
        );
        let mut filter = abc(&["server"], secret_url);
        filter
            .env("ABC_PUBLIC_URL", "https://abc.ts.net")
            .env("ABC_LOG", "[[[");
        let filter = run(&mut filter);
        assert_eq!(filter.status.code(), Some(2), "a bad log filter");
        let mut unreachable = abc(&["server", "setup-code"], secret_url);
        unreachable.env("ABC_DATABASE_WAIT", "0");
        let unreachable = run(&mut unreachable);
        assert_eq!(unreachable.status.code(), Some(1), "no database");
        for output in [&missing, &wrong, &filter, &unreachable] {
            assert!(
                !text(&output.stderr).contains("hunter2"),
                "the URL is never echoed"
            );
            assert!(
                !text(&output.stdout).contains("hunter2"),
                "the URL is never echoed"
            );
        }
    }

    /// The ops listener's answer to `GET /readyz`, or nothing when it does not answer.
    async fn readyz(ops: std::net::SocketAddr) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let Ok(mut stream) = tokio::net::TcpStream::connect(ops).await else {
            return String::new();
        };
        let _sent = stream
            .write_all(b"GET /readyz HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await;
        let mut answer = String::new();
        let _read = stream.read_to_string(&mut answer).await;
        answer
    }

    #[tokio::test]
    async fn the_server_runs_until_sigterm_and_exits_cleanly() {
        let database = TestDatabase::migrated().await;
        let pool = database.pool();
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("abc.sock");
        let ops = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut server = abc(&["server"], &url(&pool))
            .env("ABC_PUBLIC_URL", "https://abc.test.ts.net")
            .env("ABC_SIDECAR_SOCKET", &socket)
            .env("ABC_OPS_ADDR", ops.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Thirty seconds: the first run of a freshly linked binary can be slow to start (macOS
        // scans it), and a server that exits instead fails at once with its status.
        let mut ready = false;
        for _ in 0..1200 {
            if let Some(status) = server.try_wait().unwrap() {
                panic!("the server exited before it was ready: {status}");
            }
            if socket.exists() && readyz(ops).await.starts_with("HTTP/1.0 200") {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(ready, "the server binds its socket and reports ready");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            server.try_wait().unwrap().is_none(),
            "and keeps running until told to stop"
        );
        let killed = Command::new("kill")
            .args(["-TERM", &server.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success(), "SIGTERM was sent");
        let output = tokio::task::spawn_blocking(move || {
            let status = server.wait().unwrap();
            let mut stdout = String::new();
            std::io::Read::read_to_string(&mut server.stdout.take().unwrap(), &mut stdout).unwrap();
            (status, stdout)
        })
        .await
        .unwrap();
        assert!(output.0.success(), "a clean exit on SIGTERM: {}", output.1);
        assert!(
            output.1.contains("\"message\":\"draining"),
            "it drained: {}",
            output.1
        );
        assert!(
            output.1.contains("this server is unclaimed"),
            "an unclaimed server says how to claim it: {}",
            output.1
        );
    }

    /// `abc worker` with `args`, a clean environment and the worker home `home`.
    fn worker(args: &[&str], home: &std::path::Path) -> Command {
        let mut command = abc(&["worker"], "");
        command
            .env_remove("ABC_DATABASE_URL")
            .args(args)
            .arg("--home")
            .arg(home);
        command
    }

    /// A private, empty worker home.
    fn worker_home() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("worker");
        (dir, home)
    }

    const TOKEN: &str = "abc_enr_0000000000000000000000000000000000000000000000000000";

    #[test]
    fn a_worker_that_is_not_enrolled_says_how_to_enrol() {
        let (_dir, home) = worker_home();
        let output = run(&mut worker(&[], &home));
        assert_eq!(output.status.code(), Some(2), "configuration");
        assert_eq!(
            text(&output.stderr),
            "abc: this machine is not enrolled; run `abc worker setup <server-url>` first\n",
            "the fix"
        );
        let (_other_dir, other) = worker_home();
        let mut bad_filter = worker(&[], &other);
        bad_filter.env("ABC_LOG", "[[[");
        assert_eq!(
            run(&mut bad_filter).status.code(),
            Some(2),
            "a bad log filter"
        );
    }

    #[test]
    fn enrolment_refuses_a_bad_url_or_token_without_echoing_it() {
        let (_dir, home) = worker_home();
        let plain = run(&mut worker(&["enrol", "http://abc.ts.net"], &home));
        assert_eq!(plain.status.code(), Some(2), "not https");
        assert!(
            text(&plain.stderr).contains("it must start with https://"),
            "{}",
            text(&plain.stderr)
        );
        let mut env_token = worker(&["enrol", "https://abc.ts.net"], &home);
        env_token.env("ABC_ENROLMENT_TOKEN", "abc_enr_hunter2");
        let env_token = run(&mut env_token);
        assert_eq!(env_token.status.code(), Some(2), "a malformed token");
        assert!(
            text(&env_token.stderr).contains("malformed enrolment token"),
            "{}",
            text(&env_token.stderr)
        );
        let mut from_stdin = worker(&["enrol", "https://abc.ts.net"], &home);
        from_stdin
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = from_stdin.spawn().unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"hunter2\n").unwrap();
        let piped = child.wait_with_output().unwrap();
        assert_eq!(piped.status.code(), Some(2), "a malformed token on stdin");
        let mut hostname = worker(&["enrol", "https://abc.ts.net", "--hostname", " "], &home);
        hostname.env("ABC_ENROLMENT_TOKEN", TOKEN);
        let hostname = run(&mut hostname);
        assert_eq!(hostname.status.code(), Some(2), "an empty hostname");
        assert!(
            text(&hostname.stderr).contains("pass --hostname"),
            "{}",
            text(&hostname.stderr)
        );
        for output in [&plain, &env_token, &piped, &hostname] {
            assert!(!text(&output.stderr).contains("hunter2"), "never echoed");
            assert!(!text(&output.stdout).contains("hunter2"), "never echoed");
        }
    }

    #[test]
    fn a_token_file_is_read_and_never_echoed() {
        let (dir, home) = worker_home();
        let file = dir.path().join("token");
        std::fs::write(&file, "hunter2\n").unwrap();
        let output = run(&mut worker(
            &[
                "enrol",
                "https://abc.ts.net",
                "--token-file",
                file.to_str().unwrap(),
            ],
            &home,
        ));
        assert_eq!(
            output.status.code(),
            Some(2),
            "a malformed token in the file"
        );
        assert!(
            text(&output.stderr).contains("--token-file"),
            "says where tokens can come from: {}",
            text(&output.stderr)
        );
        assert!(!text(&output.stderr).contains("hunter2"), "never echoed");
        let missing = run(&mut worker(
            &[
                "enrol",
                "https://abc.ts.net",
                "--token-file",
                "/nonexistent/token",
            ],
            &home,
        ));
        assert_eq!(missing.status.code(), Some(2), "no such file");
        assert!(
            text(&missing.stderr).contains("cannot read the token file /nonexistent/token"),
            "{}",
            text(&missing.stderr)
        );
    }

    #[test]
    fn setup_refuses_a_bad_url_and_says_when_the_server_cannot_be_reached() {
        let (dir, home) = worker_home();
        let mut bad = worker(&["setup", "http://abc.ts.net"], &home);
        bad.env("HOME", dir.path());
        let bad = run(&mut bad);
        assert_eq!(bad.status.code(), Some(2), "not https");
        assert!(
            text(&bad.stderr).contains("it must start with https://"),
            "{}",
            text(&bad.stderr)
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("https://127.0.0.1:{port}");
        let mut command = worker(&["setup", &url], &home);
        command.env("HOME", dir.path());
        let output = run(&mut command);
        assert_eq!(output.status.code(), Some(2), "a machine to fix");
        let said = text(&output.stderr);
        assert!(
            said.contains(&format!("cannot reach {url}"))
                && said.contains("own Tailscale")
                && said.contains("tailscale status"),
            "says what is wrong and how to fix it: {said}"
        );
        assert!(
            !home.join("worker.toml").exists() || {
                let record = std::fs::read_to_string(home.join("worker.toml")).unwrap();
                !record.contains("[current")
            },
            "nothing is enrolled"
        );
    }

    #[test]
    fn enrolment_saves_its_key_before_it_reaches_for_the_server() {
        let (_dir, home) = worker_home();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("https://127.0.0.1:{port}");
        let mut command = worker(&["enrol", &url], &home);
        command.env("ABC_ENROLMENT_TOKEN", TOKEN);
        let output = run(&mut command);
        assert_eq!(output.status.code(), Some(1), "nothing answers there");
        assert!(
            text(&output.stderr)
                .starts_with("abc: could not reach the server; run the same command again"),
            "{}",
            text(&output.stderr)
        );
        let record = home.join("worker.toml");
        let mode = std::fs::metadata(&record).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the key is saved, privately, for the retry");
    }

    /// A private worker home holding an enrolment with the server at `server`.
    fn enrolled_home(server: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, home) = worker_home();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = home.join("worker.toml");
        std::fs::write(
            &record,
            format!(
                "format = 1\n\n[current.key]\nserver = \"{server}\"\nbasis = {{ token = \"{}\" }}\nkey = \"abc_wk_{}\"\n\n[current.worker]\nworker = \"00000000-0000-0000-0000-000000000001\"\nworkspace = \"00000000-0000-0000-0000-000000000002\"\nname = \"mbp\"\n",
                "0".repeat(64),
                "0".repeat(52)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, home)
    }

    #[test]
    fn setup_keeps_an_enrolment_with_the_same_server_and_refuses_another_without_replace() {
        let (dir, home) = enrolled_home("https://abc.ts.net");
        let mut same = worker(&["setup", "https://abc.ts.net"], &home);
        same.env("HOME", dir.path());
        let same = run(&mut same);
        assert!(
            text(&same.stdout)
                .contains("This machine is already enrolled as mbp with https://abc.ts.net"),
            "{}",
            text(&same.stdout)
        );
        let mut other = worker(&["setup", "https://other.ts.net"], &home);
        other.env("HOME", dir.path());
        let other = run(&mut other);
        assert_eq!(other.status.code(), Some(2));
        assert!(
            text(&other.stderr).contains("enrolled with https://abc.ts.net")
                && text(&other.stderr).contains("--replace"),
            "{}",
            text(&other.stderr)
        );
    }

    #[test]
    fn replace_goes_past_an_enrolment_with_the_same_or_another_server() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("https://127.0.0.1:{port}");
        for enrolled_with in [url.as_str(), "https://abc.ts.net"] {
            let (dir, home) = enrolled_home(enrolled_with);
            let mut command = worker(&["setup", &url, "--replace"], &home);
            command.env("HOME", dir.path());
            let output = run(&mut command);
            let said = text(&output.stderr);
            assert!(
                said.contains(&format!("cannot reach {url}")),
                "it tries to enrol anew ({enrolled_with}): {said} {}",
                text(&output.stdout)
            );
        }
    }

    /// A directory holding a `loginctl` that says nobody lingers, for `PATH`.
    fn path_with_loginctl(dir: &std::path::Path) -> std::path::PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let loginctl = bin.join("loginctl");
        std::fs::write(&loginctl, "#!/bin/sh\necho no\n").unwrap();
        std::fs::set_permissions(&loginctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn status_reports_the_enrolment_and_exits_nonzero_while_the_service_is_not_healthy() {
        let (dir, home) = enrolled_home("https://abc.ts.net");
        let bin = path_with_loginctl(dir.path());
        let mut status = worker(&["status"], &home);
        status.env("HOME", dir.path()).env("PATH", &bin);
        let status = run(&mut status);
        assert_eq!(
            status.status.code(),
            Some(1),
            "no service runs in this home"
        );
        let said = text(&status.stdout);
        assert!(
            said.contains(
                "enrolled as mbp (worker 00000000-0000-0000-0000-000000000001) with https://abc.ts.net"
            ) && said.contains("FAIL"),
            "{said}"
        );
        assert!(
            text(&status.stderr).contains("not healthy"),
            "{}",
            text(&status.stderr)
        );
        let (dir, home) = worker_home();
        let bin = path_with_loginctl(dir.path());
        let mut status = worker(&["status"], &home);
        status.env("HOME", dir.path()).env("PATH", &bin);
        let status = run(&mut status);
        assert_eq!(status.status.code(), Some(1), "not enrolled");
        assert!(
            text(&status.stdout).contains("not enrolled: run `abc worker setup <server-url>`"),
            "{}",
            text(&status.stdout)
        );
    }

    #[test]
    fn uninstall_keeps_the_home_and_purge_deletes_it() {
        let (dir, home) = enrolled_home("https://abc.ts.net");
        let mut keep = worker(&["uninstall"], &home);
        keep.env("HOME", dir.path());
        let keep = run(&mut keep);
        assert!(keep.status.success(), "{}", text(&keep.stderr));
        assert!(
            text(&keep.stdout).contains("was not installed"),
            "{}",
            text(&keep.stdout)
        );
        assert!(home.join("worker.toml").exists(), "the key stays");
        let mut purge = worker(&["uninstall", "--purge"], &home);
        purge.env("HOME", dir.path());
        let purge = run(&mut purge);
        assert!(purge.status.success(), "{}", text(&purge.stderr));
        assert!(!home.exists(), "purged");
    }

    #[tokio::test]
    async fn the_worker_runs_until_sigterm_and_exits_cleanly() {
        let (_dir, home) = worker_home();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let record = home.join("worker.toml");
        std::fs::write(
            &record,
            format!(
                "format = 1\n\n[current.key]\nserver = \"https://127.0.0.1:{port}\"\nbasis = {{ token = \"{}\" }}\nkey = \"abc_wk_{}\"\n\n[current.worker]\nworker = \"00000000-0000-0000-0000-000000000001\"\nworkspace = \"00000000-0000-0000-0000-000000000002\"\nname = \"mbp\"\n",
                "0".repeat(64),
                "0".repeat(52)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut running = worker(&[], &home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Nothing answers at the server's address, so the worker keeps backing off until told to
        // stop. Its first log line says it started.
        let stdout = running.stdout.take().unwrap();
        let (lines, logged) = std::sync::mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut log = String::new();
        while !log.contains("\"message\":\"starting\"") {
            let line = logged
                .recv_timeout(Duration::from_secs(30))
                .expect("the worker logs that it started");
            log.push_str(&line);
            log.push('\n');
        }
        let busy = run(&mut worker(&[], &home));
        assert_eq!(busy.status.code(), Some(2), "one worker per home");
        assert!(
            text(&busy.stderr).contains("another abc worker is already using"),
            "{}",
            text(&busy.stderr)
        );
        let killed = Command::new("kill")
            .args(["-TERM", &running.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success(), "SIGTERM was sent");
        let output = tokio::task::spawn_blocking(move || running.wait_with_output().unwrap())
            .await
            .unwrap();
        reader.join().unwrap();
        log.extend(logged.try_iter().map(|line| line + "\n"));
        assert!(
            output.status.success(),
            "a clean exit on SIGTERM: {}",
            text(&output.stderr)
        );
        assert!(log.contains("\"message\":\"stopped\""), "{log}");
        assert!(!log.contains(&"0".repeat(52)), "the key is never logged");
    }
}
