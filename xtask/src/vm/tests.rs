use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus, Output};

use super::{
    Action, Backend, Host, Os, Spec, TOKEN_ITEM, billing, microsandbox, refuse_linked_worktree,
    require_kvm, run,
};
use crate::testing::{git, repository, script};

/// A backend that records what it is asked and answers from a script.
#[derive(Default)]
struct Fake {
    running: bool,
    /// What `claude auth status --json` prints in the VM.
    auth: String,
    /// Whether the VM ever gets ready.
    ready: bool,
    started: RefCell<Vec<Spec>>,
    calls: RefCell<Vec<String>>,
    /// What each exec received on stdin.
    stdin: RefCell<Vec<Vec<u8>>>,
    /// Whether the VM's git refuses the commits `vm push` sends.
    refuses_push: bool,
    /// What the backend's volumes are and take.
    volumes: Vec<(String, u64)>,
}

fn output(code: i32, stdout: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: b"said no".to_vec(),
    }
}

fn shown(words: &[OsString]) -> String {
    words
        .iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Backend for Fake {
    fn is_running(&self, name: &str) -> anyhow::Result<bool> {
        self.calls.borrow_mut().push(format!("is_running {name}"));
        Ok(self.running)
    }

    fn start(&self, spec: &Spec) -> anyhow::Result<()> {
        self.started.borrow_mut().push(Spec {
            name: spec.name.clone(),
            image: spec.image.clone(),
            cpus: spec.cpus,
            memory: spec.memory.clone(),
            egress: spec.egress.clone(),
            mounts: spec.mounts.clone(),
            volumes: Vec::new(),
            tmpfs: spec.tmpfs.clone(),
            vsock: spec.vsock.clone(),
            env: spec.env.clone(),
        });
        Ok(())
    }

    fn exec(&self, name: &str, command: &[OsString], stdin: &[u8]) -> anyhow::Result<Output> {
        let command = shown(command);
        self.calls
            .borrow_mut()
            .push(format!("exec {name} {command}"));
        self.stdin.borrow_mut().push(stdin.to_vec());
        Ok(match command.as_str() {
            "test -e /run/abc/ready" => output(i32::from(!self.ready), ""),
            "agent-exec claude auth status --json" => output(0, &self.auth),
            pushed if pushed.starts_with("agent-exec sh -c ") => {
                output(i32::from(self.refuses_push), "")
            }
            _ => output(0, ""),
        })
    }

    fn attach(&self, name: &str, command: &[OsString], tty: bool) -> anyhow::Result<ExitStatus> {
        let tty = if tty { " --tty" } else { "" };
        self.calls
            .borrow_mut()
            .push(format!("attach{tty} {name} {}", shown(command)));
        Ok(ExitStatus::from_raw(5 << 8))
    }

    fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("remove {name}"));
        Ok(())
    }

    fn stream(
        &self,
        name: &str,
        command: &[OsString],
        log: &mut dyn std::io::Write,
    ) -> anyhow::Result<ExitStatus> {
        self.calls
            .borrow_mut()
            .push(format!("stream {name} {}", shown(command)));
        log.write_all(b"one\ntwo\n")?;
        Ok(ExitStatus::from_raw(6 << 8))
    }

    fn remove_volumes(&self, names: &[String]) -> anyhow::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("remove_volumes {}", names.join(" ")));
        Ok(())
    }

    fn volumes(&self) -> anyhow::Result<Vec<(String, u64)>> {
        Ok(self.volumes.clone())
    }

    fn list(&self) -> anyhow::Result<String> {
        Ok("NAME\nabc-w\n".to_owned())
    }

    fn load_image(&self, archive: &mut dyn Read, tag: &str) -> anyhow::Result<()> {
        let mut bytes = String::new();
        archive.read_to_string(&mut bytes)?;
        self.calls
            .borrow_mut()
            .push(format!("load_image {tag} {bytes}"));
        Ok(())
    }
}

const SUBSCRIBED: &str =
    r#"{"loggedIn": true, "authMethod": "oauth_token", "apiProvider": "firstParty"}"#;

