use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus, Output};

use super::{
    Action, Backend, Host, Os, Sandbox, Spec, TOKEN_ITEM, billing, microsandbox,
    refuse_linked_worktree, require_kvm, run,
};
use crate::testing::{git, repository, script};

/// A backend that records what it is asked and answers from a script.
///
/// Models: microsandbox's `msb`, as [`Backend`] drives it. Environment: none, since msb's own
/// `MSB_HOME` is the real backend's concern. Streams: `exec` gives stdout and stderr apart, and
/// `list` is the JSON `msb ls --format json` prints on stdout, never msb's human table, which
/// goes to stderr. Exit codes: an `exec` carries the program's, and every other call succeeds.
/// `msb::tests::a_listing_means_the_same_to_the_fake_and_to_the_real_msb` holds the listing to the
/// real msb.
#[derive(Default)]
pub(super) struct Fake {
    pub(super) running: bool,
    /// What `claude auth status --json` prints in the VM.
    auth: String,
    /// Whether the VM ever gets ready.
    pub(super) ready: bool,
    pub(super) started: RefCell<Vec<Spec>>,
    pub(super) calls: RefCell<Vec<String>>,
    /// What each exec received on stdin.
    stdin: RefCell<Vec<Vec<u8>>>,
    /// Whether the VM's git refuses the commits `vm push` sends.
    refuses_push: bool,
    /// How many more network probes fail.
    pub(super) offline: std::cell::Cell<u32>,
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
            probe if probe.starts_with("agent-exec curl ") => {
                let offline = self.offline.get();
                self.offline.set(offline.saturating_sub(1));
                output(if offline > 0 { 6 } else { 0 }, "")
            }
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

