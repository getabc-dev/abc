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
            _ => output(0, ""),
        })
    }

    fn attach(&self, name: &str, command: &[OsString]) -> anyhow::Result<ExitStatus> {
        self.calls
            .borrow_mut()
            .push(format!("attach {name} {}", shown(command)));
        Ok(ExitStatus::from_raw(5 << 8))
    }

    fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.calls.borrow_mut().push(format!("remove {name}"));
        Ok(())
    }

    fn remove_volumes(&self, names: &[String]) -> anyhow::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("remove_volumes {}", names.join(" ")));
        Ok(())
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
    let sockets = crate::broker::sockets(home, "abc-w");
    std::fs::create_dir_all(sockets.control.parent().unwrap()).unwrap();
    let control = UnixListener::bind(&sockets.control).unwrap();
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
    // Not under /tmp: on Linux the temporary directory is a guest system directory, which a
    // session's worktree may not be.
    let target = crate::repo_root().unwrap().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let dir = tempfile::Builder::new().tempdir_in(target).unwrap();
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
        (
            crate::broker::sockets(&world.host.home, "abc-w").tokens,
            5000
        ),
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
fn mounts_only_plain_paths_outside_the_guests_system_directories() {
    for good in ["/Users/m/abc", "/home/m/abc", "/srv/work/abc-x"] {
        super::check_mount_paths(&[Path::new(good)]).unwrap();
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
            super::check_mount_paths(&[Path::new(bad)]).is_err(),
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
    let world = world("t");
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
        calls.contains(&"attach abc-w agent-exec claude".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"attach abc-w agent-exec bash -l".to_owned()),
        "{calls:?}"
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