/// A main clone with a linked worktree `w` on branch `feature`, a host for it whose keychain
/// holds `token` (none when empty), and a broker socket that listens.
struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    host: Host,
    _home: tempfile::TempDir,
    _broker: std::thread::JoinHandle<()>,
}

/// A broker for session `abc-w` under `home` that answers every stop as the real one does.
fn fake_broker(home: &Path) -> std::thread::JoinHandle<()> {
    let files = crate::broker::files(home, "abc-w");
    std::fs::create_dir_all(files.control.parent().unwrap()).unwrap();
    let control = UnixListener::bind(&files.control).unwrap();
    std::thread::spawn(move || {
        for client in control.incoming() {
            let Ok(mut client) = client else { return };
            let mut line = String::new();
            let read =
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&client), &mut line);
            if read.is_ok() && line == "stop\n" {
                let _told =
                    std::io::Write::write_all(&mut client, b"stopped revoked 0 of 0 tokens\n");
            }
        }
    })
}

fn world(token: &str) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let main = repository(&root.join("main"));
    git(
        &main,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/getabc-dev/abc.git",
        ],
    );
    git(&main, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let worktree = root.join("w");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(main.join("xtask/vm")).unwrap();
    std::fs::write(
        main.join(super::egress::FILE),
        "github.com\n*.githubusercontent.com\n",
    )
    .unwrap();
    let security = root.join("security");
    let body = if token.is_empty() {
        "exit 44\n".to_owned()
    } else {
        format!(
            "[ \"$*\" = 'find-generic-password -s {TOKEN_ITEM} -w' ] || exit 9\necho '{token}'\n"
        )
    };
    script(&security, &body);
    // Socket paths are short on macOS: the home lives under /tmp.
    let home = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let host = Host {
        os: Os::MacOs,
        repo: main,
        state: root.join("state"),
        home: home.path().to_path_buf(),
        broker_exe: root.join("no-broker"),
        secret: (
            security,
            ["find-generic-password", "-s", TOKEN_ITEM, "-w"]
                .map(str::to_owned)
                .into(),
        ),
        builder: root.join("container"),
        patience: 3,
        broker_patience: std::time::Duration::from_millis(200),
        github_cli: root.join("gh"),
        // Scratch worktrees live in the system's temporary directory, which the production policy
        // reserves on Linux; the policy itself is tested on its own.
        reserved: Vec::new(),
        terminal: true,
    };
    let broker = fake_broker(&host.home);
    World {
        _dir: dir,
        _home: home,
        root,
        worktree,
        host,
        _broker: broker,
    }
}

fn none(_: &str) -> Option<OsString> {
    None
}

fn act(world: &World, backend: &Fake, action: Action) -> (anyhow::Result<ExitCode>, String) {
    let mut out = Vec::new();
    let result = run(action, &world.host, backend, &none, &mut out);
    (result, String::from_utf8(out).unwrap())
}

/// Starts the session for `world`'s worktree, which must fail, and returns why.
fn start_error(world: &World, backend: &Fake) -> String {
    let start = Action::Start {
        worktree: world.worktree.clone(),
    };
    format!("{:#}", act(world, backend, start).0.unwrap_err())
}

/// Starts the session for `world`'s worktree, which must succeed, and returns the report.
fn start_ok(world: &World, backend: &Fake) -> String {
    let start = Action::Start {
        worktree: world.worktree.clone(),
    };
    let (result, out) = act(world, backend, start);
    result.unwrap();
    out
}

/// Stops the session for `world`'s worktree, purging it when `purge`.
fn stop_session(world: &World, backend: &Fake, purge: bool) {
    let stop = Action::Stop {
        worktree: world.worktree.clone(),
        purge,
    };
    act(world, backend, stop).0.unwrap();
}

#[test]
fn a_purge_reports_state_it_cannot_delete() {
    let mut world = world("t");
    let file = world.root.join("not-a-directory");
    std::fs::write(&file, "").unwrap();
    world.host.state = file;
    let stop = Action::Stop {
        worktree: world.worktree.clone(),
        purge: true,
    };
    let error = format!("{:#}", act(&world, &subscribed(), stop).0.unwrap_err());
    assert!(error.starts_with("could not delete "), "{error}");
}