    /// Models `msb ls --format json` as the real msb answers it
    /// (`msb::tests::a_listing_means_the_same_to_the_fake_and_to_the_real_msb`): a list of VMs,
    /// empty for none, and with `--running` only the running ones. The one VM here is `abc-w`,
    /// running when [`Fake::running`] says so and stopped otherwise. A failing msb is
    /// [`Msb`](super::msb::Msb)'s own test.
    fn list(&self, running_only: bool) -> anyhow::Result<Vec<Sandbox>> {
        if running_only && !self.running {
            return Ok(Vec::new());
        }
        Ok(vec![Sandbox {
            name: "abc-w".to_owned(),
            status: if self.running { "Running" } else { "Stopped" }.to_owned(),
        }])
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
pub(super) struct World {
    _dir: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) host: Host,
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

/// The host of a world at `root` whose user's home is `home`.
pub(super) fn host_at(root: &Path, home: &Path) -> Host {
    Host {
        os: Os::MacOs,
        repo: root.join("main"),
        state: root.join("state"),
        runs: root.join("runs"),
        home: home.to_path_buf(),
        broker_exe: root.join("no-broker"),
        secret: (
            root.join("security"),
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
    }
}

pub(super) fn world(token: &str) -> World {
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
    let host = host_at(&root, home.path());
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

/// Services that record what they are asked to do.
///
/// Models: the host's service manager (launchd or systemd) as `runs::service::Services` drives
/// it: install, start and retire a supervisor's service. Environment: none runs, so the empty
/// environment launchd gives a service is `service`'s own tests' concern (and
/// `crates/abc-host/tests/fakes.rs` holds a fake launchctl to it). Streams: none. Exit codes: every
/// call succeeds. No real launchd or systemd runs in CI.
#[derive(Default)]
pub(super) struct FakeServices {
    pub(super) calls: RefCell<Vec<String>>,
    /// Whether installing fails.
    pub(super) broken: bool,
}

impl super::runs::service::Services for FakeServices {
    fn install(&self, run: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("install {run}"));
        anyhow::ensure!(!self.broken, "launchctl said no");
        Ok(())
    }

    fn remove(&self, run: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("remove {run}"));
        Ok(())
    }

    fn retire(&self, run: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("retire {run}"));
        Ok(())
    }
}

/// What a run of an action did: its result, what a person would read (stderr), and what a script
/// would read (stdout).
pub(super) struct Streams {
    pub(super) result: anyhow::Result<ExitCode>,
    pub(super) stderr: String,
    pub(super) stdout: String,
}

pub(super) fn act_streams(
    world: &World,
    backend: &Fake,
    services: &FakeServices,
    action: Action,
) -> Streams {
    let (mut out, mut data) = (Vec::new(), Vec::new());
    let result = run(
        action,
        &world.host,
        backend,
        services,
        &none,
        &mut out,
        &mut data,
    );
    Streams {
        result,
        stderr: String::from_utf8(out).unwrap(),
        stdout: String::from_utf8(data).unwrap(),
    }
}

/// [`act_streams`] with both streams as one text, the data first.
pub(super) fn act_with(
    world: &World,
    backend: &Fake,
    services: &FakeServices,
    action: Action,
) -> (anyhow::Result<ExitCode>, String) {
    let streams = act_streams(world, backend, services, action);
    (streams.result, streams.stdout + &streams.stderr)
}

fn act(world: &World, backend: &Fake, action: Action) -> (anyhow::Result<ExitCode>, String) {
    act_with(world, backend, &FakeServices::default(), action)
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
pub(super) fn subscribed() -> Fake {
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
fn lists_the_vms_as_text_and_as_json_on_stdout() {
    let world = world("t");
    let backend = subscribed();
    let listed = act_streams(
        &world,
        &backend,
        &FakeServices::default(),
        Action::Ls { json: false },
    );
    assert_eq!(listed.stdout, "abc-w\tStopped\n", "this VM is not running");
    let running = Fake {
        running: true,
        ..Fake::default()
    };
    assert_eq!(
        running.list(true).unwrap(),
        [Sandbox {
            name: "abc-w".to_owned(),
            status: "Running".to_owned()
        }]
    );
    assert_eq!(backend.list(true).unwrap(), [], "--running leaves it out");
    assert_eq!(listed.stderr, "", "no message beside the listing");
    let json = act_streams(
        &world,
        &backend,
        &FakeServices::default(),
        Action::Ls { json: true },
    );
    assert_eq!(
        serde_json::from_str::<Vec<Sandbox>>(&json.stdout).unwrap(),
        [Sandbox {
            name: "abc-w".to_owned(),
            status: "Stopped".to_owned()
        }],
        "JSON on stdout"
    );
    assert_eq!(json.stderr, "", "nothing else is said");
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
    // A session repository that holds no branch.
    let old = world.root.join("state/old/git");
    std::fs::create_dir_all(&old).unwrap();
    git(&old, &["init", "--quiet", "--bare"]);
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
    let short = |full: &str| crate::git::short(full).to_owned();
    assert_eq!(
        gc(&world, &backend, true),
        format!(
            "abc-w\tnot finished: feature merged in #7 at 012345678901, but the VM's feature is \
             at {}, so what it holds is not all merged\n{none}",
            short(&commit)
        ),
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
        format!(
            "abc-w\tnot finished: feature merged in #7 at {}, but the VM's feature is at {}, so \
             what it holds is not all merged\n{none}",
            short(&commit),
            short(later.trim())
        ),
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
            &FakeServices::default(),
            &key,
            &mut Vec::new(),
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
    let system = crate::testing::checkout().join("xtask/vm/gitconfig");
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
    let root = crate::testing::checkout();
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

/// Makes a bare repository at `repo` whose branch `branch` holds one commit, `subject`, that no
/// remote has; returns the commit.
fn unpushed(repo: &Path, branch: &str, subject: &str) -> String {
    std::fs::create_dir_all(repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet", "--bare"]);
    let tree = git(&["mktree"]);
    let commit = git(&["commit-tree", "-m", subject, &tree]);
    git(&["update-ref", &format!("refs/heads/{branch}"), &commit]);
    commit
}

/// `vm gc` named a session's state by stripping every leading `abc-` from its name: the session
/// of worktree `abc-cli` (`abc-abc-cli`) got `<state>/cli`, another session's, and the session of
/// worktree `abc` got `<state>/` itself, so `--yes` deleted every session's state. Each session's
/// name and state come from its slug alone.
#[test]
fn gc_purges_only_each_sessions_own_state() {
    let world = world("t");
    git(
        &world.host.repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "cli",
            world.root.join("cli").to_str().unwrap(),
        ],
    );
    github_merged(&world, "[]");
    let state = &world.host.state;
    std::fs::create_dir_all(state.join("cli")).unwrap();
    std::fs::write(state.join("cli/kept"), "the live session cli's").unwrap();
    std::fs::create_dir_all(state.join("w")).unwrap();
    std::fs::write(state.join("w/kept"), "the live session w's").unwrap();
    std::fs::create_dir_all(state.join("abc-cli")).unwrap();
    std::fs::create_dir_all(state.join("abc")).unwrap();
    let backend = Fake {
        volumes: vec![
            ("abc-abc-cli-target".to_owned(), 1_000_000_000),
            ("abc-abc-home".to_owned(), 1_000_000_000),
        ],
        ..subscribed()
    };
    let listed = gc(&world, &backend, false);
    assert_eq!(
        listed,
        "abc-abc\t1.0 GB\tits worktree is gone\n\
         abc-abc-cli\t1.0 GB\tits worktree is gone\n\
         vm gc: 2 finished sessions hold 2.0 GB; `cargo xtask vm gc --yes` purges them\n"
    );
    gc(&world, &backend, true);
    assert!(state.join("cli/kept").is_file(), "session abc-cli's state");
    assert!(state.join("w/kept").is_file(), "session abc-w's state");
    assert!(!state.join("abc-cli").exists(), "session abc-abc-cli's own");
    assert!(!state.join("abc").exists(), "session abc-abc's own");
    let calls = backend.calls.borrow();
    for name in ["abc-abc", "abc-abc-cli"] {
        assert!(calls.contains(&format!("remove {name}")), "{calls:?}");
    }
    assert!(!calls.contains(&"remove abc-cli".to_owned()), "{calls:?}");
}

/// The session repository of a session whose worktree is gone may hold the only copy of commits
/// made in the VM. `vm gc` keeps such a session, naming the commits, and purges the others.
#[test]
fn gc_keeps_a_gone_sessions_commits_that_are_nowhere_else() {
    let world = world("t");
    github_merged(&world, "[]");
    let state = &world.host.state;
    let lost = unpushed(&state.join("gone/git"), "feature", "only in the VM");
    unpushed(&state.join("done/git"), "other", "pushed");
    let pushed = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(state.join("done/git"))
        .args([
            "update-ref",
            "refs/remotes/origin/other",
            "refs/heads/other",
        ])
        .status()
        .unwrap();
    assert!(pushed.success());
    std::fs::create_dir_all(state.join("broken/git")).unwrap();
    let backend = subscribed();
    let listed = gc(&world, &backend, false);
    assert!(
        listed.contains(
            "abc-broken\t0.0 GB\tkept: its worktree is gone, but its repository cannot be read: "
        ),
        "{listed}"
    );
    let kept = format!(
        "abc-gone\t0.0 GB\tkept: its worktree is gone, but its repository has commits on neither \
         origin nor a merged pull request: {} only in the VM\n",
        crate::git::short(&lost)
    );
    assert!(
        listed.contains("abc-done\t0.0 GB\tits worktree is gone\n"),
        "{listed}"
    );
    assert!(listed.contains(&kept), "{listed}");
    let (result, out) = act(&world, &backend, Action::Gc { yes: true });
    let error = format!("{:#}", result.unwrap_err());
    assert!(out.contains("vm gc: purged abc-done; "), "{out}");
    assert!(error.contains("kept abc-gone"), "{error}");
    assert!(state.join("gone/git").is_dir(), "its commits are kept");
    assert!(!state.join("done").exists());
    assert!(
        !backend
            .calls
            .borrow()
            .contains(&"remove abc-gone".to_owned()),
        "{:?}",
        backend.calls.borrow()
    );
}

/// A session whose repository lacks the worktree's branch was counted as merged: no commit there
/// compared with the merged pull request's. Only a branch at that commit is merged.
#[test]
fn gc_needs_the_sessions_branch_at_the_merged_commit() {
    let world = world("t");
    github_merged(
        &world,
        &format!(
            "[{{\"number\":32,\"headRefOid\":\"{}\"}}]",
            head(&world.worktree)
        ),
    );
    let backend = Fake {
        volumes: vec![("abc-w-target".to_owned(), 1_000_000_000)],
        ..subscribed()
    };
    assert_eq!(
        gc(&world, &backend, true),
        format!(
            "abc-w\tnot finished: feature merged in #32 at {}, but the session's repository has \
             no feature, so nothing shows its commits are merged\n\
             vm gc: no finished session to purge\n",
            crate::git::short(&head(&world.worktree))
        )
    );
}

/// The agent pushed from its VM and the pull request merged, but the host's worktree was never
/// synced, so it lagged behind the merge, and `vm gc` said only "no finished session". When the
/// session's branch is at the merge, `gc` syncs the worktree forward and finds it finished; when
/// the worktree cannot be synced, it says why.
#[test]
fn gc_syncs_a_worktree_behind_the_merge_its_vm_pushed() {
    let world = world("t");
    let backend = Fake {
        volumes: vec![("abc-w-target".to_owned(), 1_000_000_000)],
        ..subscribed()
    };
    start_ok(&world, &backend);
    let before = head(&world.worktree);
    let repo = world.root.join("state/w/git");
    let made = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args([
            "commit-tree",
            "-m",
            "pushed from the VM",
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
    assert!(made.status.success(), "{made:?}");
    let pushed = String::from_utf8(made.stdout).unwrap().trim().to_owned();
    let moved = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["update-ref", "refs/heads/feature", &pushed])
        .status()
        .unwrap();
    assert!(moved.success());
    github_merged(
        &world,
        &format!("[{{\"number\":9,\"headRefOid\":\"{pushed}\"}}]"),
    );
    // A commit on the host after the VM's push: the worktree cannot be synced.
    git(
        &world.worktree,
        &[
            "-c",
            "user.name=m",
            "-c",
            "user.email=m@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "made on the host",
        ],
    );
    let diverged = gc(&world, &backend, false);
    assert!(
        diverged.starts_with(&format!(
            "abc-w\tnot finished: feature merged in #9 at {}, which the VM's feature is at, but \
             the worktree's is at ",
            crate::git::short(&pushed)
        )) && diverged.contains("which the merge does not hold")
            && diverged.ends_with("vm gc: no finished session to purge\n"),
        "{diverged}"
    );
    assert!(
        gc(&world, &backend, true).contains(
            "could not be synced: the worktree's feature has \
             commits the VM's lacks; merge them by hand\n"
        ),
        "--yes tries, and says why it cannot"
    );
    git(&world.worktree, &["reset", "--quiet", "--hard", &before]);
    let listed = gc(&world, &backend, false);
    assert_eq!(
        listed,
        format!(
            "vm gc: the worktree's feature is behind the merge at {}; --yes syncs it first\n\
             abc-w\t1.0 GB\tfeature merged in #9\n\
             vm gc: 1 finished sessions hold 1.0 GB; `cargo xtask vm gc --yes` purges them\n",
            crate::git::short(&before)
        )
    );
    assert_eq!(head(&world.worktree), before, "listing changes nothing");
    let purged = gc(&world, &backend, true);
    assert!(
        purged.starts_with(&format!(
            "vm gc: synced the worktree's feature: {} -> {}\n",
            crate::git::short(&before),
            crate::git::short(&pushed)
        )) && purged.contains("vm gc: purged abc-w; "),
        "{purged}"
    );
    assert_eq!(
        head(&world.worktree),
        pushed,
        "the worktree's branch moved forward"
    );
}
