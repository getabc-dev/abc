//! Agent VMs, `cargo xtask vm` (D37): one microsandbox microVM per agent session, for one
//! worktree.
//!
//! The VM sees only the worktree, read-write at its own path, and the session's own git
//! repository ([`session`]); its build output, Linux tools and home are disk volumes of its own.
//! Postgres runs inside it, the agent can reach only the hosts in `xtask/vm/egress-allow.txt`
//! ([`egress`]), and it gets agent App tokens from the host's broker over vsock, never the key
//! ([`crate::broker`]). Claude Code runs on the maintainer's subscription token, piped from the
//! keychain into the VM's memory, and nothing else ([`mod@billing`]).
//!
//! The maintainer runs these commands from the main clone, which agents do not work in: they run
//! on the host with the maintainer's full access.

mod billing;
mod egress;
mod msb;
pub(crate) mod runs;
mod session;

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output, Stdio};
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use clap::Subcommand;

use session::{Push, Session};

/// What `cargo xtask vm` does.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Start the session for WORKTREE: its git repository, its VM with Postgres, the Claude Code
    /// subscription token from the keychain, and a check that Claude Code uses only that.
    Start { worktree: PathBuf },
    /// Run COMMAND in the session as the agent; Claude Code when none is given. With a terminal
    /// this is the interactive way in; `vm run` drives a session from a script.
    Shell {
        worktree: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Start a supervised run of BRIEF (a file in the worktree, such as .abc/brief.md) in the
    /// session for WORKTREE, as a host service that survives reboots: fresh `claude -p` sessions
    /// carry the brief on from the checkpoint .abc/progress.md across usage limits and outages
    /// until the agent hands off PASS, FAIL or BLOCKED. Refuses while another run is active.
    Run { worktree: PathBuf, brief: String },
    /// List the queued runs; `vm queue add` queues one, to start when the active run ends.
    Queue {
        #[command(subcommand)]
        action: Option<QueueAction>,
    },
    /// List every run with its status, attempts, brief and log (in
    /// ~/.local/state/abc/runs/<run>/); --json prints each run's state file.
    Runs {
        #[arg(long)]
        json: bool,
    },
    /// Cancel run RUN: stop its supervisor and its claude, and start the next queued run.
    Cancel { run: String },
    /// The supervisor of run RUN, which its host service runs.
    #[command(hide = true)]
    Supervise { run: String },
    /// Show how Claude Code in the session is signed in, without secrets.
    Auth { worktree: PathBuf },
    /// Move the worktree's branch to the commits made in the session.
    Sync { worktree: PathBuf },
    /// Bring the worktree branch's commits made on the host into the running session's
    /// repository, fast-forward only; refuses when the session's branch has diverged.
    Push { worktree: PathBuf },
    /// Stop the session's VM and its token broker, and delete the VM with its logs; --purge also
    /// deletes its volumes and git repository.
    Stop {
        worktree: PathBuf,
        #[arg(long)]
        purge: bool,
    },
    /// List the sessions that are not running and whose worktree is gone or whose branch GitHub
    /// merged, with the disk their volumes take; --yes purges them as `vm stop --purge` does.
    Gc {
        #[arg(long)]
        yes: bool,
    },
    /// Build the agent image from xtask/vm/Containerfile and load it into microsandbox.
    Image,
    /// List the agent VMs.
    Ls,
    /// Inside an agent VM: fail unless Claude Code can use only the subscription token.
    #[command(hide = true)]
    Billing,
}

/// What `cargo xtask vm queue` does besides listing the queue.
#[derive(Debug, Subcommand)]
pub(crate) enum QueueAction {
    /// Queue a supervised run of BRIEF in the session for WORKTREE, as `vm run` starts one; it
    /// starts at once when no run is active.
    Add { worktree: PathBuf, brief: String },
}

/// The GitHub CLI, which `vm gc` asks which branches GitHub merged.
const GITHUB_CLI: &str = "gh";

/// The image every session runs.
const IMAGE: &str = "abc-agent:latest";

/// The vsock port the VM reaches the token broker on.
const BROKER_PORT: u32 = 5000;

/// The broker's socket inside the VM (`xtask/vm/entrypoint.sh` relays it to the vsock port).
const GUEST_BROKER: &str = "/run/abc/agent/broker.sock";