/// A VM that gets ready and reports the subscription token.
fn subscribed() -> Fake {
    Fake {
        auth: SUBSCRIBED.to_owned(),
        ready: true,
        ..Fake::default()
    }
}

#[test]
fn starts_a_session_with_its_token_and_checks_the_subscription() {
    let world = world("sk-ant-oat01-secret");
    let backend = subscribed();
    let (result, out) = act(
        &world,
        &backend,
        Action::Start {
            worktree: world.worktree.clone(),
        },
    );
    result.unwrap();
    assert!(
        out.contains("abc-w is up; claude auth status: loggedIn true, authMethod oauth_token"),
        "{out}"
    );
    assert!(!out.contains("secret"), "the token is never shown: {out}");
    let started = backend.started.borrow();
    let spec = &started[0];
    assert_eq!(spec.name, "abc-w");
    assert_eq!(spec.image, "abc-agent:latest");
    assert_eq!(spec.egress, ["github.com", "*.githubusercontent.com"]);
    assert_eq!(
        spec.mounts,
        [
            (world.worktree.clone(), world.worktree.clone()),
            (world.root.join("state/w/git"), PathBuf::from("/abc/git")),
        ]
    );
    assert_eq!(
        spec.vsock,
        (crate::broker::files(&world.host.home, "abc-w").tokens, 5000),
        "the session's own broker"
    );
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for expected in [
        "ABC_VM=1".to_owned(),
        format!("ABC_WORKTREE={}", world.worktree.display()),
        "ABC_AGENT_BROKER=/run/abc/agent/broker.sock".to_owned(),
        format!("ABC_GIT_COMMON={}", world.root.join("main/.git").display()),
        "ABC_BROKER_VSOCK_PORT=5000".to_owned(),
        "ABC_TEST_DATABASE_URL=postgres://abc_test@127.0.0.1:5432/postgres".to_owned(),
    ] {
        assert!(env.contains(&expected), "{expected}: {env:?}");
    }
    assert!(
        !env.iter()
            .any(|entry| entry.contains("secret") || entry.starts_with("CLAUDE_CODE_OAUTH")),
        "the token is not in the VM's configuration"
    );
    let calls = backend.calls.borrow();
    let handoff = calls
        .iter()
        .filter(|call| call.starts_with("exec "))
        .position(|call| call.contains("cat > /run/abc/claude-token"))
        .unwrap();
    assert_eq!(
        backend.stdin.borrow()[handoff],
        b"sk-ant-oat01-secret",
        "piped, trimmed"
    );
    assert!(
        !calls.iter().any(|call| call.contains("secret")),
        "never an argument"
    );
    assert!(world.worktree.join("target").is_dir() && world.worktree.join(".tools").is_dir());
    assert!(
        world.root.join("state/w/git/worktrees/w/HEAD").is_file(),
        "the session repository"
    );
}

#[test]
fn a_running_session_is_reused() {
    let world = world("t");
    let backend = Fake {
        running: true,
        auth: SUBSCRIBED.to_owned(),
        ready: true,
        ..Fake::default()
    };
    let out = start_ok(&world, &backend);
    assert!(backend.started.borrow().is_empty());
    assert!(out.contains("refreshed only while it is stopped"), "{out}");
    assert!(
        !world.root.join("state/w/git").exists(),
        "the host leaves a running VM's repository alone"
    );
}

#[test]
fn pushes_host_commits_into_a_running_vm_only() {
    let world = world("t");
    let push = || Action::Push {
        worktree: world.worktree.clone(),
    };
    let stopped = subscribed();
    start_ok(&world, &stopped);
    let error = format!("{:#}", act(&world, &stopped, push()).0.unwrap_err());
    assert!(error.contains("abc-w is not running"), "{error}");
    let running = Fake {
        running: true,
        ..subscribed()
    };
    let (current, already) = act(&world, &running, push());
    current.unwrap();
    assert!(already.contains("already at"), "{already}");
    git(
        &world.worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "on the host"],
    );
    let (result, out) = act(&world, &running, push());
    result.unwrap();
    assert!(
        out.starts_with("vm: the VM's feature: ") && out.contains("index now matches"),
        "{out}"
    );
    let calls = running.calls.borrow();
    let last = calls.last().unwrap();
    assert!(
        last.starts_with("exec abc-w agent-exec sh -c set -eu") && last.ends_with(" sh feature"),
        "{last}"
    );
    assert!(
        running
            .stdin
            .borrow()
            .last()
            .unwrap()
            .starts_with(b"# v2 git bundle\n"),
        "the commits go in as a bundle on stdin"
    );
    let refusing = Fake {
        running: true,
        refuses_push: true,
        ..subscribed()
    };
    let refused = format!("{:#}", act(&world, &refusing, push()).0.unwrap_err());
    assert_eq!(refused, "the VM did not take the commits: said no");
}

