//! `abc server setup` and the day-2 commands end to end: the real `abc` binary sets up the real
//! Postgres 18 and runs the real `abc server` under this OS's service manager, played by the fakes
//! in `crates/abc-host/tests/fakes` (which run what each service definition says), with a fake
//! tailscaled and tailscale CLI, sudo, loginctl and browser. The setup code it opens the claim page
//! with is checked against the real database.
//!
//! Postgres 18's programs are the pinned `postgresql` tool's, which xtask names in
//! `ABC_TEST_POSTGRES_BIN`.

#[cfg(test)]
#[expect(
    dead_code,
    reason = "the shared harness; the fakes leave its real-Tailscale variant unused"
)]
mod common;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Output};

    use abc_core::credential::SetupCode;
    use abc_store::testing::Fixture;
    use sqlx::PgPool;

    use crate::common::{Machine, Tailscale, programs_dir};

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
        let machine = Machine::new(&Tailscale::Fakes);
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
        let quoted = machine.root().join("Ada's backup.dump");
        let said = succeeded(&machine.abc(&["backup", quoted.to_str().unwrap()]));
        assert!(
            said.contains(&format!(
                "Restore it with: abc server restore '{}'",
                quoted.display().to_string().replace('\'', r"'\''")
            )),
            "a command a shell reads back whole: {said}"
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
        // A home that is not abc's is never purged: a mistake to fix, so exit 2.
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("precious"), "keep me").unwrap();
        let refused = bare(&["server", "uninstall", "--purge"], &env);
        assert_eq!(refused.status.code(), Some(2), "{}", text(&refused.stderr));
        assert!(home.join("precious").exists(), "kept");
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

    /// A directory holding `program`, and its path as a string.
    fn programs(program: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(program), "").unwrap();
        let path = dir.path().to_str().unwrap().to_owned();
        (dir, path)
    }

    #[test]
    fn a_programs_directory_is_taken_when_it_holds_the_program() {
        let (_dir, path) = programs("initdb");
        assert_eq!(
            programs_dir("BIN", Some(path.clone().into()), &["initdb"]),
            Ok(path.into())
        );
    }

    #[test]
    fn a_programs_directory_unset_is_named_unset() {
        assert_eq!(
            programs_dir("BIN", None, &["initdb"]),
            Err("BIN is not set".to_owned())
        );
    }

    #[test]
    fn a_relative_programs_directory_is_named_relative() {
        // As the real-tailnet workflow once gave it: relative to the repository root, while the
        // tests run from their crate's directory.
        assert_eq!(
            programs_dir("BIN", Some(".tools/postgresql/bin".into()), &["initdb"]),
            Err(
                "BIN is `.tools/postgresql/bin`, a relative path, and the tests run from their \
                 crate's directory: give an absolute one"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_programs_directory_that_does_not_exist_is_named_missing() {
        let (dir, path) = programs("initdb");
        drop(dir);
        assert_eq!(
            programs_dir("BIN", Some(path.clone().into()), &["initdb"]),
            Err(format!("BIN is `{path}`, which is not a directory"))
        );
    }

    #[test]
    fn a_programs_directory_without_a_program_names_it() {
        let (_dir, path) = programs("tailscaled");
        assert_eq!(
            programs_dir(
                "BIN",
                Some(path.clone().into()),
                &["tailscaled", "tailscale"]
            ),
            Err(format!("BIN is `{path}`, which holds no `tailscale`"))
        );
    }
}