/// The keychain item holding the maintainer's `claude setup-token` token.
pub(crate) const TOKEN_ITEM: &str = "abc-vm-claude-code-oauth-token";

/// The installed broker, relative to the data directory (`cargo xtask broker --help`).
const BROKER_EXE: &str = "abc/broker/bin/xtask";

/// A VM to start.
#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) cpus: u32,
    pub(crate) memory: String,
    /// Hosts it may reach over HTTPS.
    pub(crate) egress: Vec<String>,
    /// Host directories and where the VM sees them.
    pub(crate) mounts: Vec<(PathBuf, PathBuf)>,
    pub(crate) volumes: Vec<Volume>,
    pub(crate) tmpfs: Vec<PathBuf>,
    /// The host socket forwarded over vsock, and its port.
    pub(crate) vsock: (PathBuf, u32),
    pub(crate) env: Vec<(String, String)>,
}

/// A disk volume of the session's, named after the VM.
#[derive(Debug)]
pub(crate) struct Volume {
    pub(crate) name: &'static str,
    pub(crate) guest: PathBuf,
    pub(crate) size: &'static str,
}

/// The names of session `session`'s volumes.
fn volume_names(session: &str) -> Vec<String> {
    volumes(Path::new("/"))
        .iter()
        .map(|volume| format!("{session}-{}", volume.name))
        .collect()
}

/// The volumes of a session in `worktree`.
fn volumes(worktree: &Path) -> Vec<Volume> {
    vec![
        Volume {
            name: "target",
            guest: worktree.join("target"),
            size: "60G",
        },
        Volume {
            name: "tools",
            guest: worktree.join(".tools"),
            size: "4G",
        },
        Volume {
            name: "home",
            guest: PathBuf::from("/home/agent"),
            size: "8G",
        },
    ]
}

/// What runs VMs.
pub(crate) trait Backend {
    fn is_running(&self, name: &str) -> anyhow::Result<bool>;
    fn start(&self, spec: &Spec) -> anyhow::Result<()>;
    /// Runs `command` as root in VM `name` with `stdin`, capturing its output.
    fn exec(&self, name: &str, command: &[OsString], stdin: &[u8]) -> anyhow::Result<Output>;
    /// Runs `command` as root in VM `name` with this process's stdin, stdout and stderr. With
    /// `tty` the VM gives it a pseudo-terminal; without, the streams pass through as they are.
    fn attach(&self, name: &str, command: &[OsString], tty: bool) -> anyhow::Result<ExitStatus>;
    /// Runs `command` as root in VM `name` with no stdin and no pseudo-terminal, writing its
    /// stdout and stderr to `log` a line at a time as they come.
    fn stream(
        &self,
        name: &str,
        command: &[OsString],
        log: &mut dyn Write,
    ) -> anyhow::Result<ExitStatus>;
    /// Stops and deletes VM `name` with its logs; a VM that is gone is fine.
    fn remove(&self, name: &str) -> anyhow::Result<()>;
    fn remove_volumes(&self, names: &[String]) -> anyhow::Result<()>;
    /// Every volume's name and the bytes it takes on the host's disk.
    fn volumes(&self) -> anyhow::Result<Vec<(String, u64)>>;
    fn list(&self) -> anyhow::Result<String>;
    fn load_image(&self, archive: &mut dyn Read, tag: &str) -> anyhow::Result<()>;
}

/// The host operating systems sessions run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Os {
    MacOs,
    Linux,
}

impl Os {
    fn host() -> anyhow::Result<Self> {
        match std::env::consts::OS {
            "macos" => Ok(Self::MacOs),
            "linux" => Ok(Self::Linux),
            other => bail!("agent VMs run on macOS and Linux, not {other}"),
        }
    }

    /// The program that builds and saves OCI images: Apple's `container` on macOS, Docker (or
    /// Podman's `docker`) on Linux.
    fn builder(self) -> &'static str {
        match self {
            Self::MacOs => "container",
            Self::Linux => "docker",
        }
    }

    /// The command that prints the keychain item `item`: the login keychain on macOS, the Secret
    /// Service (libsecret) on Linux.
    fn secret(self, item: &str) -> (&'static str, Vec<String>) {
        match self {
            Self::MacOs => (
                "security",
                ["find-generic-password", "-s", item, "-w"]
                    .map(str::to_owned)
                    .into(),
            ),
            Self::Linux => (
                "secret-tool",
                ["lookup", "service", item].map(str::to_owned).into(),
            ),
        }
    }
}