/// The reserved guest directories this host refuses to mount over.
fn production() -> Vec<PathBuf> {
    Host::this(Path::new("/repo"), Path::new("/home/m"), &none)
        .unwrap()
        .reserved
}

#[test]
fn refuses_to_start_a_worktree_in_a_reserved_directory() {
    let mut world = world("t");
    world.host.reserved = vec![world.root.clone()];
    let backend = subscribed();
    let error = start_error(&world, &backend);
    assert!(
        error.contains("lies in a guest system directory"),
        "{error}"
    );
    assert!(backend.started.borrow().is_empty(), "nothing starts");
}

#[test]
fn mounts_only_plain_paths_outside_the_guests_system_directories() {
    for good in ["/Users/m/abc", "/home/m/abc", "/srv/work/abc-x"] {
        super::check_mount_paths(&[Path::new(good)], &production()).unwrap();
    }
    for bad in [
        "relative/path",
        "/Users/m/a:b",
        "/Users/m/a,b",
        "/Users/m/a=b",
        "/Users/m/../etc",
        "/usr/local/bin",
        "/home/agent/x",
        "/abc/git",
        "/run/abc",
        "/etc",
        "/x",
        "/",
    ] {
        assert!(
            super::check_mount_paths(&[Path::new(bad)], &production()).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn refuses_to_start_without_the_subscription() {
    let world = world("");
    let backend = subscribed();
    let error = start_error(&world, &backend);
    assert!(
        error.contains("no Claude Code subscription token"),
        "{error}"
    );
    assert!(backend.calls.borrow().is_empty(), "nothing starts");
}

#[test]
fn refuses_a_session_on_an_api_key() {
    let world = world("t");
    let api_key = Fake {
        auth: r#"{"loggedIn": true, "authMethod": "api_key", "apiProvider": "firstParty"}"#
            .to_owned(),
        ready: true,
        ..Fake::default()
    };
    let error = start_error(&world, &api_key);
    assert!(error.contains("not on the subscription token"), "{error}");
}

#[test]
fn gives_up_on_a_vm_that_never_gets_ready() {
    let world = world("t");
    let backend = Fake::default();
    let error = start_error(&world, &backend);
    assert!(error.contains("abc-w did not get ready"), "{error}");
    let probes = backend
        .calls
        .borrow()
        .iter()
        .filter(|call| call.ends_with("test -e /run/abc/ready"))
        .count();
    assert_eq!(probes, 3, "as patient as the host says");
}

#[test]
fn starts_the_installed_broker_when_none_listens() {
    let mut world = world("t");
    let silent = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    world.host.home = silent.path().to_path_buf();
    let backend = Fake::default();
    let missing = start_error(&world, &backend);
    assert!(
        missing.contains("the token broker is not installed"),
        "{missing}"
    );
    let marker = world.root.join("started");
    script(
        &world.host.broker_exe,
        &format!("echo \"$*\" > {}\nexit 3\n", marker.display()),
    );
    world.host.patience = 100;
    let stopped = start_error(&world, &backend);
    assert!(stopped.contains("the token broker stopped"), "{stopped}");
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "broker --session abc-w\n"
    );
    script(&world.host.broker_exe, "sleep 30\n");
    world.host.patience = 2;
    let stuck = start_error(&world, &backend);
    assert!(stuck.contains("the token broker did not start"), "{stuck}");
}

#[test]
fn shells_auths_syncs_lists_and_stops() {
    let mut world = world("t");
    let backend = subscribed();
    let worktree = world.worktree.clone();
    start_ok(&world, &backend);
    let (code, _) = act(
        &world,
        &backend,
        Action::Shell {
            worktree: worktree.clone(),
            command: vec![],
        },
    );
    assert_eq!(code.unwrap(), ExitCode::from(5), "the command's exit code");
    act(
        &world,
        &backend,
        Action::Shell {
            worktree: worktree.clone(),
            command: vec!["bash".into(), "-l".into()],
        },
    )
    .0
    .unwrap();
    world.host.terminal = false;
    act(
        &world,
        &backend,
        Action::Shell {
            worktree: worktree.clone(),
            command: vec!["claude".into(), "-p".into()],
        },
    )
    .0
    .unwrap();
    let (_, auth) = act(
        &world,
        &backend,
        Action::Auth {
            worktree: worktree.clone(),
        },
    );
    assert_eq!(
        auth,
        "vm: claude auth status: loggedIn true, authMethod oauth_token, apiProvider firstParty\n"
    );
    let (_, synced) = act(&world, &backend, Action::Sync { worktree });
    assert!(synced.contains("already at"), "{synced}");
    let (_, listed) = act(&world, &backend, Action::Ls);
    assert_eq!(listed, "NAME\nabc-w\n");
    stop_session(&world, &backend, false);
    assert!(
        world.root.join("state/w").exists(),
        "a stop keeps the session's state"
    );
    stop_session(&world, &backend, true);
    assert!(!world.root.join("state/w").exists(), "a purge drops it");
    stop_session(&world, &backend, true);
    let calls = backend.calls.borrow();
    assert!(
        calls.contains(&"attach --tty abc-w agent-exec claude".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"attach --tty abc-w agent-exec bash -l".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"attach abc-w agent-exec claude -p".to_owned()),
        "no pseudo-terminal without a terminal: {calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|call| *call == "remove abc-w").count(),
        3
    );
    assert!(
        calls.contains(&"remove_volumes abc-w-target abc-w-tools abc-w-home".to_owned()),
        "{calls:?}"
    );
}

#[test]
fn runs_a_command_into_a_log_without_a_terminal() {
    let world = world("t");
    let backend = subscribed();
    start_ok(&world, &backend);
    let log = world.root.join("logs/run.log");
    let run = || {
        act(
            &world,
            &backend,
            Action::Run {
                worktree: world.worktree.clone(),
                log: log.clone(),
                command: vec!["claude".into(), "-p".into(), "hi".into()],
            },
        )
    };
    let missing = format!("{:#}", run().0.unwrap_err());
    assert!(missing.starts_with("could not create "), "{missing}");
    std::fs::create_dir(world.root.join("logs")).unwrap();
    std::fs::write(&log, "an older run\n").unwrap();
    let (code, out) = run();
    assert_eq!(code.unwrap(), ExitCode::from(6), "the command's exit code");
    assert_eq!(out, "", "the output goes to the log alone");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "one\ntwo\n",
        "the log is the run's own"
    );
    assert!(
        backend
            .calls
            .borrow()
            .contains(&"stream abc-w agent-exec claude -p hi".to_owned()),
        "as the agent: {:?}",
        backend.calls.borrow()
    );
}

/// The worktree's commit, as `vm gc` compares it with a merged pull request's.
fn head(worktree: &Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A GitHub CLI that lists the pull request merged from `feature` as `merged`, a JSON list, and
/// none from any other branch.
fn github_merged(world: &World, merged: &str) {
    script(
        &world.host.github_cli,
        &format!(
            "[ \"$1 $2 $3 $4 $5\" = 'pr list --state merged --head' ] || exit 9\n\
             [ \"$7 $8\" = '--json number,headRefOid' ] || exit 9\n\
             [ \"$6\" = feature ] && echo '{merged}' || echo '[]'\n"
        ),
    );
}

fn gc(world: &World, backend: &Fake, yes: bool) -> String {
    let (result, out) = act(world, backend, Action::Gc { yes });
    result.unwrap();
    out
}

/// 233 GB sat in the disks of finished sessions, three of them merged PRs' target disks of about
/// 57 GB. `vm gc` lists the sessions whose worktree is gone or whose branch GitHub merged, with
/// their disk, and purges them only with --yes.
#[test]
fn gc_lists_finished_sessions_and_purges_them_only_when_told() {
    let world = world("t");
    let backend = Fake {
        volumes: vec![
            ("abc-w-target".to_owned(), 57_000_000_000),
            ("abc-w-tools".to_owned(), 2_000_000_000),
            ("abc-w-home".to_owned(), 300_000_000),
            ("abc-gone-target".to_owned(), 41_250_000_000),
            ("abc-gone-home".to_owned(), 100_000_000),
            ("someone-elses".to_owned(), 9_000_000_000),
        ],
        ..subscribed()
    };
    start_ok(&world, &backend);
    std::fs::create_dir_all(world.root.join("state/old/git")).unwrap();
    github_merged(
        &world,
        &format!(
            "[{{\"number\":32,\"headRefOid\":\"{}\"}}]",
            head(&world.worktree)
        ),
    );
    let listed = gc(&world, &backend, false);
    assert_eq!(
        listed,
        "abc-gone\t41.3 GB\tits worktree is gone\n\
         abc-old\t0.0 GB\tits worktree is gone\n\
         abc-w\t59.3 GB\tfeature merged in #32\n\
         vm gc: 3 finished sessions hold 100.6 GB; `cargo xtask vm gc --yes` purges them\n"
    );
    assert!(
        !backend
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("remove")),
        "nothing purged without --yes"
    );
    let purged = gc(&world, &backend, true);
    assert!(purged.contains("vm gc: purged abc-gone; "), "{purged}");
    assert!(purged.ends_with("vm gc: freed 100.6 GB\n"), "{purged}");
    let calls = backend.calls.borrow();
    for name in ["abc-gone", "abc-old", "abc-w"] {
        assert!(calls.contains(&format!("remove {name}")), "{calls:?}");
        assert!(
            calls.contains(&format!(
                "remove_volumes {name}-target {name}-tools {name}-home"
            )),
            "{calls:?}"
        );
    }
    assert!(!world.root.join("state/w").exists(), "as stop --purge does");
    assert!(!world.root.join("state/old").exists());
}

#[test]
fn gc_keeps_running_and_unmerged_sessions_and_commits_made_after_the_merge() {
    let world = world("t");
    let volumes = vec![("abc-w-target".to_owned(), 1_000_000_000)];
    let backend = Fake {
        volumes: volumes.clone(),
        ..subscribed()
    };
    start_ok(&world, &backend);
    let commit = head(&world.worktree);
    let none = "vm gc: no finished session to purge\n";
    github_merged(&world, "[]");
    assert_eq!(gc(&world, &backend, true), none, "not merged");
    github_merged(
        &world,
        "[{\"number\":7,\"headRefOid\":\"0123456789012345678901234567890123456789\"}]",
    );
    assert_eq!(
        gc(&world, &backend, true),
        none,
        "merged at another commit: the worktree has commits since"
    );
    github_merged(
        &world,
        &format!("[{{\"number\":7,\"headRefOid\":\"{commit}\"}}]"),
    );
    let running = Fake {
        running: true,
        volumes,
        ..subscribed()
    };
    assert_eq!(gc(&world, &running, true), none, "running");
    // The VM committed after the merge: its repository's branch moved on.
    let repo = world.root.join("state/w/git");
    let empty = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args([
            "commit-tree",
            "-m",
            "made in the VM",
            "-p",
            "refs/heads/feature",
            "refs/heads/feature^{tree}",
        ])
        .env("GIT_AUTHOR_NAME", "a")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "a")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .unwrap();
    assert!(empty.status.success(), "{empty:?}");
    let later = String::from_utf8(empty.stdout).unwrap();
    let updated = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["update-ref", "refs/heads/feature", later.trim()])
        .status()
        .unwrap();
    assert!(updated.success());
    assert_eq!(
        gc(&world, &backend, true),
        none,
        "the VM's commits after the merge are kept"
    );
    script(&world.host.github_cli, "echo 'not logged in' >&2\nexit 4\n");
    let failed = format!(
        "{:#}",
        act(&world, &backend, Action::Gc { yes: true })
            .0
            .unwrap_err()
    );
    assert!(
        failed.ends_with("pr list failed: not logged in"),
        "{failed}"
    );
    assert!(
        !backend
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("remove")),
        "nothing purged"
    );
}

