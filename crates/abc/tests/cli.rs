//! The `abc` binary end to end: `abc server setup-code` against a real database, configuration
//! errors and their exit codes, and `abc server` stopping cleanly on SIGTERM.

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Output, Stdio};
    use std::time::Duration;

    use abc_core::credential::SetupCode;
    use abc_store::testing::Fixture;
    use sqlx::PgPool;

    /// The test database's URL; the container trusts local connections.
    fn url(pool: &PgPool) -> String {
        let options = pool.connect_options();
        format!(
            "postgres://{}@{}:{}/{}",
            options.get_username(),
            options.get_host(),
            options.get_port(),
            options.get_database().unwrap(),
        )
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

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn setup_code_prints_a_code_that_claims_the_server_once(pool: PgPool) {
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
        assert!(
            text(&again.stderr).contains("already claimed"),
            "{}",
            text(&again.stderr)
        );
    }

    #[sqlx::test(migrations = false)]
    async fn setup_code_refuses_an_unmigrated_database(pool: PgPool) {
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

    #[sqlx::test(migrator = "abc_store::MIGRATOR")]
    async fn the_server_runs_until_sigterm_and_exits_cleanly(pool: PgPool) {
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
        let mut ready = false;
        for _ in 0..200 {
            if socket.exists() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(ready, "the server binds its socket");
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
}