/// The host's places and programs a session uses.
pub(crate) struct Host {
    pub(crate) os: Os,
    /// The repository xtask was built from: the main clone.
    pub(crate) repo: PathBuf,
    /// Where sessions keep their state.
    pub(crate) state: PathBuf,
    /// Where supervised runs keep their state and logs.
    pub(crate) runs: PathBuf,
    /// The user's home directory, under which each session's broker listens.
    pub(crate) home: PathBuf,
    /// The installed broker, started when none is listening.
    pub(crate) broker_exe: PathBuf,
    /// The program printing the subscription token, with its arguments.
    pub(crate) secret: (PathBuf, Vec<String>),
    /// The program that builds and saves OCI images.
    pub(crate) builder: PathBuf,
    /// How long to wait for a VM, in tries a fifth of a second apart.
    pub(crate) patience: u32,
    /// How long to wait for a token broker to stop when asked, then after SIGTERM.
    pub(crate) broker_patience: Duration,
    /// The GitHub CLI, which says which branches GitHub merged.
    pub(crate) github_cli: PathBuf,
    /// Guest directories no mount may cover: [`RESERVED`].
    pub(crate) reserved: Vec<PathBuf>,
    /// Whether stdin is a terminal: `shell` attaches a pseudo-terminal only then, so a headless
    /// `claude -p` gets plain pipes and the command's exit status.
    pub(crate) terminal: bool,
}