#[test]
fn stop_says_what_became_of_the_broker() {
    let world = world("t");
    let backend = subscribed();
    let (result, out) = act(
        &world,
        &backend,
        Action::Stop {
            worktree: world.worktree.clone(),
            purge: false,
        },
    );
    result.unwrap();
    assert!(
        out.starts_with("vm: stopped the token broker; revoked 0 of 0 tokens")
            && out.ends_with(&format!("vm: stopped {}\n", world.worktree.display())),
        "{out}"
    );
}

#[test]
fn builds_the_image_and_loads_it() {
    let world = world("t");
    let log = world.root.join("built");
    script(
        &world.host.builder,
        &format!(
            "echo \"$*\" >> {}\n[ \"$1\" = image ] && printf 'oci-bytes'\nexit 0\n",
            log.display()
        ),
    );
    let backend = Fake::default();
    let (result, out) = act(&world, &backend, Action::Image);
    result.unwrap();
    assert_eq!(out, "vm: built and loaded abc-agent:latest\n");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "build --file {repo}/xtask/vm/Containerfile --tag abc-agent:latest {repo}\nimage save abc-agent:latest\n",
            repo = world.host.repo.display()
        )
    );
    assert_eq!(
        backend.calls.borrow().as_slice(),
        ["load_image abc-agent:latest oci-bytes"]
    );
    script(&world.host.builder, "exit 1\n");
    let build = format!("{:#}", act(&world, &backend, Action::Image).0.unwrap_err());
    assert!(build.ends_with("build failed"), "{build}");
    script(&world.host.builder, "[ \"$1\" = build ]\n");
    let error = format!("{:#}", act(&world, &backend, Action::Image).0.unwrap_err());
    assert!(error.ends_with("image save failed"), "{error}");
}

