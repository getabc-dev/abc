//! The fakes in `tests/fakes` stand in for programs that `stack.rs` cannot run for real: service
//! managers, a browser, Tailscale, `sudo` and Postgres 18's programs. Each states in its header
//! what it models (environment, streams, exit codes), and a test here holds it to that: the
//! programs that exist everywhere, Postgres's, against the real ones, the others against what
//! their header says. (`crates/abc/tests/server_setup.rs` runs the real abc against the real
//! Postgres, and a real `launchctl` or `systemctl` runs on no CI runner of ours.)

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::{Duration, Instant};

    /// The fakes' sources.
    fn fakes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes")
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

    /// A scratch machine holding the fakes, with `state/` and `services/` beside them, as the fakes
    /// expect.
    fn machine() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        copy_tree(&fakes().join("bin"), &dir.path().join("bin"));
        copy_tree(&fakes().join("programs"), &dir.path().join("programs"));
        std::fs::create_dir(dir.path().join("state")).unwrap();
        std::fs::create_dir(dir.path().join("services")).unwrap();
        dir
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Every fake script: the files under `bin/` and `programs/`, but the library they share.
    fn scripts(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(scripts(&path));
            } else if path.file_name().is_some_and(|name| name != "lib.sh") {
                found.push(path);
            }
        }
        found.sort();
        found
    }

    /// The comment block under a script's `#!` line, as one line of text.
    fn header(script: &Path) -> String {
        std::fs::read_to_string(script)
            .unwrap()
            .lines()
            .skip(1)
            .take_while(|line| line.starts_with('#'))
            .map(|line| line.trim_start_matches('#').trim())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn every_fake_says_what_it_models() {
        let found: Vec<PathBuf> = ["bin", "programs"]
            .iter()
            .flat_map(|dir| scripts(&fakes().join(dir)))
            .collect();
        assert!(found.len() >= 16, "the fakes were not found: {found:?}");
        for script in found {
            let header = header(&script);
            for part in ["Models:", "Environment:", "Streams:", "Exit codes:"] {
                assert!(
                    header.contains(part),
                    "{} does not say `{part}` in its header, so nothing says what it stands for:\n{header}",
                    script.display()
                );
            }
        }
    }

    /// Runs the fake `program` of the machine `dir`, with exactly `env` as its environment.
    fn run(dir: &Path, program: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(dir.join(program))
            .args(args)
            .env_clear()
            .envs(env.iter().copied())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    }

    /// Waits until `file` holds something, which a service starts writing at once.
    fn log_of(file: &Path) -> String {
        let until = Instant::now().checked_add(Duration::from_secs(10)).unwrap();
        loop {
            let said = std::fs::read_to_string(file).unwrap_or_default();
            if !said.is_empty() || Instant::now() > until {
                return said;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_fake_launchctl_starts_an_agent_in_an_environment_of_its_own() {
        let machine = machine();
        let plist = machine.path().join("dev.abc.env.plist");
        std::fs::write(
            &plist,
            "<plist>\n<dict>\n<key>Label</key>\n<string>dev.abc.env</string>\n<key>ProgramArguments</key>\n<array>\n<string>/usr/bin/env</string>\n</array>\n<key>EnvironmentVariables</key>\n<dict>\n<key>ONLY_THIS</key>\n<string>1</string>\n</dict>\n</dict>\n</plist>\n",
        )
        .unwrap();
        // The caller's variables, which a launchd agent never sees.
        let started = run(
            machine.path(),
            "bin/launchctl",
            &["bootstrap", "gui/501", plist.to_str().unwrap()],
            &[("FROM_THE_CALLER", "1"), ("HOME", "/home/caller")],
        );
        assert!(started.status.success(), "{}", text(&started.stderr));
        let mut seen: Vec<String> = log_of(&machine.path().join("state/dev.abc.env.log"))
            .lines()
            .map(str::to_owned)
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            ["ONLY_THIS=1", "PATH=/usr/bin:/bin"],
            "the plist's variables and PATH alone: launchd gives an agent nothing of its caller's"
        );
        assert_eq!(text(&started.stdout), "", "a bootstrap prints nothing");

        // Exit codes and streams: errors on stderr with launchctl's codes.
        let again = run(
            machine.path(),
            "bin/launchctl",
            &["bootstrap", "gui/501", plist.to_str().unwrap()],
            &[],
        );
        assert_eq!(again.status.code(), Some(5), "an agent that is loaded");
        assert!(text(&again.stderr).contains("Bootstrap failed: 5"));
        assert_eq!(text(&again.stdout), "");
        let unknown = run(
            machine.path(),
            "bin/launchctl",
            &["print", "gui/501/dev.abc.none"],
            &[],
        );
        assert_eq!(unknown.status.code(), Some(113));
        let other = run(machine.path(), "bin/launchctl", &["frobnicate"], &[]);
        assert_eq!(other.status.code(), Some(64));
        let out = run(
            machine.path(),
            "bin/launchctl",
            &["bootout", "gui/501/dev.abc.env"],
            &[],
        );
        assert!(out.status.success(), "{}", text(&out.stderr));
        let gone = run(
            machine.path(),
            "bin/launchctl",
            &["bootout", "gui/501/dev.abc.env"],
            &[],
        );
        assert_eq!(gone.status.code(), Some(3), "an agent that is not loaded");
    }

    #[test]
    fn the_fake_systemctl_starts_a_unit_in_an_environment_of_its_own() {
        let machine = machine();
        std::fs::write(
            machine.path().join("services/abc-env.service"),
            "[Service]\nExecStart=/usr/bin/env\nEnvironment=\"ONLY_THIS=2\"\n",
        )
        .unwrap();
        let started = run(
            machine.path(),
            "bin/systemctl",
            &["--user", "start", "abc-env.service"],
            &[("FROM_THE_CALLER", "1")],
        );
        assert!(started.status.success(), "{}", text(&started.stderr));
        let mut seen: Vec<String> = log_of(&machine.path().join("state/abc-env.service.log"))
            .lines()
            .map(str::to_owned)
            .collect();
        seen.sort();
        assert_eq!(seen, ["ONLY_THIS=2", "PATH=/usr/bin:/bin"]);

        // `env` ended at once: the unit is not running, which is exit 3 and the word on stdout.
        let active = run(
            machine.path(),
            "bin/systemctl",
            &["--user", "is-active", "abc-env.service"],
            &[],
        );
        assert_eq!(active.status.code(), Some(3));
        assert_eq!(text(&active.stdout), "inactive\n");
        let system = run(machine.path(), "bin/systemctl", &["start", "x"], &[]);
        assert_eq!(system.status.code(), Some(2), "user units only");
        assert!(text(&system.stderr).contains("user units only"));
        let other = run(
            machine.path(),
            "bin/systemctl",
            &["--user", "frobnicate"],
            &[],
        );
        assert_eq!(other.status.code(), Some(64));
    }

    #[test]
    fn the_fake_sudo_and_loginctl_keep_to_what_their_headers_say() {
        let machine = machine();
        let linger = |env: &[(&str, &str)]| {
            run(
                machine.path(),
                "bin/loginctl",
                &["enable-linger", "someone"],
                env,
            )
        };
        let denied = linger(&[]);
        if Command::new("id").arg("-u").output().unwrap().stdout != b"0\n" {
            assert_eq!(denied.status.code(), Some(1), "without root");
            assert!(text(&denied.stderr).contains("Access denied"));
            assert_eq!(text(&denied.stdout), "");
        }
        let allowed = linger(&[("FAKE_ROOT", "1")]);
        assert!(allowed.status.success(), "{}", text(&allowed.stderr));
        let shown = run(machine.path(), "bin/loginctl", &["show-user", "x"], &[]);
        assert_eq!(text(&shown.stdout), "yes\n");
        // sudo runs a command with FAKE_ROOT=1 and the command's own exit status.
        let through = run(
            machine.path(),
            "bin/sudo",
            &["--", "sh", "-c", "echo $FAKE_ROOT; exit 7"],
            &[],
        );
        assert_eq!(through.status.code(), Some(7));
        assert_eq!(text(&through.stdout), "1\n");
        std::fs::write(machine.path().join("state/sudo-refuses"), "").unwrap();
        let refused = run(machine.path(), "bin/sudo", &["--", "true"], &[]);
        assert_eq!(refused.status.code(), Some(1));
        assert!(text(&refused.stderr).contains("incorrect password"));
        let tripwire = run(machine.path(), "bin/tailscale", &[], &[]);
        assert_eq!(tripwire.status.code(), Some(1));
        assert_eq!(text(&tripwire.stdout), "");
        for opener in ["bin/open", "bin/xdg-open"] {
            let opened = run(machine.path(), opener, &["https://example.com"], &[]);
            assert!(opened.status.success());
            assert_eq!(text(&opened.stdout), "");
        }
    }

    #[test]
    fn the_fake_tailscaled_wants_a_log_directory_of_its_own() {
        let machine = machine();
        let without = run(machine.path(), "programs/tailscaled", &[], &[]);
        assert_eq!(without.status.code(), Some(1));
        assert!(text(&without.stderr).contains("using system state directory"));
        assert_eq!(text(&without.stdout), "");
        let no_socket = run(
            machine.path(),
            "programs/tailscale",
            &["status", "--json"],
            &[],
        );
        assert_eq!(no_socket.status.code(), Some(2), "--socket comes first");
        let no_daemon = run(
            machine.path(),
            "programs/tailscale",
            &["--socket=/none", "status", "--json"],
            &[],
        );
        assert_eq!(no_daemon.status.code(), Some(1), "no daemon runs");
        assert!(text(&no_daemon.stderr).contains("failed to connect to local tailscaled"));
    }

    /// Postgres 18's bin directory, which `cargo xtask check` gives the tests: an absolute path, never
    /// one from `PATH`.
    fn real_postgres() -> PathBuf {
        let value = std::env::var_os("ABC_TEST_POSTGRES_BIN").unwrap_or_else(|| {
            panic!(
                "ABC_TEST_POSTGRES_BIN is not set; run the tests through `cargo xtask check`, which \
                 gives them the pinned Postgres 18"
            )
        });
        let dir = PathBuf::from(value);
        assert!(
            dir.is_absolute(),
            "ABC_TEST_POSTGRES_BIN is `{}`, a relative path, and the tests run from their crate's \
             directory: give an absolute one",
            dir.display()
        );
        assert!(
            dir.join("initdb").is_file(),
            "ABC_TEST_POSTGRES_BIN is `{}`, which holds no `initdb`",
            dir.display()
        );
        dir
    }

    /// What the fake `initdb` and `postgres --version` do, which the real ones do too: both are run
    /// here with the same arguments and judged by the same assertions.
    fn assert_postgres_contract(initdb: &Path, postgres: &Path) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let initialised = Command::new(initdb)
            .args([
                "--pgdata",
                data.to_str().unwrap(),
                "--no-sync",
                "-A",
                "trust",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            initialised.status.success(),
            "{}",
            text(&initialised.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(data.join("PG_VERSION"))
                .unwrap()
                .trim(),
            "18",
            "a cluster that says it is 18"
        );
        let without = Command::new(initdb)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert_eq!(
            without.status.code(),
            Some(1),
            "no data directory is an error"
        );
        assert!(!text(&without.stderr).is_empty(), "said on stderr");
        let version = Command::new(postgres)
            .arg("--version")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(version.status.success(), "{}", text(&version.stderr));
        assert!(
            text(&version.stdout).starts_with("postgres (PostgreSQL) 18"),
            "{}",
            text(&version.stdout)
        );
    }

    #[test]
    fn the_fake_postgres_programs_do_what_the_real_ones_do() {
        let machine = machine();
        let fake = machine.path().join("programs/pg");
        assert_postgres_contract(&fake.join("initdb"), &fake.join("postgres"));
        let real = real_postgres();
        assert_postgres_contract(&real.join("initdb"), &real.join("postgres"));

        // What only the fake can be asked: without LC_ALL it fails as the macOS postmaster does.
        let unlocaled = run(machine.path(), "programs/pg/postgres", &["-D", "x"], &[]);
        assert_eq!(unlocaled.status.code(), Some(1));
        assert!(text(&unlocaled.stderr).contains("postmaster became multithreaded"));
        // No server runs, so pg_isready fails, as the real one's exit 1 or 2 does.
        let isready = run(machine.path(), "programs/pg/pg_isready", &[], &[]);
        assert!(!isready.status.success());
        let real_isready = Command::new(real.join("pg_isready"))
            .args(["--host", "127.0.0.1", "--port", "1", "--timeout", "1"])
            .env_clear()
            .output()
            .unwrap();
        assert!(
            !real_isready.status.success(),
            "no server answers on port 1"
        );
        assert_eq!(text(&isready.stdout), "", "the fake prints nothing");
    }
}
