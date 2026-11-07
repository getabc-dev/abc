//! `abc server setup` and the day-2 commands end to end: the real `abc` binary sets up the real
//! Postgres 18 and runs the real `abc server` under this OS's service manager, played by the fakes
//! in `crates/abc-host/tests/fakes` (which run what each service definition says), with a fake
//! tailscaled and tailscale CLI, sudo, loginctl and browser. The setup code it opens the claim page
//! with is checked against the real database.
//!
//! Postgres 18's programs come from `ABC_TEST_POSTGRES_BIN`, else from where Debian's
//! `postgresql-18` or Homebrew's `postgresql@18` puts them.

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use abc_core::credential::SetupCode;
    use abc_store::testing::Fixture;
    use sqlx::PgPool;

    /// Postgres 18's bin directory on this machine.
    fn postgres_bin() -> PathBuf {
        if let Some(dir) = std::env::var_os("ABC_TEST_POSTGRES_BIN") {
            return PathBuf::from(dir);
        }
        [
            "/usr/lib/postgresql/18/bin",
            "/opt/homebrew/opt/postgresql@18/bin",
            "/usr/local/opt/postgresql@18/bin",
            "/home/linuxbrew/.linuxbrew/opt/postgresql@18/bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|dir| dir.join("initdb").is_file())
        .unwrap_or_else(|| {
            panic!(
                "these tests run abc's private Postgres for real: install Postgres 18 \
                 (apt install postgresql-18, or brew install postgresql@18), or set \
                 ABC_TEST_POSTGRES_BIN to its bin directory"
            )
        })
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    /// A scratch machine: the fakes, the user's home, abc's server home and the configuration
    /// naming the real abc, the real Postgres and the fake Tailscale.
    struct Machine {
        dir: tempfile::TempDir,
    }

    impl Machine {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let fakes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../abc-host/tests/fakes");
            copy_tree(&fakes.join("bin"), &root.join("bin"));
            copy_tree(&fakes.join("programs"), &root.join("programs"));
            for made in ["state", "services", "user/Library", "user/.config/systemd"] {
                std::fs::create_dir_all(root.join(made)).unwrap();
            }
            // Both service directories are the one the fakes read.
            std::os::unix::fs::symlink(
                root.join("services"),
                root.join("user/Library/LaunchAgents"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                root.join("services"),
                root.join("user/.config/systemd/user"),
            )
            .unwrap();
            let home = root.join("h");
            std::fs::create_dir(&home).unwrap();
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
            let ops = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            std::fs::write(
                home.join("config.toml"),
                format!(
                    "format = 1\nops_addr = \"{ops}\"\n\n[programs]\nabc = \"{abc}\"\n\
                     tailscaled = \"{root}/programs/tailscaled\"\n\
                     tailscale = \"{root}/programs/tailscale\"\npostgres = \"{postgres}\"\n",
                    abc = env!("CARGO_BIN_EXE_abc"),
                    root = root.display(),
                    postgres = postgres_bin().display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(
                home.join("config.toml"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            Self { dir }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn home(&self) -> PathBuf {
            self.root().join("h")
        }

        /// `abc server <args>` as the person runs it here.
        fn abc(&self, args: &[&str]) -> Output {
            let mut command = Command::new(env!("CARGO_BIN_EXE_abc"));
            command
                .arg("server")
                .args(args)
                .env_clear()
                .env("HOME", self.root().join("user"))
                .env("ABC_SERVER_HOME", self.home())
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", self.root().join("bin").display()),
                );
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
            command.output().unwrap()
        }

        fn calls(&self) -> Vec<String> {
            let calls =
                std::fs::read_to_string(self.root().join("state/calls")).unwrap_or_default();
            std::fs::write(self.root().join("state/calls"), "").unwrap();
            calls.lines().map(str::to_owned).collect()
        }

        /// abc's database, through the URL setup gave abc server.
        async fn pool(&self) -> PgPool {
            let url = format!(
                "postgres://abc@localhost/abc?host={}&port=5432",
                self.home()
                    .join("run")
                    .display()
                    .to_string()
                    .replace('/', "%2F")
            );
            PgPool::connect(&url).await.unwrap()
        }
    }

    impl Drop for Machine {
        /// Stops whatever the fake service managers left running, Postgres last and gently.
        fn drop(&mut self) {
            let Ok(entries) = std::fs::read_dir(self.root().join("state")) else {
                return;
            };
            let mut pids: Vec<(bool, String)> = entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pid"))
                .filter_map(|entry| {
                    let postgres = entry.file_name().to_string_lossy().contains("postgres");
                    std::fs::read_to_string(entry.path())
                        .ok()
                        .map(|pid| (postgres, pid.trim().to_owned()))
                })
                .collect();
            pids.sort();
            for (postgres, pid) in pids {
                let signal = if postgres { "-INT" } else { "-KILL" };
                let _signalled = Command::new("kill").args([signal, &pid]).status();
            }
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn succeeded(output: &Output) -> String {
        assert!(
            output.status.success(),
            "{}\n{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    #[tokio::test]
    async fn setup_runs_the_real_stack_and_opens_a_claim_page_with_a_code_the_database_holds() {
        let machine = Machine::new();
        let said = succeeded(&machine.abc(&["setup"]));
        let calls = machine.calls();
        let browser = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let claim = calls
            .iter()
            .find_map(|call| {
                call.strip_prefix(&format!("{browser} https://abc.tailfake.ts.net/#code="))
            })
            .unwrap_or_else(|| panic!("the claim page was opened: {calls:#?}"));
        assert!(
            said.contains(&format!("https://abc.tailfake.ts.net/#code={claim}")),
            "and printed: {said}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("sudo "))
                .count(),
            1,
            "one sudo: {calls:#?}"
        );
        let pool = machine.pool().await;
        let digest: Vec<u8> = sqlx::query_scalar("SELECT digest FROM setup_code")
            .fetch_one(&pool)
            .await
            .unwrap();
        let code = SetupCode::parse(claim).unwrap();
        assert_eq!(
            digest.as_slice(),
            code.expose_secret().digest().as_bytes(),
            "the page's code is the server's"
        );
        let mode = std::fs::metadata(machine.home().join("postgres"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the cluster is private");

        let status = succeeded(&machine.abc(&["status"]));
        assert!(!status.contains("FAIL"), "{status}");
        assert!(status.contains("ok    abc server"), "{status}");

        let again = succeeded(&machine.abc(&["setup"]));
        let second = machine.calls();
        assert!(
            !second
                .iter()
                .any(|call| call.starts_with("sudo") || call.starts_with("initdb")),
            "nothing asked or made again: {second:#?}"
        );
        assert!(
            again.contains("/#code="),
            "a fresh code for the unclaimed server: {again}"
        );

        let printed = succeeded(&machine.abc(&["setup-code"]));
        assert!(
            printed.starts_with("Setup code (valid until "),
            "setup-code finds abc's own database: {printed}"
        );

        backup_and_restore(&machine, &pool).await;

        succeeded(&machine.abc(&["upgrade"]));
        let after = succeeded(&machine.abc(&["status"]));
        assert!(!after.contains("FAIL"), "{after}");

        pool.close().await;
        let removed = succeeded(&machine.abc(&["uninstall"]));
        assert!(removed.contains("admin/machines"), "{removed}");
        assert!(
            machine.home().join("postgres/PG_VERSION").exists(),
            "data kept"
        );
        let down = machine.abc(&["status"]);
        assert_eq!(down.status.code(), Some(1), "unhealthy once uninstalled");
        succeeded(&machine.abc(&["uninstall", "--purge"]));
        assert!(!machine.home().exists(), "purged");
    }

    /// A backup restores into the unclaimed server; a claimed one needs --force.
    async fn backup_and_restore(machine: &Machine, pool: &PgPool) {
        let file = machine.root().join("before-claim.dump");
        let wrote = succeeded(&machine.abc(&["backup", file.to_str().unwrap()]));
        assert!(
            wrote.starts_with(&format!("Wrote {}", file.display())),
            "{wrote}"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600,
            "private"
        );
        let again = machine.abc(&["backup", file.to_str().unwrap()]);
        assert_eq!(again.status.code(), Some(2), "never overwritten");

        // Claimed now: a restore would replace the owner.
        let fixture = Fixture::new(pool.clone()).await;
        fixture.claim(&Fixture::alice()).await.unwrap();
        let claimed = succeeded(&machine.abc(&["setup"]));
        assert!(
            claimed.contains("abc is already claimed; open https://abc.tailfake.ts.net"),
            "no claim page for a claimed server: {claimed}"
        );
        let bogus = machine.root().join("bogus.dump");
        std::fs::write(&bogus, "not a dump").unwrap();
        let unreadable = machine.abc(&["restore", "--force", bogus.to_str().unwrap()]);
        assert_eq!(
            unreadable.status.code(),
            Some(1),
            "{}",
            text(&unreadable.stderr)
        );
        assert!(
            text(&unreadable.stderr).contains("name a file abc server backup wrote"),
            "{}",
            text(&unreadable.stderr)
        );
        let refused = machine.abc(&["restore", file.to_str().unwrap()]);
        assert_eq!(refused.status.code(), Some(2), "{}", text(&refused.stderr));
        assert!(
            text(&refused.stderr).contains("pass --force"),
            "{}",
            text(&refused.stderr)
        );
        succeeded(&machine.abc(&["restore", "--force", file.to_str().unwrap()]));
        let restored = machine.pool().await;
        let workspaces: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace")
            .fetch_one(&restored)
            .await
            .unwrap();
        assert_eq!(workspaces, 0, "the unclaimed server is back");
        restored.close().await;
    }

    /// `abc <args>` with a clean environment and only `env`.
    fn bare(args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abc"));
        command.args(args).env_clear();
        for (name, value) in env {
            command.env(name, value);
        }
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command.output().unwrap()
    }

    #[test]
    fn commands_before_setup_say_to_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("server");
        let env = [("HOME", dir.path()), ("ABC_SERVER_HOME", home.as_path())];
        let code = bare(&["server", "setup-code"], &env);
        assert_eq!(code.status.code(), Some(2), "{}", text(&code.stderr));
        assert!(
            text(&code.stderr).contains("run abc server setup, or set ABC_DATABASE_URL"),
            "{}",
            text(&code.stderr)
        );
        for args in [
            &["server", "status"][..],
            &["server", "backup"],
            &["server", "upgrade"],
        ] {
            let output = bare(args, &env);
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            assert!(
                text(&output.stderr).ends_with("run abc server setup\n"),
                "{args:?}: {}",
                text(&output.stderr)
            );
        }
        let restore = bare(&["server", "restore", "x.dump"], &env);
        assert_eq!(restore.status.code(), Some(2), "{}", text(&restore.stderr));
        let nothing = bare(&["server", "uninstall"], &env);
        assert!(nothing.status.success(), "{}", text(&nothing.stderr));
        let homeless = bare(&["server", "status"], &[]);
        assert_eq!(homeless.status.code(), Some(2), "no HOME");
        assert!(
            text(&homeless.stderr).contains("set HOME"),
            "{}",
            text(&homeless.stderr)
        );
    }

    #[test]
    fn the_server_needs_its_socket_named() {
        let output = bare(&["server"], &[]).status.code();
        assert_eq!(output, Some(2), "a configuration error");
        let named = bare(
            &["server"],
            &[
                ("ABC_PUBLIC_URL", Path::new("https://abc.example.ts.net")),
                (
                    "ABC_DATABASE_URL",
                    Path::new("postgres://abc@127.0.0.1:1/abc"),
                ),
            ],
        );
        assert_eq!(named.status.code(), Some(2), "{}", text(&named.stderr));
        assert!(
            text(&named.stderr).contains("ABC_SIDECAR_SOCKET is required to run the server"),
            "{}",
            text(&named.stderr)
        );
    }
}