#[test]
fn the_billing_check_reads_the_sessions_settings() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let worktree = dir.path().join("w");
    std::fs::create_dir_all(worktree.join(".claude")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let env = |name: &str| match name {
        "HOME" => Some(home.clone().into_os_string()),
        "ABC_WORKTREE" => Some(worktree.clone().into_os_string()),
        _ => None,
    };
    billing(&env).unwrap();
    std::fs::write(
        worktree.join(".claude/settings.local.json"),
        r#"{"apiKeyHelper": "x"}"#,
    )
    .unwrap();
    assert!(format!("{:#}", billing(&env).unwrap_err()).contains("apiKeyHelper"));
    std::fs::write(home.join(".claude.json"), r#"{"primaryApiKey": "x"}"#).unwrap();
    std::fs::remove_file(worktree.join(".claude/settings.local.json")).unwrap();
    assert!(format!("{:#}", billing(&env).unwrap_err()).contains("primaryApiKey"));
    let world = world("t");
    let key = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| OsString::from("k"));
    let error = format!(
        "{:#}",
        run(
            Action::Billing,
            &world.host,
            &Fake::default(),
            &key,
            &mut Vec::new()
        )
        .unwrap_err()
    );
    assert!(
        error.contains("ANTHROPIC_API_KEY in the environment"),
        "{error}"
    );
}