impl Host {
    /// This machine, for xtask built from `repo`, run by the user whose home is `home`.
    pub(crate) fn this(
        repo: &Path,
        home: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<Self> {
        let os = Os::host()?;
        let dir = |name: &str, fallback: &str| {
            env(name)
                .filter(|value| !value.is_empty())
                .map_or_else(|| home.join(fallback), PathBuf::from)
        };
        let (program, args) = os.secret(TOKEN_ITEM);
        Ok(Self {
            os,
            repo: repo.to_path_buf(),
            state: dir("XDG_STATE_HOME", ".local/state").join("abc/vm"),
            runs: dir("XDG_STATE_HOME", ".local/state").join("abc/runs"),
            home: home.to_path_buf(),
            broker_exe: dir("XDG_DATA_HOME", ".local/share").join(BROKER_EXE),
            secret: (PathBuf::from(program), args),
            builder: PathBuf::from(os.builder()),
            patience: 150,
            broker_patience: crate::broker::STOP_PATIENCE,
            github_cli: PathBuf::from(GITHUB_CLI),
            reserved: RESERVED.iter().map(PathBuf::from).collect(),
            terminal: std::io::stdin().is_terminal(),
        })
    }
}

/// microsandbox at `exe`, as `cargo xtask setup` installed it under its version, with its state
/// under the data directory of the user whose home is `home`.
pub(crate) fn microsandbox(
    exe: PathBuf,
    home: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> anyhow::Result<msb::Msb> {
    let version = exe
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("msb is not installed under its version")?
        .to_owned();
    let data = env("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join(".local/share"), PathBuf::from);
    Ok(msb::Msb {
        exe,
        home: msb::home(&data, &version),
    })
}

/// Refuses to run from a linked worktree: these commands run on the host with the maintainer's
/// full access, so they come from the main clone, which agents do not work in.
pub(crate) fn refuse_linked_worktree(repo: &Path) -> anyhow::Result<()> {
    ensure!(
        !repo.join(".git").is_file(),
        "`cargo xtask vm` runs on the host with your full access, so run it from the main clone, \
         not from the worktree {}, where an agent may have changed xtask",
        repo.display()
    );
    Ok(())
}

/// On Linux, microsandbox needs KVM: fails with what to do when `/dev/kvm` at `kvm` cannot be
/// opened for reading and writing.
pub(crate) fn require_kvm(os: Os, kvm: &Path) -> anyhow::Result<()> {
    if os == Os::Linux {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(kvm)
            .with_context(|| {
                format!(
                    "agent VMs on Linux need KVM, but {} cannot be opened for reading and writing; \
                     enable virtualization (nested virtualization on a cloud VM) and add yourself \
                     to the kvm group",
                    kvm.display()
                )
            })?;
    }
    Ok(())
}

/// Runs `action` on `host` with `backend`, installing supervisors with `services`, reporting to
/// `out`.
pub(crate) fn run(
    action: Action,
    host: &Host,
    backend: &dyn Backend,
    services: &dyn runs::service::Services,
    env: &dyn Fn(&str) -> Option<OsString>,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let all = runs::Runs {
        dir: host.runs.clone(),
    };
    let now = crate::progress::now();
    match action {
        Action::Start { worktree } => {
            start(
                &Session::open(&worktree, &host.repo, &host.state)?,
                host,
                backend,
                out,
            )?;
        }
        Action::Shell { worktree, command } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            return shell(&session, backend, command, host.terminal);
        }
        Action::Auth { worktree } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            writeln!(out, "vm: claude auth status: {}", auth(&session, backend)?)?;
        }
        Action::Sync { worktree } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            writeln!(out, "vm: {}", session.sync()?)?;
        }
        Action::Push { worktree } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            writeln!(out, "vm: {}", push(&session, backend)?)?;
        }
        Action::Run { worktree, brief } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            let run = runs::start(&all, services, &session, &brief, now)?;
            writeln!(
                out,
                "vm: started run {}; `cargo xtask vm runs` shows it, its log is {}",
                run.id,
                all.log(&run.id).display()
            )?;
        }
        Action::Queue { action: None } => runs::queued(&all, out)?,
        Action::Queue {
            action: Some(QueueAction::Add { worktree, brief }),
        } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            let run = runs::enqueue(&all, services, &session, &brief, now)?;
            writeln!(out, "vm: run {}: {}", run.id, run.status.describe())?;
        }
        Action::Runs { json } => runs::list(&all, json, out)?,
        Action::Cancel { run } => {
            writeln!(
                out,
                "vm: {}",
                runs::cancel(&all, services, backend, host, &run, now)?
            )?;
        }
        Action::Supervise { run } => {
            let status = runs::Supervisor {
                runs: &all,
                host,
                backend,
                services,
                clock: &runs::SystemClock,
            }
            .supervise(&run)?;
            writeln!(out, "vm: run {run}: {}", status.describe())?;
        }
        Action::Stop { worktree, purge } => {
            let session = Session::open(&worktree, &host.repo, &host.state)?;
            let broker = stop(&session.name, &session.state, host, backend, purge)?;
            writeln!(out, "vm: {broker}\nvm: stopped {}", worktree.display())?;
        }
        Action::Gc { yes } => gc(host, backend, yes, out)?,
        Action::Image => image(host, backend, out)?,
        Action::Ls => write!(out, "{}", backend.list()?)?,
        Action::Billing => billing(env)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask vm billing`, inside a VM: fails unless Claude Code there can use only the
/// subscription token.
pub(crate) fn billing(env: &dyn Fn(&str) -> Option<OsString>) -> anyhow::Result<()> {
    let home = env("HOME").map_or_else(|| PathBuf::from("/home/agent"), PathBuf::from);
    let worktree = env("ABC_WORKTREE").map_or_else(|| PathBuf::from("/nonexistent"), PathBuf::from);
    billing::check(
        env,
        &billing::settings_files(billing::managed(), &home, &worktree),
    )
}

fn start(
    session: &Session,
    host: &Host,
    backend: &dyn Backend,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let token = subscription_token(host)?;
    check_mount_paths(&[&session.worktree, &session.common], &host.reserved)?;
    let broker = crate::broker::files(&host.home, &session.name);
    ensure_broker(host, &session.name, &broker)?;
    if backend.is_running(&session.name)? {
        writeln!(
            out,
            "vm: {} is running; its git repository is refreshed only while it is stopped",
            session.name
        )?;
    } else {
        // The VM is stopped, so nothing can change the session repository while the host
        // rewrites it.
        if let Some(note) = session.prepare_git()? {
            writeln!(out, "vm: {note}")?;
        }
        for dir in ["target", ".tools"] {
            std::fs::create_dir_all(session.worktree.join(dir))?;
        }
        let egress = egress::parse(
            &std::fs::read_to_string(host.repo.join(egress::FILE))
                .with_context(|| format!("could not read {}", egress::FILE))?,
        )?;
        backend.start(&spec(session, &broker.tokens, egress))?;
    }
    wait_ready(session, backend, host.patience)?;
    let handed = backend.exec(
        &session.name,
        &words(&["sh", "-c", "umask 077 && cat > /run/abc/claude-token"]),
        &token,
    )?;
    ensure!(
        handed.status.success(),
        "could not hand the subscription token to {}: {}",
        session.name,
        String::from_utf8_lossy(&handed.stderr).trim()
    );
    let status = auth(session, backend)?;
    writeln!(
        out,
        "vm: {} is up; claude auth status: {status}\nvm: enter it with `cargo xtask vm shell {}`",
        session.name,
        session.worktree.display()
    )?;
    Ok(())
}

/// The VM to start for `session`, reaching its broker at `broker`.
fn spec(session: &Session, broker: &Path, egress: Vec<String>) -> Spec {
    let worktree = session.worktree.display().to_string();
    Spec {
        name: session.name.clone(),
        image: IMAGE.to_owned(),
        cpus: 6,
        memory: "6G".to_owned(),
        egress,
        mounts: vec![
            (session.worktree.clone(), session.worktree.clone()),
            (session.git_dir(), PathBuf::from(session::GUEST_GIT)),
        ],
        volumes: volumes(&session.worktree),
        tmpfs: vec![
            PathBuf::from("/run/abc"),
            PathBuf::from("/var/lib/postgresql"),
        ],
        vsock: (broker.to_path_buf(), BROKER_PORT),
        env: [
            ("ABC_VM", "1".to_owned()),
            ("ABC_WORKTREE", worktree),
            ("ABC_GIT_COMMON", session.common.display().to_string()),
            ("ABC_BROKER_VSOCK_PORT", BROKER_PORT.to_string()),
            (crate::broker::ENV, GUEST_BROKER.to_owned()),
            (
                crate::postgres::TEST_DATABASE_URL,
                "postgres://abc_test@127.0.0.1:5432/postgres".to_owned(),
            ),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
            // Claude Code names a usage limit's reset in its time zone, which the supervisor
            // reads as UTC (`runs::outcome::reset`).
            ("TZ", "UTC".to_owned()),
            ("DISABLE_AUTOUPDATER", "1".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect(),
    }
}

/// The maintainer's `claude setup-token` token from the keychain. It goes to the VM through a
/// pipe, never in an argument, the VM's configuration or a log.
fn subscription_token(host: &Host) -> anyhow::Result<Vec<u8>> {
    let (program, args) = &host.secret;
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {}", program.display()))?;
    let mut token = output.stdout;
    while token.last().is_some_and(u8::is_ascii_whitespace) {
        token.pop();
    }
    ensure!(
        output.status.success() && !token.is_empty(),
        "no Claude Code subscription token in the keychain item {TOKEN_ITEM}; run `claude \
         setup-token` and store what it prints (see CONTRIBUTING.md)"
    );
    Ok(token)
}

/// Makes sure session `session`'s token broker listens on the sockets of `files`, starting the
/// installed one when it does not.
fn ensure_broker(host: &Host, session: &str, files: &crate::broker::Files) -> anyhow::Result<()> {
    let listening = || std::os::unix::net::UnixStream::connect(&files.control).is_ok();
    if listening() {
        return Ok(());
    }
    ensure!(
        host.broker_exe.is_file(),
        "the token broker is not installed at {}; see `cargo xtask broker --help`",
        host.broker_exe.display()
    );
    let log = host.state.join(format!("{session}.broker.log"));
    std::fs::create_dir_all(&host.state)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("could not open {}", log.display()))?;
    let mut broker = Command::new(&host.broker_exe)
        .args(["broker", "--session", session])
        .env("HOME", &host.home)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()
        .with_context(|| format!("could not start {}", host.broker_exe.display()))?;
    for _ in 0..host.patience {
        if listening() {
            return Ok(());
        }
        if let Some(status) = broker.try_wait()? {
            bail!("the token broker stopped ({status}); see {}", log.display());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // It may be stuck; it will not be left running unseen.
    let _killed = broker.kill();
    bail!("the token broker did not start; see {}", log.display())
}

/// Guest system directories a mount must never cover: a mount there could replace the image's
/// own programs, such as agent-exec and abc-xtask, which run as root.
const RESERVED: [&str; 17] = [
    "/abc",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home/agent",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/tmp",
    "/usr",
    "/var",
];

/// Fails unless each of `paths` can be mounted at its own path in the guest: absolute, plain,
/// free of the separators microsandbox's mount syntax uses, and clear of the guest's system
/// directories.
fn check_mount_paths(paths: &[&Path], reserved: &[PathBuf]) -> anyhow::Result<()> {
    for path in paths {
        let text = path
            .to_str()
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        ensure!(
            path.is_absolute()
                && path
                    .components()
                    .skip(1)
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
                && !text.contains([':', ',', '\n', '='])
                && path.components().count() > 2,
            "{text} cannot be mounted in an agent VM: it must be an absolute, plain path without \
             ':', ',' or '='"
        );
        ensure!(
            !reserved.iter().any(|dir| path.starts_with(dir)),
            "{text} cannot be mounted in an agent VM: it lies in a guest system directory"
        );
    }
    Ok(())
}

/// Waits until the VM's entrypoint has readied it.
fn wait_ready(session: &Session, backend: &dyn Backend, patience: u32) -> anyhow::Result<()> {
    let probe = words(&["test", "-e", "/run/abc/ready"]);
    for _ in 0..patience {
        if backend.exec(&session.name, &probe, b"")?.status.success() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!(
        "{} did not get ready; `msb logs {}` says why",
        session.name,
        session.name
    )
}

/// How Claude Code in the session is signed in; fails unless it is the subscription token.
fn auth(session: &Session, backend: &dyn Backend) -> anyhow::Result<billing::Status> {
    let output = backend.exec(
        &session.name,
        &words(&["agent-exec", "claude", "auth", "status", "--json"]),
        b"",
    )?;
    billing::subscription(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("in {}", session.name))
}

fn shell(
    session: &Session,
    backend: &dyn Backend,
    command: Vec<OsString>,
    tty: bool,
) -> anyhow::Result<ExitCode> {
    let command = if command.is_empty() {
        vec![OsString::from("claude")]
    } else {
        command
    };
    let mut full = vec![OsString::from("agent-exec")];
    full.extend(command);
    let status = backend.attach(&session.name, &full, tty)?;
    Ok(ExitCode::from(crate::tools::status_code(status)))
}

/// Brings the worktree branch's new commits into the running VM of `session`: the VM's own git
/// fetches them from a bundle piped in, so the host never runs git in the repository the VM is
/// writing. Returns what changed, for the user.
fn push(session: &Session, backend: &dyn Backend) -> anyhow::Result<String> {
    ensure!(
        backend.is_running(&session.name)?,
        "{} is not running; `cargo xtask vm start` brings the worktree's commits in as it starts",
        session.name
    );
    let (branch, from, to, bundle) = match session.push()? {
        Push::Current(note) => return Ok(note),
        Push::Bundle {
            branch,
            from,
            to,
            bundle,
        } => (branch, from, to, bundle),
    };
    let mut command = words(&["agent-exec", "sh", "-c", session::APPLY_PUSH, "sh"]);
    command.push(OsString::from(&branch));
    let output = backend.exec(&session.name, &command, &bundle)?;
    ensure!(
        output.status.success(),
        "the VM did not take the commits: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(format!(
        "the VM's {branch}: {} -> {}; its index now matches the new commit",
        session::short(&from),
        session::short(&to)
    ))
}

/// Stops session `name`'s token broker and VM, and deletes the VM; with `purge`, also its volumes
/// and its directory `state`. A broker that does not answer is stopped by signal
/// ([`crate::broker::shut_down`]); the VM stops in any case, and a broker that may still run then
/// fails the stop, naming its pid. Returns what happened to the broker, for the user.
fn stop(
    name: &str,
    state: &Path,
    host: &Host,
    backend: &dyn Backend,
    purge: bool,
) -> anyhow::Result<String> {
    let broker = crate::broker::shut_down(
        &crate::broker::files(&host.home, name),
        name,
        host.broker_patience,
    );
    // The VM stops whatever became of the broker; a broker that may still run fails the stop
    // after it.
    backend.remove(name)?;
    if purge {
        backend.remove_volumes(&volume_names(name))?;
        match std::fs::remove_dir_all(state) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("could not delete {}", state.display()));
            }
            _ => {}
        }
    }
    broker
}

/// Why `vm gc` purges a session.
#[derive(Debug, PartialEq, Eq)]
enum Finished {
    /// No worktree of the main clone is named for it.
    WorktreeGone,
    /// GitHub merged pull request `number` from its worktree's branch, at the commit the worktree
    /// and the session's own repository are at.
    Merged { branch: String, number: u64 },
}

impl std::fmt::Display for Finished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorktreeGone => write!(f, "its worktree is gone"),
            Self::Merged { branch, number } => write!(f, "{branch} merged in #{number}"),
        }
    }
}

/// A finished session `vm gc` found.
struct Candidate {
    name: String,
    state: PathBuf,
    why: Finished,
    /// The bytes its volumes take.
    bytes: u64,
    /// Why it is kept, when its repository holds commits found nowhere else.
    kept: Option<String>,
}

/// `vm gc`: lists the sessions that are not running and are finished, with the disk their
/// volumes take, and purges them with `yes`.
///
/// "Merged" is GitHub's merged state of a pull request from the worktree's branch, not ancestry
/// in `origin/main`: pull requests are squash-merged, so a merged branch's commits are never
/// ancestors of main. The pull request must have been merged at the commit both the worktree's
/// branch and the session's own repository are at, so commits made after the merge, on the host
/// or in the VM, are never purged. A finished session whose repository holds commits that neither
/// `origin` nor a merged pull request has is kept, naming them, and `--yes` then fails after
/// purging the others.
fn gc(host: &Host, backend: &dyn Backend, yes: bool, out: &mut impl Write) -> anyhow::Result<()> {
    let candidates = finished(host, backend)?;
    if candidates.is_empty() {
        writeln!(out, "vm gc: no finished session to purge")?;
        return Ok(());
    }
    for candidate in &candidates {
        let size = size(candidate.bytes);
        match &candidate.kept {
            None => writeln!(out, "{}\t{size}\t{}", candidate.name, candidate.why)?,
            Some(kept) => writeln!(
                out,
                "{}\t{size}\tkept: {}, but {kept}",
                candidate.name, candidate.why
            )?,
        }
    }
    let (purge, kept): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|candidate| candidate.kept.is_none());
    if !purge.is_empty() {
        let total = size(purge.iter().map(|candidate| candidate.bytes).sum());
        if !yes {
            writeln!(
                out,
                "vm gc: {} finished sessions hold {total}; `cargo xtask vm gc --yes` purges them",
                purge.len()
            )?;
            return Ok(());
        }
        for candidate in &purge {
            let broker = stop(&candidate.name, &candidate.state, host, backend, true)?;
            writeln!(out, "vm gc: purged {}; {broker}", candidate.name)?;
        }
        writeln!(out, "vm gc: freed {total}")?;
    }
    if yes && !kept.is_empty() {
        bail!(
            "vm gc: {}; fetch their commits from those repositories, delete them, and run \
             `cargo xtask vm gc --yes` again",
            kept.iter()
                .map(|candidate| format!(
                    "kept {} ({})",
                    candidate.name,
                    candidate.state.join("git").display()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// The sessions that are not running and are finished, each with whether it must be kept.
fn finished(host: &Host, backend: &dyn Backend) -> anyhow::Result<Vec<Candidate>> {
    let worktrees = session::worktrees(&host.repo)?;
    let volumes = backend.volumes()?;
    let mut ids: std::collections::BTreeSet<session::Id> = volumes
        .iter()
        .filter_map(|(volume, _)| {
            volume_names("")
                .iter()
                .find_map(|suffix| volume.strip_suffix(suffix.as_str()))
                .and_then(session::Id::of_name)
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(&host.state) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().is_dir()
                && let Some(id) = entry.file_name().to_str().and_then(session::Id::of_slug)
            {
                ids.insert(id);
            }
        }
    }
    let mut candidates = Vec::new();
    for id in ids {
        let name = id.name();
        if backend.is_running(&name)? {
            continue;
        }
        let state = id.state(&host.state);
        let why = match worktrees.iter().find(|worktree| worktree.session == id) {
            None => Some(Finished::WorktreeGone),
            Some(worktree) => merged(host, worktree, &state)?,
        };
        let Some(why) = why else { continue };
        let kept = only_here(host, &state)?;
        let bytes: u64 = volumes
            .iter()
            .filter(|(volume, _)| volume_names(&name).contains(volume))
            .map(|(_, bytes)| bytes)
            .sum();
        candidates.push(Candidate {
            name,
            state,
            why,
            bytes,
            kept,
        });
    }
    Ok(candidates)
}

/// Why the finished session whose state is `state` must be kept, if it must: its repository
/// holds commits on no branch of `origin` and in no pull request GitHub merged from one of its
/// branches, or cannot be read.
fn only_here(host: &Host, state: &Path) -> anyhow::Result<Option<String>> {
    let unreadable = |error: anyhow::Error| format!("its repository cannot be read: {error:#}");
    let branches = match session::branches(state) {
        Ok(branches) => branches,
        Err(error) => return Ok(Some(unreadable(error))),
    };
    let mut merged = Vec::new();
    for (branch, _) in branches {
        if let Some(pull) = merged_pull(host, &branch)? {
            merged.push(pull.head_ref_oid);
        }
    }
    Ok(match session::unshared(state, &merged) {
        Ok(commits) if commits.is_empty() => None,
        Ok(commits) => Some(format!(
            "its repository has commits on neither origin nor a merged pull request: {}",
            commits.join(", ")
        )),
        Err(error) => Some(unreadable(error)),
    })
}

/// A pull request GitHub merged, as its CLI lists it.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pull {
    number: u64,
    head_ref_oid: String,
}

/// The pull request GitHub merged from `branch`, if any.
fn merged_pull(host: &Host, branch: &str) -> anyhow::Result<Option<Pull>> {
    let program = host.github_cli.display();
    let output = Command::new(&host.github_cli)
        .args(["pr", "list", "--state", "merged", "--head", branch])
        .args(["--json", "number,headRefOid", "--limit", "1"])
        .current_dir(&host.repo)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {program}"))?;
    ensure!(
        output.status.success(),
        "{program} pr list failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let pulls: Vec<Pull> = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{program} pr list printed no list of pull requests"))?;
    Ok(pulls.into_iter().next())
}

/// Why `worktree`'s session, whose state is `state`, is finished, if GitHub merged its branch at
/// the commit the worktree is at and the session repository's branch is at too. A session
/// repository without the branch is not at the merge: nothing shows what it holds is merged.
fn merged(
    host: &Host,
    worktree: &session::Worktree,
    state: &Path,
) -> anyhow::Result<Option<Finished>> {
    let Some(branch) = &worktree.branch else {
        return Ok(None);
    };
    let Some(pull) = merged_pull(host, branch)? else {
        return Ok(None);
    };
    let in_session = session::branches(state)?
        .into_iter()
        .find_map(|(name, commit)| (name == *branch).then_some(commit));
    Ok((worktree.commit == pull.head_ref_oid
        && in_session.as_deref() == Some(pull.head_ref_oid.as_str()))
    .then(|| Finished::Merged {
        branch: branch.clone(),
        number: pull.number,
    }))
}

/// `bytes` in gigabytes, to a tenth.
fn size(bytes: u64) -> String {
    let tenths = bytes / 100_000_000;
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// Builds the agent image with the host's OCI builder and loads it into the backend.
fn image(host: &Host, backend: &dyn Backend, out: &mut impl Write) -> anyhow::Result<()> {
    let builder = host.builder.display();
    let containerfile = host.repo.join("xtask/vm/Containerfile");
    let status = Command::new(&host.builder)
        .arg("build")
        .arg("--file")
        .arg(&containerfile)
        .args(["--tag", IMAGE])
        .arg(&host.repo)
        .status()
        .with_context(|| format!("could not run {builder}"))?;
    ensure!(status.success(), "{builder} build failed");
    let mut save = Command::new(&host.builder)
        .args(["image", "save", IMAGE])
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {builder}"))?;
    let mut archive = save
        .stdout
        .take()
        .context("the image save gave no output")?;
    backend.load_image(&mut archive, IMAGE)?;
    ensure!(save.wait()?.success(), "{builder} image save failed");
    writeln!(out, "vm: built and loaded {IMAGE}")?;
    Ok(())
}

fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests;
