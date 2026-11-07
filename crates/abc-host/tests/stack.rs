//! `abc server setup` and the day-2 commands against fakes, on both service managers: fake
//! launchctl and systemctl that run what each definition says, fake loginctl, sudo and browser, a
//! fake tailscaled and tailscale CLI with a login and Serve, fake Postgres 18 programs, and a
//! fake abc server whose ops address the test answers while it runs. Every fake records its
//! calls in `state/calls`. (`crates/abc/tests/server_setup.rs` runs the real abc and the real
//! Postgres 18.)

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use abc_host::config::Config;
    use abc_host::programs::Programs;
    use abc_host::{Console, Host, HostError, Os, ServerHome, SetupOptions, Timing};

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

    /// A scratch machine: the fakes, their state, the service directory and abc's home.
    struct Machine {
        dir: tempfile::TempDir,
        os: Os,
        ops: SocketAddr,
    }

    impl Machine {
        fn new(os: Os) -> Self {
            let dir = tempfile::tempdir().unwrap();
            copy_tree(&fakes().join("bin"), &dir.path().join("bin"));
            copy_tree(&fakes().join("programs"), &dir.path().join("programs"));
            std::fs::create_dir(dir.path().join("state")).unwrap();
            let ops = answer_while_abc_runs(dir.path().join("state"));
            let machine = Self { dir, os, ops };
            let mut config = Config::new(machine.programs());
            config.ops_addr = ops;
            machine.home().create().unwrap();
            config.save(&machine.home()).unwrap();
            machine
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn programs(&self) -> Programs {
            let programs = self.root().join("programs");
            Programs {
                abc: programs.join("abc"),
                tailscaled: programs.join("tailscaled"),
                tailscale: programs.join("tailscale"),
                postgres: programs.join("pg"),
            }
        }

        fn home(&self) -> ServerHome {
            ServerHome::at(self.root().join("home"))
        }

        fn host(&self) -> Host {
            Host {
                os: self.os,
                home: self.home(),
                services_dir: self.root().join("services"),
                uid: 501,
                path: self.root().join("bin").into_os_string(),
                timing: Timing {
                    poll: Duration::from_millis(20),
                    postgres: Duration::from_secs(10),
                    tailscaled: Duration::from_secs(10),
                    login: Duration::from_secs(10),
                    server: Duration::from_secs(10),
                    service: Duration::from_secs(5),
                },
            }
        }

        fn state(&self, name: &str) -> PathBuf {
            self.root().join("state").join(name)
        }

        /// Every call the fakes recorded, one a line, and forgets them.
        fn take_calls(&self) -> Vec<String> {
            let calls = std::fs::read_to_string(self.state("calls")).unwrap_or_default();
            std::fs::write(self.state("calls"), "").unwrap();
            calls.lines().map(str::to_owned).collect()
        }

        fn setup(&self, auth_key: Option<PathBuf>) -> (Result<abc_host::Stack, HostError>, String) {
            let mut said = Vec::new();
            let result = abc_host::setup(
                &self.host(),
                &SetupOptions {
                    exe: PathBuf::from("/nonexistent/bin/abc"),
                    auth_key,
                },
                &mut Console::new(&mut said),
            );
            (result, String::from_utf8(said).unwrap())
        }

        fn alive(&self, daemon: &str) -> bool {
            std::fs::read_to_string(self.state(daemon)).is_ok_and(|pid| {
                std::process::Command::new("kill")
                    .args(["-0", pid.trim()])
                    .status()
                    .unwrap()
                    .success()
            })
        }

        fn socket(&self, name: &str) -> String {
            self.home().run().join(name).display().to_string()
        }
    }

    impl Drop for Machine {
        /// Stops whatever the fake service managers left running.
        fn drop(&mut self) {
            let Ok(entries) = std::fs::read_dir(self.root().join("state")) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("pid" | "daemon")
                ) && let Ok(pid) = std::fs::read_to_string(&path)
                {
                    let _killed = std::process::Command::new("kill")
                        .args(["-9", pid.trim()])
                        .status();
                }
            }
        }
    }

    /// An ops address answering `/readyz` with 200 while the fake abc server runs, else 503.
    fn answer_while_abc_runs(state: PathBuf) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0_u8; 256];
                let _read = stream.read(&mut request);
                let running = std::fs::read_to_string(state.join("abc.daemon")).is_ok_and(|pid| {
                    std::process::Command::new("kill")
                        .args(["-0", pid.trim()])
                        .status()
                        .is_ok_and(|status| status.success())
                });
                let answer: &[u8] = if running {
                    b"HTTP/1.1 200 OK\r\n\r\n{\"status\":\"ready\"}"
                } else {
                    b"HTTP/1.1 503 Service Unavailable\r\n\r\n{\"status\":\"starting\"}"
                };
                let _written = stream.write_all(answer);
            }
        });
        addr
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The whole of setup, then setup again, on `os`.
    fn setup_and_again(os: Os) {
        let machine = Machine::new(os);
        let (stack, said) = machine.setup(None);
        let stack = stack.unwrap_or_else(|error| panic!("{error}\n{said}"));
        first_setup_ran_the_stack(&machine, &stack, &said);
        assert_eq!(
            abc_host::database_url(&machine.host()).unwrap(),
            stack.database_url,
            "the day-2 commands reach the database setup made"
        );
        let report = abc_host::status(&machine.host()).unwrap();
        assert!(report.healthy(), "{:#?}", report.lines());
        second_setup_changes_nothing(&machine);
    }

    /// What the first setup on `machine` ran, and what it left.
    fn first_setup_ran_the_stack(machine: &Machine, stack: &abc_host::Stack, said: &str) {
        let os = machine.os;
        assert_eq!(
            stack.public_url.origin(),
            "https://abc.tailfake.ts.net",
            "the node's name"
        );
        let run = machine.home().run();
        assert!(
            stack
                .database_url
                .starts_with("postgres://abc@localhost/abc?host=%2F"),
            "{}",
            stack.database_url
        );
        let calls = machine.take_calls();
        let tailscale_socket = format!("--socket={}", machine.socket("tailscaled.sock"));
        for call in calls.iter().filter(|call| call.starts_with("tailscale ")) {
            assert!(
                call.starts_with(&format!("tailscale {tailscale_socket} ")),
                "every CLI call names abc's socket: {call}"
            );
        }
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("the user's tailscale")),
            "the PATH's tailscale is never run: {calls:#?}"
        );
        let initdb: Vec<&String> = calls
            .iter()
            .filter(|call| call.starts_with("initdb "))
            .collect();
        assert_eq!(
            initdb,
            [&format!(
                "initdb --pgdata {} --username abc --auth-local=trust --auth-host=reject \
                 --encoding=UTF8 --no-locale",
                machine.home().postgres().display()
            )],
            "one cluster, trusted on its socket only"
        );
        assert!(
            calls.contains(&format!(
                "postgres -D {} -p 5432 -c listen_addresses= -c unix_socket_directories=\"{}\" \
                 -c unix_socket_permissions=0700",
                machine.home().postgres().display(),
                run.display()
            )),
            "the service runs Postgres with no TCP: {calls:#?}"
        );
        assert!(
            calls.contains(&format!(
                "tailscaled --tun=userspace-networking --statedir={} {tailscale_socket} --port=0",
                machine.home().tailscale().display()
            )),
            "the service runs abc's tailscaled: {calls:#?}"
        );
        assert!(
            calls.contains(&"psql CREATE DATABASE abc".to_owned()),
            "the database"
        );
        assert!(
            calls.contains(&format!("tailscale {tailscale_socket} up --hostname=abc")),
            "up as abc"
        );
        let browser = match os {
            Os::MacOs => "open",
            Os::Linux => "xdg-open",
        };
        assert!(
            calls.contains(&format!("{browser} https://login.tailscale.com/a/fake")),
            "the login link opened: {calls:#?}"
        );
        assert!(
            said.contains("Log abc's node in to your tailnet in the browser:\n  https://login.tailscale.com/a/fake"),
            "and printed: {said}"
        );
        root_was_asked_once(machine, &calls, said);
        assert!(
            calls.contains(&format!(
                "abc server --sidecar-socket {} --ops-addr {} --public-url https://abc.tailfake.ts.net",
                machine.socket("abc.sock"),
                machine.ops
            )),
            "the server's service: {calls:#?}"
        );
        the_stack_runs_privately(machine, stack);
    }

    /// The services run, and everything setup wrote is private.
    fn the_stack_runs_privately(machine: &Machine, stack: &abc_host::Stack) {
        let os = machine.os;
        assert_eq!(
            std::fs::read_to_string(machine.state("abc.environment")).unwrap(),
            format!("ABC_DATABASE_URL={}\n", stack.database_url),
            "the server's environment, through the service manager"
        );
        for daemon in ["postgres.daemon", "tailscaled.daemon", "abc.daemon"] {
            assert!(machine.alive(daemon), "{daemon} runs under its service");
        }
        let names: Vec<String> = abc_host::services::Part::ALL
            .iter()
            .map(|part| part.service_name(os))
            .collect();
        for name in &names {
            let file = match os {
                Os::MacOs => format!("{name}.plist"),
                Os::Linux => name.clone(),
            };
            assert_eq!(
                mode(&machine.root().join("services").join(file)),
                0o600,
                "{name}"
            );
        }
        assert_eq!(mode(machine.home().dir()), 0o700, "the home is private");
        assert_eq!(mode(&machine.home().config()), 0o600, "so is its config");
        let config = Config::require(&machine.home()).unwrap();
        assert_eq!(
            config
                .public_url
                .as_ref()
                .map(abc_protocol::url::PublicUrl::origin),
            Some("https://abc.tailfake.ts.net"),
            "the public URL is kept"
        );
    }

    /// The one sudo of the first setup, and what it said before.
    fn root_was_asked_once(machine: &Machine, calls: &[String], said: &str) {
        let tailscale = machine.programs().tailscale;
        let tailscale_socket = format!("--socket={}", machine.socket("tailscaled.sock"));
        let os = machine.os;
        let sudo: Vec<&String> = calls
            .iter()
            .filter(|call| call.starts_with("sudo "))
            .collect();
        let serve = format!(
            "{} {tailscale_socket} serve --bg unix:{}",
            tailscale.display(),
            machine.socket("abc.sock")
        );
        let expected = match os {
            Os::MacOs => format!("sudo -- {serve}"),
            Os::Linux => format!(
                "sudo -- /bin/sh -c \"$1\" enable-linger \"$2\" && shift 2 && exec \"$@\" \
                 abc-setup {} 501 {serve}",
                machine.root().join("bin/loginctl").display()
            ),
        };
        assert_eq!(sudo, [&expected], "exactly one sudo, of exactly this");
        assert!(
            said.contains("abc needs your administrator password once")
                && said.contains("Nothing else runs as root."),
            "said what root does first: {said}"
        );
        let sudo_at = said.find("The command: ").unwrap();
        assert!(
            said.find("abc server is ready").unwrap() > sudo_at,
            "in order: {said}"
        );
    }

    /// Setup again repairs nothing, because nothing is broken, and asks nothing.
    fn second_setup_changes_nothing(machine: &Machine) {
        let (again, said_again) = machine.setup(None);
        again.unwrap_or_else(|error| panic!("{error}\n{said_again}"));
        let calls = machine.take_calls();
        for word in ["initdb", "sudo", "open", "xdg-open", "abc "] {
            assert!(
                !calls.iter().any(|call| call.starts_with(word)),
                "no {word} again: {calls:#?}"
            );
        }
        assert!(
            !calls.iter().any(|call| call.contains(" up ")
                || call.contains("bootout")
                || call.contains("restart")
                || call.contains("CREATE")),
            "nothing restarted or created again: {calls:#?}"
        );
        assert!(
            !said_again.contains("password"),
            "no root step: {said_again}"
        );
    }

    #[test]
    fn setup_on_macos_runs_launchd_agents_and_asks_root_once_for_serve() {
        setup_and_again(Os::MacOs);
    }

    #[test]
    fn setup_on_linux_runs_systemd_units_and_asks_root_once_for_serve_and_lingering() {
        setup_and_again(Os::Linux);
    }

    #[test]
    fn a_missing_private_tailscale_fails_with_the_fix_and_never_runs_the_paths() {
        let machine = Machine::new(Os::MacOs);
        std::fs::remove_file(machine.programs().tailscale).unwrap();
        let (result, _said) = machine.setup(None);
        let error = result.unwrap_err();
        assert!(
            matches!(
                &error,
                HostError::ProgramMissing {
                    key: "tailscale",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("set `tailscale` under [programs] in"),
            "names the fix: {error}"
        );
        let calls = machine.take_calls();
        assert!(
            calls.is_empty(),
            "nothing ran, the user's tailscale least of all: {calls:#?}"
        );
    }

    #[test]
    fn a_home_too_long_for_a_socket_fails_before_anything_is_made() {
        let machine = Machine::new(Os::MacOs);
        let mut host = machine.host();
        host.home = ServerHome::at(machine.root().join("a".repeat(120)));
        let mut said = Vec::new();
        let error = abc_host::setup(
            &host,
            &SetupOptions {
                exe: PathBuf::from("/x"),
                auth_key: None,
            },
            &mut Console::new(&mut said),
        )
        .unwrap_err();
        assert!(
            matches!(error, HostError::SocketPathTooLong { limit: 103, .. }),
            "{error:?}"
        );
        assert!(!host.home.exists(), "nothing created");
    }

    #[test]
    fn a_refused_sudo_names_the_exact_command_and_installs_no_server() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("sudo-refuses"), "").unwrap();
        let (result, _said) = machine.setup(None);
        let error = result.unwrap_err();
        let shown = error.to_string();
        assert!(
            shown.starts_with(&format!(
                "`{}/bin/sudo -- {} --socket=",
                machine.root().display(),
                machine.programs().tailscale.display()
            )) && shown.contains("yourself, then abc server setup again"),
            "{shown}"
        );
        assert!(
            !machine
                .root()
                .join("services/dev.getabc.server.plist")
                .exists(),
            "no server without Serve"
        );
        // Once the person can, setup finishes.
        std::fs::remove_file(machine.state("sudo-refuses")).unwrap();
        let (again, said) = machine.setup(None);
        assert!(again.is_ok(), "{said}");
    }

    #[test]
    fn an_auth_key_file_logs_in_without_a_browser() {
        let machine = Machine::new(Os::Linux);
        let key = machine.root().join("key");
        std::fs::write(&key, "tskey-auth-x").unwrap();
        let (result, said) = machine.setup(Some(key.clone()));
        result.unwrap_or_else(|error| panic!("{error}\n{said}"));
        let calls = machine.take_calls();
        assert!(
            calls.iter().any(|call| call.ends_with(&format!(
                "up --hostname=abc --auth-key=file:{}",
                key.display()
            ))),
            "{calls:#?}"
        );
        assert!(
            !calls.iter().any(|call| call.starts_with("xdg-open")),
            "no browser: {calls:#?}"
        );
    }

    #[test]
    fn a_login_nobody_finishes_times_out_with_the_fix() {
        let machine = Machine::new(Os::MacOs);
        // A browser that opens nothing.
        std::fs::write(machine.root().join("bin/open"), "#!/bin/sh\n").unwrap();
        let mut host = machine.host();
        host.timing.login = Duration::from_millis(300);
        let mut said = Vec::new();
        let error = abc_host::setup(
            &host,
            &SetupOptions {
                exe: PathBuf::from("/x"),
                auth_key: None,
            },
            &mut Console::new(&mut said),
        )
        .unwrap_err();
        assert!(
            matches!(&error, HostError::Timeout { fix, .. } if fix == "run abc server setup again for a new login link"),
            "{error:?}"
        );
        let said = String::from_utf8(said).unwrap();
        assert!(
            said.contains("https://login.tailscale.com/a/fake\n  (opened in your browser"),
            "the link is printed too: {said}"
        );
        let up = std::fs::read_to_string(machine.state("up.pid")).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", up.trim()])
            .status()
            .unwrap();
        assert!(
            !alive.success(),
            "tailscale up is stopped, not left waiting"
        );
    }

    #[test]
    fn status_names_the_fix_for_each_broken_part() {
        let machine = Machine::new(Os::Linux);
        let (result, said) = machine.setup(None);
        result.unwrap_or_else(|error| panic!("{error}\n{said}"));
        // abc server dies, Serve is lost and lingering is turned off.
        let abc_pid = std::fs::read_to_string(machine.state("abc-server.service.pid")).unwrap();
        std::process::Command::new("kill")
            .args(["-9", abc_pid.trim()])
            .status()
            .unwrap();
        std::fs::remove_file(machine.state("serve.json")).unwrap();
        std::fs::write(machine.state("linger"), "no\n").unwrap();
        let report = abc_host::status(&machine.host()).unwrap();
        assert!(!report.healthy(), "broken");
        let failing: Vec<(&str, &str)> = report
            .checks
            .iter()
            .filter(|check| !check.healthy)
            .map(|check| (check.part.as_str(), check.fix.as_deref().unwrap()))
            .collect();
        assert_eq!(
            failing,
            [
                (
                    "server service",
                    "abc server setup (and read why it stopped: journalctl --user -u \
                     abc-server.service -n 50)"
                ),
                (
                    "serve",
                    "abc server setup, which asks for your password once to set it"
                ),
                (
                    "lingering",
                    "abc server setup, or sudo loginctl enable-linger 501"
                ),
                (
                    "abc server",
                    "read journalctl --user -u abc-server.service -n 50, then abc server setup"
                ),
            ],
            "{:#?}",
            report.lines()
        );
        // Setup repairs all three, with one sudo for the two root needs.
        machine.take_calls();
        let (repaired, said_repairing) = machine.setup(None);
        assert!(repaired.is_ok(), "{said_repairing}");
        let sudo = machine
            .take_calls()
            .into_iter()
            .filter(|call| call.starts_with("sudo "))
            .count();
        assert_eq!(sudo, 1, "one sudo");
        assert!(
            abc_host::status(&machine.host()).unwrap().healthy(),
            "repaired"
        );
    }

    #[test]
    fn status_before_setup_says_to_run_setup() {
        let machine = Machine::new(Os::MacOs);
        std::fs::remove_file(machine.home().config()).unwrap();
        let error = abc_host::status(&machine.host()).unwrap_err();
        assert!(
            error.to_string().ends_with("run abc server setup"),
            "{error}"
        );
    }

    #[test]
    fn status_of_a_stack_that_is_down_says_what_to_read() {
        let machine = Machine::new(Os::MacOs);
        let report = abc_host::status(&machine.host()).unwrap();
        let lines = report.lines();
        let fixes: Vec<&str> = report
            .checks
            .iter()
            .filter_map(|check| check.fix.as_deref())
            .collect();
        assert_eq!(fixes.len(), 7, "all but the programs: {lines:#?}");
        assert!(
            fixes.iter().all(|fix| fix.contains("abc server setup")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("abc's tailscaled does not answer")),
            "{lines:#?}"
        );
    }

    #[test]
    fn backup_writes_a_private_dump_and_restore_puts_it_back_with_the_server_stopped() {
        let machine = Machine::new(Os::MacOs);
        let (result, said) = machine.setup(None);
        result.unwrap_or_else(|error| panic!("{error}\n{said}"));
        let host = machine.host();
        std::fs::write(machine.state("db-abc"), "the original\n").unwrap();
        let at = time::OffsetDateTime::from_unix_timestamp(1_793_000_000).unwrap();
        let backup = abc_host::backup(&host, None, at).unwrap();
        assert_eq!(
            backup.path,
            machine.home().backups().join("abc-20261026-073320.dump"),
            "named by its time"
        );
        assert_eq!(mode(&backup.path), 0o600, "private");
        assert_eq!(
            std::fs::read_to_string(&backup.path).unwrap(),
            "FAKE DUMP\nthe original\n",
            "the database"
        );
        assert!(backup.bytes > 0, "its size");
        assert!(
            matches!(
                abc_host::backup(&host, Some(&backup.path), at),
                Err(HostError::BackupExists { .. })
            ),
            "never overwritten"
        );

        // A file that is no backup is refused before abc server stops.
        let bogus = machine.root().join("bogus");
        std::fs::write(&bogus, "not a dump").unwrap();
        machine.take_calls();
        let mut restoring = Vec::new();
        let refused =
            abc_host::restore(&host, &bogus, &mut Console::new(&mut restoring)).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("name a file abc server backup wrote"),
            "{refused}"
        );
        assert!(
            !machine
                .take_calls()
                .iter()
                .any(|call| call.contains("bootout")),
            "abc server kept running"
        );

        std::fs::write(machine.state("db-abc"), "changed since\n").unwrap();
        abc_host::restore(&host, &backup.path, &mut Console::new(&mut restoring)).unwrap();
        assert_eq!(
            std::fs::read_to_string(machine.state("db-abc")).unwrap(),
            "the original\n",
            "the backup is back"
        );
        let calls = machine.take_calls();
        let position = |text: &str| {
            calls
                .iter()
                .position(|call| call.contains(text))
                .unwrap_or_else(|| panic!("{text}: {calls:#?}"))
        };
        assert!(
            position("bootout gui/501/dev.getabc.server")
                < position("ALTER DATABASE abc_restoring RENAME TO abc")
                && position("ALTER DATABASE") < position("bootstrap gui/501"),
            "abc server stops for the swap and starts after: {calls:#?}"
        );
        assert!(machine.alive("abc.daemon"), "running again");
    }

    #[test]
    fn upgrade_restarts_each_service_and_uninstall_keeps_the_data_unless_purged() {
        let machine = Machine::new(Os::MacOs);
        let (result, said) = machine.setup(None);
        result.unwrap_or_else(|error| panic!("{error}\n{said}"));
        let host = machine.host();
        machine.take_calls();
        let mut upgrading = Vec::new();
        abc_host::upgrade(&host, &mut Console::new(&mut upgrading)).unwrap();
        let kickstarts: Vec<String> = machine
            .take_calls()
            .into_iter()
            .filter(|call| call.starts_with("launchctl kickstart -k"))
            .collect();
        assert_eq!(
            kickstarts,
            [
                "launchctl kickstart -k gui/501/dev.getabc.postgres",
                "launchctl kickstart -k gui/501/dev.getabc.tailscaled",
                "launchctl kickstart -k gui/501/dev.getabc.server",
            ],
            "each restarted, in order"
        );

        let mut uninstalling = Vec::new();
        abc_host::uninstall(&host, false, &mut Console::new(&mut uninstalling)).unwrap();
        let told = String::from_utf8(uninstalling).unwrap();
        let calls = machine.take_calls();
        assert!(
            calls.contains(&format!(
                "tailscale --socket={} logout",
                machine.socket("tailscaled.sock")
            )),
            "logged out: {calls:#?}"
        );
        assert!(
            told.contains("https://login.tailscale.com/admin/machines"),
            "says how to delete the device: {told}"
        );
        assert!(
            std::fs::read_dir(machine.root().join("services"))
                .unwrap()
                .next()
                .is_none(),
            "no services left"
        );
        for daemon in ["postgres.daemon", "tailscaled.daemon", "abc.daemon"] {
            assert!(!machine.alive(daemon), "{daemon} stopped");
        }
        assert!(machine.home().postgres().exists(), "the data stays");
        assert!(told.contains("--purge deletes it"), "{told}");
        let mut purging = Vec::new();
        abc_host::uninstall(&host, true, &mut Console::new(&mut purging)).unwrap();
        assert!(!machine.home().exists(), "purged");
    }

    #[test]
    fn day_two_commands_before_setup_say_to_run_setup() {
        let machine = Machine::new(Os::Linux);
        std::fs::remove_file(machine.home().config()).unwrap();
        let host = machine.host();
        let mut said = Vec::new();
        let now = time::OffsetDateTime::UNIX_EPOCH;
        for error in [
            abc_host::database_url(&host).unwrap_err(),
            abc_host::upgrade(&host, &mut Console::new(&mut said)).unwrap_err(),
            abc_host::backup(&host, None, now).unwrap_err(),
            abc_host::restore(&host, Path::new("/x"), &mut Console::new(&mut said)).unwrap_err(),
        ] {
            assert!(matches!(error, HostError::NotSetUp { .. }), "{error:?}");
        }
        // Uninstalling nothing succeeds.
        abc_host::uninstall(&host, false, &mut Console::new(&mut said)).unwrap();
    }

    /// `setup` on `machine` with `timing` changed by `adjust`, and what it said.
    fn setup_with(machine: &Machine, adjust: impl FnOnce(&mut Timing)) -> (HostError, String) {
        let mut host = machine.host();
        adjust(&mut host.timing);
        let mut said = Vec::new();
        let error = abc_host::setup(
            &host,
            &SetupOptions {
                exe: PathBuf::from("/x"),
                auth_key: None,
            },
            &mut Console::new(&mut said),
        )
        .unwrap_err();
        (error, String::from_utf8(said).unwrap())
    }

    fn replace_program(machine: &Machine, name: &str, script: &str) {
        let path = machine.root().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_cluster_of_another_postgres_is_refused_with_the_way_back() {
        let machine = Machine::new(Os::Linux);
        std::fs::create_dir_all(machine.home().postgres()).unwrap();
        std::fs::write(machine.home().postgres().join("PG_VERSION"), "17\n").unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::ClusterVersion { found, .. } if found == "17"),
            "{error:?}"
        );
        assert!(error.to_string().contains("abc server restore"), "{error}");
        assert!(
            !machine
                .take_calls()
                .iter()
                .any(|call| call.starts_with("initdb")),
            "never initialised over it"
        );
    }

    #[test]
    fn a_postgres_that_never_answers_times_out_naming_its_log() {
        let machine = Machine::new(Os::MacOs);
        replace_program(&machine, "programs/pg/pg_isready", "#!/bin/sh\nexit 2\n");
        let (error, _said) = setup_with(&machine, |timing| {
            timing.postgres = Duration::from_millis(200);
        });
        assert!(
            matches!(&error, HostError::Timeout { what, fix, .. }
                if what == "abc's Postgres accepting connections" && fix.contains("logs/postgres.log")),
            "{error:?}"
        );
    }

    #[test]
    fn a_tailscaled_that_never_answers_times_out_with_the_fix() {
        let machine = Machine::new(Os::Linux);
        replace_program(&machine, "programs/tailscaled", "#!/bin/sh\nexit 1\n");
        let (error, _said) = setup_with(&machine, |timing| {
            timing.tailscaled = Duration::from_millis(200);
        });
        assert!(
            matches!(&error, HostError::Timeout { what, fix, .. }
                if what.starts_with("abc's tailscaled answering (")
                    && fix == "check abc's tailscaled with abc server status, and its log"),
            "{error:?}"
        );
    }

    #[test]
    fn an_answer_abc_cannot_read_names_the_programs_to_check() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("garbled"), "").unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::TailscaleAnswer { command, .. } if command.ends_with("status --json")),
            "{error:?}"
        );
        assert!(error.to_string().contains("[programs]"), "{error}");
    }

    #[test]
    fn a_refused_login_shows_what_tailscale_said() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("up-fails"), "").unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::Failed { output, fix, .. }
                if output.contains("invalid key")
                    && fix == "read Tailscale's message above, then run abc server setup again"),
            "{error:?}"
        );
    }

    #[test]
    fn a_tailnet_that_approves_devices_is_told_to_approve_abc() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("needs-approval"), "").unwrap();
        let (result, said) = machine.setup(None);
        assert!(result.is_ok(), "{said}");
        assert_eq!(
            said.matches("approve `abc` at https://login.tailscale.com/admin/machines")
                .count(),
            1,
            "said once: {said}"
        );
    }

    #[test]
    fn a_root_step_that_did_not_take_names_the_command_to_run() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("sudo-does-nothing"), "").unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::ServeNotSet { command, .. } if command.contains(" serve --bg unix:")),
            "{error:?}"
        );
        // On Linux with Serve already set, lingering alone, which also did not take.
        let linux = Machine::new(Os::Linux);
        std::fs::write(
            linux.state("serve.json"),
            format!(
                "{{\"Web\":{{\"abc.tailfake.ts.net:443\":{{\"Handlers\":{{\"/\":{{\"Proxy\":\"unix:{}\"}}}}}}}}}}",
                linux.socket("abc.sock")
            ),
        )
        .unwrap();
        std::fs::write(linux.state("sudo-does-nothing"), "").unwrap();
        let (lingering, told) = setup_with(&linux, |_| {});
        assert!(
            lingering.to_string().contains("lingering is still off")
                && lingering
                    .to_string()
                    .contains("sudo loginctl enable-linger 501"),
            "{lingering}"
        );
        assert!(
            !told.contains("Serve on abc's own node"),
            "only lingering: {told}"
        );
    }

    #[test]
    fn upgrade_after_a_program_moved_reloads_the_changed_definitions() {
        for os in [Os::MacOs, Os::Linux] {
            let machine = Machine::new(os);
            let (result, said) = machine.setup(None);
            assert!(result.is_ok(), "{said}");
            let moved = machine.root().join("programs/abc-moved");
            std::fs::copy(machine.programs().abc, &moved).unwrap();
            let mut config = Config::require(&machine.home()).unwrap();
            config.programs.abc = moved.clone();
            config.save(&machine.home()).unwrap();
            machine.take_calls();
            let mut upgrading = Vec::new();
            abc_host::upgrade(&machine.host(), &mut Console::new(&mut upgrading)).unwrap();
            let calls = machine.take_calls();
            let expected: &[&str] = match os {
                Os::MacOs => &[
                    "launchctl bootout gui/501/dev.getabc.server",
                    "launchctl bootstrap gui/501",
                ],
                Os::Linux => &[
                    "systemctl --user daemon-reload",
                    "systemctl --user restart abc-server.service",
                ],
            };
            for call in expected {
                assert!(
                    calls.iter().any(|made| made.starts_with(call)),
                    "{os:?}: {call}: {calls:#?}"
                );
            }
            assert!(
                calls.iter().any(|call| call.starts_with("abc server")),
                "the moved abc runs: {calls:#?}"
            );
        }
    }

    #[test]
    fn status_says_when_the_node_moved_serve_points_elsewhere_or_the_node_logged_out() {
        let machine = Machine::new(Os::MacOs);
        let (result, said) = machine.setup(None);
        assert!(result.is_ok(), "{said}");
        let mut config = Config::require(&machine.home()).unwrap();
        config.public_url =
            Some(abc_protocol::url::PublicUrl::parse("https://abc-old.tailfake.ts.net").unwrap());
        config.save(&machine.home()).unwrap();
        std::fs::write(
            machine.state("serve.json"),
            "{\"Web\":{\"abc.tailfake.ts.net:443\":{\"Handlers\":{\"/\":{\"Proxy\":\"unix:/elsewhere\"}}}}}",
        )
        .unwrap();
        let lines = abc_host::status(&machine.host()).unwrap().lines();
        assert!(
            lines.iter().any(|line| line.contains(
                "logged in as https://abc.tailfake.ts.net, which is not the configured URL"
            )),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains(&format!(
                "https://abc.tailfake.ts.net/ goes to unix:/elsewhere, not unix:{}",
                machine.socket("abc.sock")
            ))),
            "{lines:#?}"
        );
        std::fs::remove_file(machine.state("logged-in")).unwrap();
        let logged_out = abc_host::status(&machine.host()).unwrap().lines();
        assert!(
            logged_out
                .iter()
                .any(|line| line.contains("not logged in (NeedsLogin)")),
            "{logged_out:#?}"
        );
        assert!(
            logged_out
                .iter()
                .any(|line| line.contains("unknown until abc's node is logged in")),
            "{logged_out:#?}"
        );
        std::fs::write(machine.state("garbled"), "").unwrap();
        let report = abc_host::status(&machine.host()).unwrap();
        let fixes: Vec<&str> = report
            .checks
            .iter()
            .filter_map(|check| check.fix.as_deref())
            .collect();
        assert!(
            fixes
                .iter()
                .any(|fix| fix.contains("tail -n 50") && fix.contains("tailscaled.log")),
            "{fixes:#?}"
        );
    }

    #[test]
    fn a_dump_that_does_not_load_leaves_the_database_and_restarts_the_server() {
        for os in [Os::MacOs, Os::Linux] {
            let machine = Machine::new(os);
            let (result, said) = machine.setup(None);
            assert!(result.is_ok(), "{said}");
            std::fs::write(machine.state("db-abc"), "the data\n").unwrap();
            let broken = machine.root().join("broken.dump");
            std::fs::write(&broken, "FAKE DUMP\nBROKEN\n").unwrap();
            let mut restoring = Vec::new();
            let error =
                abc_host::restore(&machine.host(), &broken, &mut Console::new(&mut restoring))
                    .unwrap_err();
            assert!(
                error.to_string().contains("abc's database is unchanged"),
                "{error}"
            );
            assert_eq!(
                std::fs::read_to_string(machine.state("db-abc")).unwrap(),
                "the data\n",
                "{os:?}: untouched"
            );
            assert!(
                !machine.state("db-abc_restoring").exists(),
                "the half-loaded copy is dropped"
            );
            let back = (0..200).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                machine.alive("abc.daemon")
            });
            assert!(back, "{os:?}: abc server runs again");
        }
    }

    #[test]
    fn backup_and_restore_need_a_running_postgres_and_a_failed_dump_leaves_no_file() {
        let machine = Machine::new(Os::Linux);
        let (result, said) = machine.setup(None);
        assert!(result.is_ok(), "{said}");
        let host = machine.host();
        replace_program(
            &machine,
            "programs/pg/pg_dump",
            "#!/bin/sh\necho 'pg_dump: error: connection failed' >&2\nexit 1\n",
        );
        let file = machine.root().join("x.dump");
        let error =
            abc_host::backup(&host, Some(&file), time::OffsetDateTime::UNIX_EPOCH).unwrap_err();
        assert!(error.to_string().contains("connection failed"), "{error}");
        assert!(!file.exists(), "no half a backup");
        let good = machine.root().join("good.dump");
        std::fs::write(&good, "FAKE DUMP\nx\n").unwrap();
        let pid = std::fs::read_to_string(machine.state("abc-postgres.service.pid")).unwrap();
        std::process::Command::new("kill")
            .args(["-9", pid.trim()])
            .status()
            .unwrap();
        let mut restoring = Vec::new();
        let down = abc_host::restore(&host, &good, &mut Console::new(&mut restoring)).unwrap_err();
        assert!(
            down.to_string()
                .contains("check abc's Postgres with abc server status"),
            "{down}"
        );
        assert!(machine.alive("abc.daemon"), "abc server was never stopped");
    }

    #[test]
    fn uninstall_without_a_reachable_node_says_to_remove_it_in_the_console() {
        let machine = Machine::new(Os::Linux);
        let (result, said) = machine.setup(None);
        assert!(result.is_ok(), "{said}");
        std::fs::remove_file(machine.programs().tailscale).unwrap();
        let mut uninstalling = Vec::new();
        abc_host::uninstall(&machine.host(), true, &mut Console::new(&mut uninstalling)).unwrap();
        let told = String::from_utf8(uninstalling).unwrap();
        assert!(
            told.contains("Could not log abc's node out (") && told.contains("admin console"),
            "{told}"
        );
        assert!(told.contains("Deleted "), "{told}");
        assert!(!machine.home().exists(), "purged");
        for daemon in ["postgres.daemon", "tailscaled.daemon", "abc.daemon"] {
            assert!(!machine.alive(daemon), "{daemon} stopped");
        }
        let calls = machine.take_calls();
        let disabled = calls
            .iter()
            .rposition(|call| call.starts_with("systemctl --user disable --now"))
            .unwrap();
        assert!(
            calls
                .iter()
                .skip(disabled)
                .any(|call| call == "systemctl --user daemon-reload"),
            "systemd forgets the removed units: {calls:#?}"
        );
    }

    #[test]
    fn a_node_without_a_name_is_told_to_turn_on_magicdns() {
        let machine = Machine::new(Os::MacOs);
        std::fs::write(machine.state("no-dns"), "").unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::TailscaleAnswer { reason, .. } if reason.contains("turn on MagicDNS")),
            "{error:?}"
        );
    }

    #[test]
    fn launchd_still_unloading_an_agent_is_waited_for() {
        let machine = Machine::new(Os::MacOs);
        let (result, said) = machine.setup(None);
        assert!(result.is_ok(), "{said}");
        std::fs::write(machine.state("bootout-lingers"), "").unwrap();
        let moved = machine.root().join("programs/abc-moved");
        std::fs::copy(machine.programs().abc, &moved).unwrap();
        let mut config = Config::require(&machine.home()).unwrap();
        config.programs.abc = moved;
        config.save(&machine.home()).unwrap();
        machine.take_calls();
        let mut upgrading = Vec::new();
        abc_host::upgrade(&machine.host(), &mut Console::new(&mut upgrading)).unwrap();
        let bootstraps = machine
            .take_calls()
            .into_iter()
            .filter(|call| call.starts_with("launchctl bootstrap"))
            .count();
        assert_eq!(
            bootstraps, 2,
            "one refused while unloading, then one loaded"
        );
        assert!(machine.alive("abc.daemon"), "running");
    }

    #[test]
    fn without_a_browser_the_link_is_printed_to_open_by_hand() {
        let machine = Machine::new(Os::Linux);
        let mut host = machine.host();
        host.path = std::ffi::OsString::new();
        let mut said = Vec::new();
        abc_host::open_in_browser(
            &host,
            &mut Console::new(&mut said),
            "https://x.example/#code=1",
        );
        assert_eq!(
            String::from_utf8(said).unwrap(),
            "  https://x.example/#code=1\n  (open it in a browser)\n",
            "printed"
        );
        replace_program(&machine, "bin/xdg-open", "#!/bin/sh\nexit 3\n");
        let mut failed = Vec::new();
        abc_host::open_in_browser(
            &machine.host(),
            &mut Console::new(&mut failed),
            "https://x.example/",
        );
        assert!(
            String::from_utf8(failed)
                .unwrap()
                .ends_with("(open it in a browser)\n"),
            "a browser that fails to open is no browser"
        );
    }

    #[test]
    fn a_cluster_version_that_cannot_be_read_is_an_error_not_a_new_cluster() {
        let machine = Machine::new(Os::MacOs);
        std::fs::create_dir_all(machine.home().postgres().join("PG_VERSION")).unwrap();
        let (error, _said) = setup_with(&machine, |_| {});
        assert!(
            matches!(&error, HostError::Io { path, .. } if path.ends_with("postgres/PG_VERSION")),
            "{error:?}"
        );
        assert!(
            !machine
                .take_calls()
                .iter()
                .any(|call| call.starts_with("initdb")),
            "never initialised over it"
        );
    }

    #[test]
    fn a_backup_where_no_file_can_be_made_is_an_io_error() {
        let machine = Machine::new(Os::Linux);
        let nowhere = machine.root().join("missing/dir/x.dump");
        let error = abc_host::backup(
            &machine.host(),
            Some(&nowhere),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap_err();
        assert!(
            matches!(&error, HostError::Io { path, .. } if *path == nowhere),
            "not mistaken for an existing backup: {error:?}"
        );
    }

    #[test]
    fn purging_nothing_succeeds_and_a_purge_that_fails_says_so() {
        let machine = Machine::new(Os::Linux);
        let mut host = machine.host();
        let mut said = Vec::new();
        let locked = machine.home().dir().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("f"), "").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let error = abc_host::uninstall(&host, true, &mut Console::new(&mut said)).unwrap_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(&error, HostError::Io { path, .. } if path == machine.home().dir()),
            "{error:?}"
        );
        host.home = ServerHome::at(machine.root().join("never-made"));
        abc_host::uninstall(&host, true, &mut Console::new(&mut said)).unwrap();
        assert!(!host.home.exists(), "still nothing");
    }
}