#[test]
fn finds_its_places_on_this_host() {
    let env = |name: &str| (name == "XDG_STATE_HOME").then(|| OsString::from("/xdg/state"));
    let host = Host::this(Path::new("/repo"), Path::new("/home/m"), &env).unwrap();
    assert_eq!(host.state, Path::new("/xdg/state/abc/vm"));
    assert_eq!(host.home, Path::new("/home/m"));
    assert_eq!(
        host.broker_exe,
        Path::new("/home/m/.local/share/abc/broker/bin/xtask")
    );
    assert_eq!(host.repo, Path::new("/repo"));
    assert_eq!(host.patience, 150);
    let msb = microsandbox(
        PathBuf::from("/repo/.tools/msb/0.7.13/msb"),
        Path::new("/home/m"),
        &none,
    )
    .unwrap();
    assert_eq!(msb.home, Path::new("/home/m/.local/share/abc/msb/0.7.13"));
    let data = |name: &str| (name == "XDG_DATA_HOME").then(|| OsString::from("/xdg/data"));
    let elsewhere = microsandbox(
        PathBuf::from("/t/msb/1.0.0/msb"),
        Path::new("/home/m"),
        &data,
    )
    .unwrap();
    assert_eq!(elsewhere.home, Path::new("/xdg/data/abc/msb/1.0.0"));
    assert!(microsandbox(PathBuf::from("/msb"), Path::new("/h"), &none).is_err());
}

#[test]
fn each_os_has_its_keychain_and_builder() {
    assert_eq!(Os::MacOs.builder(), "container");
    assert_eq!(Os::Linux.builder(), "docker");
    assert_eq!(
        Os::MacOs.secret("item"),
        (
            "security",
            vec![
                "find-generic-password".into(),
                "-s".into(),
                "item".into(),
                "-w".into()
            ]
        )
    );
    assert_eq!(
        Os::Linux.secret("item"),
        (
            "secret-tool",
            vec!["lookup".into(), "service".into(), "item".into()]
        )
    );
}

#[test]
fn runs_only_from_the_main_clone() {
    let dir = tempfile::tempdir().unwrap();
    let main = repository(&dir.path().join("main"));
    refuse_linked_worktree(&main).unwrap();
    let linked = dir.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );
    let error = format!("{:#}", refuse_linked_worktree(&linked).unwrap_err());
    assert!(error.contains("run it from the main clone"), "{error}");
}

#[test]
fn needs_kvm_on_linux_only() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("kvm");
    require_kvm(Os::MacOs, &missing).unwrap();
    let error = format!("{:#}", require_kvm(Os::Linux, &missing).unwrap_err());
    assert!(error.contains("agent VMs on Linux need KVM"), "{error}");
    std::fs::write(&missing, "").unwrap();
    require_kvm(Os::Linux, &missing).unwrap();
}

#[test]
fn git_in_the_vm_asks_the_broker_and_never_a_stored_credential() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(&dir.path().join("repo"));
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[credential]\n\thelper = \"!f() { echo username=user; echo password=user-secret; }; f\"\n",
    )
    .unwrap();
    // The image's xtask stands in as a fake that answers as the broker's token would.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    script(
        &bin.join("abc-xtask"),
        "[ \"$*\" = 'git-credential get' ] || exit 9\necho username=x-access-token\necho password=app-token\n",
    );
    let system = crate::repo_root().unwrap().join("xtask/vm/gitconfig");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut child = std::process::Command::new("git")
        .args(["credential", "fill"])
        .current_dir(&repo)
        .env("HOME", &home)
        .env("PATH", path)
        .env("GIT_CONFIG_SYSTEM", system)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_COUNT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        b"protocol=https\nhost=github.com\n\n",
    )
    .unwrap();
    let filled = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    assert!(filled.contains("password=app-token"), "{filled}");
    assert!(
        !filled.contains("user-secret"),
        "the empty helper drops others: {filled}"
    );
}

#[test]
fn the_image_installs_the_chromium_of_the_playwright_the_web_app_pins() {
    let root = crate::repo_root().unwrap();
    let containerfile = std::fs::read_to_string(root.join("xtask/vm/Containerfile")).unwrap();
    let package: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("web/package.json")).unwrap())
            .unwrap();
    let pinned = package["devDependencies"]["@playwright/test"]
        .as_str()
        .unwrap();
    let image: Vec<&str> = containerfile
        .lines()
        .filter_map(|line| line.strip_prefix("ARG PLAYWRIGHT_VERSION="))
        .collect();
    assert_eq!(
        image,
        [pinned],
        "the Containerfile's PLAYWRIGHT_VERSION is web/package.json's @playwright/test"
    );
    for line in [
        "ENV PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright",
        "--package=\"@playwright/test@${PLAYWRIGHT_VERSION}\"",
        "playwright install chromium",
    ] {
        assert!(
            containerfile.contains(line),
            "the image installs Chromium into its fixed path: {line}"
        );
    }
}
