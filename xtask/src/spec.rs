//! `cargo xtask spec`: the formal specs in `spec/`, checked with the pinned Quint, Apalache and
//! JRE (`xtask/tools.toml`), as `cargo xtask check` and CI run it.
//!
//! The gate typechecks every Quint file, runs the specs' tests, has TLC explore every state of
//! each model, checking its invariant and its temporal property, and requires TLC to find each
//! seeded bug: a model with a fault built in must fail the property that guards against it, so a
//! property that stops catching its bug fails the gate. `docs/spec/runner.md` explains the
//! runner's spec; the product brief records why TLC checks it.
//!
//! Nothing runs from `PATH` or downloads at run time. Quint runs with an empty environment but
//! for `PATH`, holding only the pinned JRE's `bin/`, for the TLC it starts, and `QUINT_HOME`,
//! a read-only directory holding only the pinned Apalache where Quint looks for it, so any
//! attempt to fetch another fails. Quint's tests and simulations run on its TypeScript evaluator,
//! which is built in. Quint turns a spec into TLA+ for TLC through an Apalache server; the gate
//! starts one on a free port for its run and stops it when the run ends. Apalache's server
//! listens on every interface and has no option to listen on loopback alone, so the port is
//! open to the machine's network while the gate runs; see `docs/spec/runner.md`.

use std::fs::{self, Permissions};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// The specs' directory, relative to the repository root.
pub(crate) const DIR: &str = "spec";

/// The invariant every model must hold: the conjunction of the spec's safety properties.
const INVARIANT: &str = "safety";

/// The temporal property every model must hold: under fairness, no run stalls silently.
const TEMPORAL: &str = "noSilentStall";

/// The seed of every randomized test and simulation, so each run of the gate is the same.
const SEED: &str = "0x5eed";

/// How many samples a randomized Quint test tries.
const TEST_SAMPLES: &str = "100";

/// How many ITF traces `--traces` writes per model, and how many steps each takes at most.
const TRACES_PER_MODEL: &str = "100";
const TRACE_STEPS: &str = "100";

/// How often, 200 ms apart, the gate looks for the Apalache server listening before it gives up:
/// two minutes.
const SERVER_START_CHECKS: u32 = 600;

/// The Apalache version Quint is told to use, which only names the directory in the gate's
/// `QUINT_HOME` where Quint finds the pinned Apalache.
const APALACHE_VERSION: &str = "pinned";

/// The JVM memory TLC may take: a 1 GiB heap, where Quint's default of 8 GiB is more than many
/// machines have, and 16 MiB stacks for its threads rather than Quint's 515 MiB, so that it starts
/// within the 2 GiB a test may make writable under `cargo xtask mutants`. The gate's models need
/// far less.
const TLC_CONFIG: &str = r#"{"maxHeap": "-Xmx1g", "stackSize": "-Xss16m"}"#;

/// What the gate checks, by file, relative to the repository root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checks<'a> {
    /// Files of Quint tests, each with the module whose `run` definitions ending in `Test` are
    /// the tests.
    pub(crate) tests: &'a [Module<'a>],
    /// Models TLC explores in full, each of which must hold [`INVARIANT`] and [`TEMPORAL`].
    pub(crate) models: &'a [Module<'a>],
    /// Models with a seeded bug, each of which must fail its property.
    pub(crate) seeded: &'a [Seeded<'a>],
}

/// A module of a Quint file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Module<'a> {
    pub(crate) file: &'a str,
    pub(crate) main: &'a str,
}

/// A model with a seeded bug, and the property TLC must find it breaks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seeded<'a> {
    pub(crate) model: Module<'a>,
    pub(crate) property: &'a str,
}

/// The graph runner's spec (`spec/runner/`).
pub(crate) const RUNNER: Checks<'static> = Checks {
    tests: &[Module {
        file: "spec/runner/tests.qnt",
        main: "tests",
    }],
    models: &[
        Module {
            file: "spec/runner/model.qnt",
            main: "leases",
        },
        Module {
            file: "spec/runner/model.qnt",
            main: "flow",
        },
        Module {
            file: "spec/runner/model.qnt",
            main: "contention",
        },
    ],
    seeded: &[
        Seeded {
            model: Module {
                file: "spec/runner/seeded.qnt",
                main: "keepLeaseOnExpiry",
            },
            property: "oneLiveLeasePerStep",
        },
        Seeded {
            model: Module {
                file: "spec/runner/seeded.qnt",
                main: "applyStaleResult",
            },
            property: "resultAppliedOnce",
        },
    ],
};

/// The pinned tools the gate runs.
#[derive(Debug, Clone)]
pub(crate) struct Tools {
    quint: PathBuf,
    java: PathBuf,
    /// Apalache's distribution: the directory holding `bin/` and `lib/apalache.jar`.
    apalache: PathBuf,
}

impl Tools {
    /// The pinned tools, from `tool`.
    pub(crate) fn pinned(tool: &dyn Fn(&str) -> anyhow::Result<PathBuf>) -> anyhow::Result<Self> {
        let launcher = tool("apalache")?;
        let apalache = launcher
            .parent()
            .and_then(Path::parent)
            .with_context(|| format!("{} is not in a bin/ directory", launcher.display()))?
            .to_path_buf();
        Ok(Self {
            quint: tool("quint")?,
            java: tool("jre")?,
            apalache,
        })
    }
}

/// Whether a change to `path` can change what the gate finds: a spec, a pinned tool, or the gate
/// itself.
pub(crate) fn affected_by(path: &Path) -> bool {
    path.starts_with(DIR)
        || path == Path::new("xtask/tools.toml")
        || path == Path::new("xtask/src/spec.rs")
}

/// Whether the tree at `root` holds specs at all.
pub(crate) fn present(root: &Path) -> bool {
    root.join(DIR).is_dir()
}

/// How a run of the gate goes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options<'a> {
    /// Where to write ITF traces of each model's simulations, if anywhere.
    pub(crate) traces: Option<&'a Path>,
}

/// Where the gate keeps its files: the Quint home, the server's directory and temporary files,
/// all under the tree's `target/spec/`, emptied at the start of each run.
struct Workspace {
    quint_home: PathBuf,
    server: PathBuf,
    tmp: PathBuf,
    tlc_config: PathBuf,
}

impl Workspace {
    /// Empties `target/spec/` under `root` and lays out a read-only Quint home that holds only
    /// the pinned Apalache, where Quint looks for its version.
    fn create(root: &Path, tools: &Tools) -> anyhow::Result<Self> {
        let dir = crate::web::target_dir(root).join("spec");
        remove_tree(&dir)?;
        let quint_home = dir.join("quint-home");
        let dist = quint_home.join(format!("apalache-dist-{APALACHE_VERSION}"));
        let server = dir.join("apalache");
        let tmp = dir.join("tmp");
        for path in [&dist, &server, &tmp] {
            fs::create_dir_all(path)
                .with_context(|| format!("could not create {}", path.display()))?;
        }
        std::os::unix::fs::symlink(&tools.apalache, dist.join("apalache"))
            .with_context(|| format!("could not link Apalache into {}", dist.display()))?;
        for path in [&dist, &quint_home] {
            fs::set_permissions(path, Permissions::from_mode(0o555))
                .with_context(|| format!("could not make {} read-only", path.display()))?;
        }
        let tlc_config = dir.join("tlc.json");
        fs::write(&tlc_config, TLC_CONFIG)
            .with_context(|| format!("could not write {}", tlc_config.display()))?;
        Ok(Self {
            quint_home,
            server,
            tmp,
            tlc_config,
        })
    }
}

/// Removes `dir` and everything in it, if it exists, first making each directory writable again:
/// the gate leaves its Quint home read-only.
fn remove_tree(dir: &Path) -> anyhow::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if metadata.is_dir() {
        fs::set_permissions(dir, Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} writable", dir.display()))?;
        for entry in
            fs::read_dir(dir).with_context(|| format!("could not list {}", dir.display()))?
        {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(dir).with_context(|| format!("could not remove {}", dir.display()))
    } else {
        fs::remove_file(dir).with_context(|| format!("could not remove {}", dir.display()))
    }
}

/// Why the Apalache server did not start.
#[derive(Debug)]
enum Start {
    /// It exited, as when another process holds its port.
    Exited(anyhow::Error),
    /// Anything else.
    Failed(anyhow::Error),
}

/// How many ports the server is tried on before it counts as unable to start.
const SERVER_PORTS: u32 = 3;

/// The Apalache server Quint compiles specs to TLA+ through, stopped when dropped.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    /// Starts the pinned Apalache's server on a free port, its log in the workspace, and waits
    /// until it listens, looking `checks` times, 200 ms apart. A port another process takes
    /// first makes the server exit; it is tried again on another, [`SERVER_PORTS`] times.
    fn start(tools: &Tools, workspace: &Workspace, checks: u32) -> anyhow::Result<Self> {
        let mut tries: u32 = 1;
        loop {
            match Self::start_on(tools, workspace, free_port()?, checks) {
                Err(Start::Exited(_)) if tries < SERVER_PORTS => tries = tries.saturating_add(1),
                Err(Start::Exited(error) | Start::Failed(error)) => return Err(error),
                Ok(server) => return Ok(server),
            }
        }
    }

    /// Starts the server on `port`, and waits until it says it listens there, and does.
    fn start_on(
        tools: &Tools,
        workspace: &Workspace,
        port: u16,
        checks: u32,
    ) -> Result<Self, Start> {
        let log_path = workspace.server.join("server.log");
        let log = fs::File::create(&log_path)
            .with_context(|| format!("could not create {}", log_path.display()))
            .map_err(Start::Failed)?;
        let child = Command::new(&tools.java)
            .args(server_args(&tools.apalache, port))
            .current_dir(&workspace.server)
            .env_clear()
            .env("TMPDIR", &workspace.tmp)
            .stdin(Stdio::null())
            .stdout(
                log.try_clone()
                    .context("could not share the server's log")
                    .map_err(Start::Failed)?,
            )
            .stderr(log)
            .spawn()
            .with_context(|| format!("could not run {}", tools.java.display()))
            .map_err(Start::Failed)?;
        let mut server = Self { child, port };
        // Apalache says so once it is bound: a process already listening on the port would answer
        // a connection too.
        let listening = format!("is running on port {port}");
        for _ in 0..checks {
            let exited = server
                .child
                .try_wait()
                .context("could not check on the Apalache server")
                .map_err(Start::Failed)?;
            if let Some(status) = exited {
                return Err(Start::Exited(anyhow::anyhow!(
                    "the Apalache server exited with {status} before it listened; its log: {}",
                    log_tail(&log_path)
                )));
            }
            let said = fs::read_to_string(&log_path).unwrap_or_default();
            if said.contains(&listening) && TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
            {
                return Ok(server);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(Start::Failed(anyhow::anyhow!(
            "the Apalache server did not listen on port {port} after {checks} checks 200 ms \
             apart; its log: {}",
            log_tail(&log_path)
        )))
    }

    /// The endpoint Quint connects to.
    fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Server {
    /// Stops the server, however the run ended.
    fn drop(&mut self) {
        if self.child.kill().is_ok() {
            let _exit_status_of_a_killed_server = self.child.wait();
        }
    }
}

/// The JVM arguments that run Apalache's server from the distribution at `apalache` on `port`.
/// Apalache's own launcher passes the same unsafe-memory permission on Java 24 and newer.
fn server_args(apalache: &Path, port: u16) -> Vec<String> {
    vec![
        "--sun-misc-unsafe-memory-access=allow".to_owned(),
        "-Xmx1g".to_owned(),
        "-jar".to_owned(),
        apalache
            .join("lib")
            .join("apalache.jar")
            .to_string_lossy()
            .into_owned(),
        "server".to_owned(),
        format!("--port={port}"),
    ]
}

/// A port no one listens on now.
fn free_port() -> anyhow::Result<u16> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("could not find a free port")?;
    Ok(listener
        .local_addr()
        .context("could not read the free port")?
        .port())
}

/// The last lines of the log at `path`, or why there are none.
fn log_tail(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines
                .get(lines.len().saturating_sub(20)..)
                .unwrap_or_default()
                .join("\n")
        }
        Err(error) => format!("unreadable ({error})"),
    }
}

/// A failed step of the gate, and what to do about it.
#[derive(Debug, PartialEq, Eq)]
struct Failure {
    step: String,
    fix: &'static str,
}

/// How to fix a spec that does not typecheck.
const TYPECHECK_FIX: &str = "fix each type error above; the gate stops here, as nothing else can \
                             run on a spec that does not typecheck";

/// How to fix a failing test.
const TEST_FIX: &str = "fix the spec, or the test where the behaviour it pins changed on \
                        purpose; never delete or weaken a test to pass";

/// How to fix a model that breaks a property.
const MODEL_FIX: &str = "the trace above is a behaviour that breaks the property: fix the spec \
                         if it allows a wrong behaviour, or the property if it states the wrong \
                         requirement, and say which in docs/spec/runner.md";

/// How to fix a seeded bug that TLC no longer finds.
const SEEDED_FIX: &str = "a property no longer catches the bug it exists for: restore it, or \
                          seed a bug it does catch and say why in docs/spec/runner.md";

/// Runs the gate on the tree at `root` with the tools `find_tools` finds, looked for only once
/// the tree holds specs, writing every step's output to `out`, and reports whether it passed.
pub(crate) fn run(
    root: &Path,
    checks: &Checks<'_>,
    find_tools: &dyn Fn() -> anyhow::Result<Tools>,
    options: Options<'_>,
    out: &mut impl Write,
) -> anyhow::Result<bool> {
    if !present(root) {
        writeln!(out, "xtask spec: the tree holds no specs ({DIR}/)")?;
        return Ok(false);
    }
    let started = Instant::now();
    let tools = &find_tools()?;
    let workspace = Workspace::create(root, tools)?;
    let mut gate = Gate {
        root,
        tools,
        workspace: &workspace,
        out,
        failures: Vec::new(),
    };
    gate.all(checks, options)?;
    let Gate {
        failures,
        out: report,
        ..
    } = gate;
    let seconds = started.elapsed().as_secs();
    if failures.is_empty() {
        writeln!(report, "xtask spec: passed in {seconds}s")?;
        return Ok(true);
    }
    writeln!(report, "xtask spec: failed in {seconds}s")?;
    for failure in failures {
        writeln!(report, "  {}: {}", failure.step, failure.fix)?;
    }
    Ok(false)
}

/// A run of the gate.
struct Gate<'a, W> {
    root: &'a Path,
    tools: &'a Tools,
    workspace: &'a Workspace,
    out: &'a mut W,
    failures: Vec<Failure>,
}

impl<W: Write> Gate<'_, W> {
    /// Every step of the gate, in order: typecheck, tests, models, seeded bugs, then the traces.
    fn all(&mut self, checks: &Checks<'_>, options: Options<'_>) -> anyhow::Result<()> {
        for file in quint_files(&self.root.join(DIR))? {
            let shown = file
                .strip_prefix(self.root)
                .unwrap_or(&file)
                .display()
                .to_string();
            let (passed, _) = self.quint(&["typecheck".to_owned(), shown.clone()])?;
            if !passed {
                self.failures.push(Failure {
                    step: format!("typecheck {shown}"),
                    fix: TYPECHECK_FIX,
                });
            }
        }
        if !self.failures.is_empty() {
            return Ok(());
        }
        for module in checks.tests {
            let (passed, _) = self.quint(&test_args(module))?;
            if !passed {
                self.failures.push(Failure {
                    step: format!("tests of {}", module.main),
                    fix: TEST_FIX,
                });
            }
        }
        if !checks.models.is_empty() || !checks.seeded.is_empty() {
            let server = Server::start(self.tools, self.workspace, SERVER_START_CHECKS)?;
            let endpoint = server.endpoint();
            for module in checks.models {
                let args = self.verify_args(module, &endpoint, &[INVARIANT], Some(TEMPORAL));
                let (passed, _) = self.quint(&args)?;
                if !passed {
                    self.failures.push(Failure {
                        step: format!("model {}", module.main),
                        fix: MODEL_FIX,
                    });
                }
            }
            for seeded in checks.seeded {
                let args = self.verify_args(&seeded.model, &endpoint, &[seeded.property], None);
                let (passed, printed) = self.quint(&args)?;
                match found(passed, &printed) {
                    Found::Violation => writeln!(
                        self.out,
                        "xtask spec: TLC found the seeded bug {}, as it must: it breaks {}",
                        seeded.model.main, seeded.property
                    )?,
                    Found::Nothing => self.failures.push(Failure {
                        step: format!(
                            "seeded bug {}: TLC found no violation of {}",
                            seeded.model.main, seeded.property
                        ),
                        fix: SEEDED_FIX,
                    }),
                    Found::Error => self.failures.push(Failure {
                        step: format!("seeded bug {}: TLC failed to run", seeded.model.main),
                        fix: MODEL_FIX,
                    }),
                }
            }
            drop(server);
        }
        if let Some(dir) = options.traces {
            self.traces(checks, dir)?;
        }
        Ok(())
    }

    /// Simulates each model and writes its runs to `dir` as ITF traces, with the action each
    /// step took and its nondeterministic choices.
    fn traces(&mut self, checks: &Checks<'_>, dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let dir = dir
            .canonicalize()
            .with_context(|| format!("could not resolve {}", dir.display()))?;
        for module in checks.models {
            let (passed, _) = self.quint(&trace_args(module, &dir))?;
            if !passed {
                self.failures.push(Failure {
                    step: format!("traces of {}", module.main),
                    fix: MODEL_FIX,
                });
            }
        }
        Ok(())
    }

    /// `quint verify` of `module` with TLC, for `invariants` and `temporal` if given.
    fn verify_args(
        &self,
        module: &Module<'_>,
        endpoint: &str,
        invariants: &[&str],
        temporal: Option<&str>,
    ) -> Vec<String> {
        let mut args = vec![
            "verify".to_owned(),
            "--backend=tlc".to_owned(),
            format!("--main={}", module.main),
            format!("--server-endpoint={endpoint}"),
            format!("--apalache-version={APALACHE_VERSION}"),
            format!("--tlc-config={}", self.workspace.tlc_config.display()),
            format!("--invariant={}", invariants.join(",")),
        ];
        if let Some(temporal) = temporal {
            args.push(format!("--temporal={temporal}"));
        }
        args.push(module.file.to_owned());
        args
    }

    /// Runs the pinned Quint with `args` in the tree's root, its output passed on to `out` and
    /// returned, and reports whether it passed.
    fn quint(&mut self, args: &[String]) -> anyhow::Result<(bool, String)> {
        writeln!(self.out, "xtask: quint {}", args.join(" "))?;
        let java_bin = self
            .tools
            .java
            .parent()
            .context("the JRE's java has no directory")?;
        let mut command = Command::new(&self.tools.quint);
        command
            .args(args)
            .current_dir(self.root)
            .env_clear()
            .env("PATH", java_bin)
            .env("QUINT_HOME", &self.workspace.quint_home)
            .env("TMPDIR", &self.workspace.tmp)
            .env("NO_COLOR", "1");
        let started = Instant::now();
        let (passed, printed) = crate::check::tee(&mut command, self.out, true)?;
        let printed = String::from_utf8_lossy(&printed).into_owned();
        writeln!(
            self.out,
            "xtask: quint {} took {}s",
            args.first().map_or("", String::as_str),
            started.elapsed().as_secs()
        )?;
        Ok((passed, printed))
    }
}

/// What TLC made of a seeded bug.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// A counterexample: the property caught the bug.
    Violation,
    /// The property held: it no longer catches the bug.
    Nothing,
    /// TLC did not finish: a broken spec or tool, not a finding.
    Error,
}

/// What a `quint verify` that `passed` and printed `printed` found. Quint marks a property that
/// does not hold with `[violation]`, after TLC's trace of the behaviour that breaks it.
fn found(passed: bool, printed: &str) -> Found {
    if passed {
        Found::Nothing
    } else if printed
        .lines()
        .any(|line| line.trim_start().starts_with("[violation]"))
    {
        Found::Violation
    } else {
        Found::Error
    }
}

/// `quint test` of `module`'s tests, on the built-in evaluator with a fixed seed.
fn test_args(module: &Module<'_>) -> Vec<String> {
    vec![
        "test".to_owned(),
        "--backend=typescript".to_owned(),
        format!("--main={}", module.main),
        format!("--seed={SEED}"),
        format!("--max-samples={TEST_SAMPLES}"),
        module.file.to_owned(),
    ]
}

/// `quint run` of `module` that writes its simulations to `dir` as ITF traces, each step with
/// the action it took and its nondeterministic choices (`--mbt`), checking [`INVARIANT`] on the
/// way.
fn trace_args(module: &Module<'_>, dir: &Path) -> Vec<String> {
    vec![
        "run".to_owned(),
        "--backend=typescript".to_owned(),
        format!("--main={}", module.main),
        format!("--seed={SEED}"),
        format!("--invariant={INVARIANT}"),
        "--mbt".to_owned(),
        format!("--max-samples={TRACES_PER_MODEL}"),
        format!("--n-traces={TRACES_PER_MODEL}"),
        format!("--max-steps={TRACE_STEPS}"),
        format!(
            "--out-itf={}",
            dir.join(format!("{}_{{seq}}.itf.json", module.main))
                .display()
        ),
        module.file.to_owned(),
    ]
}

/// The Quint files under `dir`, in path order.
fn quint_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)
            .with_context(|| format!("could not list {}", current.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "qnt") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{
        Checks, Found, Module, Options, RUNNER, Seeded, Tools, Workspace, affected_by, found,
        log_tail, present, quint_files, remove_tree, run, server_args, test_args, trace_args,
    };
    use crate::testing::{pinned_for_tests, script};

    /// A counter that counts to three, and with `BUGGY` skips past it, with a test, a model
    /// whose properties hold, and the bug seeded.
    const COUNTER: &str = r"module counter {
  const BUGGY: bool
  var x: int
  action init = x' = 0
  action inc = all { x < 3, x' = if (BUGGY) x + 2 else x + 1 }
  action step = any { inc, x' = x }
  val safety = x <= 3
  val atMostThree = x <= 3
  temporal noSilentStall = weakFair(inc, x) implies eventually(x >= 3)
}

module model {
  import counter(BUGGY = false).*
}

module seeded {
  import counter(BUGGY = true).*
}

module tests {
  import counter(BUGGY = false).*
  run countsTest = init.then(inc).then(inc).expect(x == 2)
  run stopsAtThreeTest = init.then(inc).then(inc).then(inc).then(inc).fail()
}
";

    const FILE: &str = "spec/counter/counter.qnt";

    const MODEL: Module<'static> = Module {
        file: FILE,
        main: "model",
    };

    const SEEDED: Module<'static> = Module {
        file: FILE,
        main: "seeded",
    };

    const TESTS: Module<'static> = Module {
        file: FILE,
        main: "tests",
    };

    /// A tree whose `spec/` holds `files`.
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        root
    }

    fn pinned() -> Tools {
        Tools::pinned(&|name| Ok(pinned_for_tests(name))).unwrap()
    }

    /// Runs the gate with `checks` on `root`, and returns whether it passed and what it said.
    fn gate(root: &Path, checks: &Checks<'_>, traces: Option<&Path>) -> (bool, String) {
        let mut out = Vec::new();
        let passed = run(root, checks, &|| Ok(pinned()), Options { traces }, &mut out).unwrap();
        (passed, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_sound_spec_passes_and_its_seeded_bug_is_found() {
        let root = tree(&[(FILE, COUNTER)]);
        let traces = root.path().join("traces");
        let checks = Checks {
            tests: &[TESTS],
            models: &[MODEL],
            seeded: &[Seeded {
                model: SEEDED,
                property: "atMostThree",
            }],
        };
        let (passed, said) = gate(root.path(), &checks, Some(&traces));
        assert!(passed, "{said}");
        assert!(said.contains("2 passing"), "the tests ran: {said}");
        assert!(
            said.contains("Model checking completed. No error has been found."),
            "TLC checked the model: {said}"
        );
        assert!(
            said.contains("TLC found the seeded bug seeded, as it must: it breaks atMostThree"),
            "{said}"
        );
        assert!(said.contains("xtask spec: passed in "), "{said}");

        let written: Vec<PathBuf> = fs::read_dir(&traces)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(!written.is_empty(), "traces written");
        let trace: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&written[0]).unwrap()).unwrap();
        assert_eq!(trace["#meta"]["format"], "ITF", "{trace}");
        let vars = trace["vars"].as_array().unwrap();
        assert!(
            vars.iter().any(|var| var == "mbt::actionTaken"),
            "each step names its action: {trace}"
        );

        let home = root.path().join("target/spec/quint-home");
        let entries: Vec<String> = fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["apalache-dist-pinned"], "Quint fetched nothing");
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o222,
            0,
            "Quint's home is read-only"
        );
        let (again, said_again) = gate(root.path(), &checks, None);
        assert!(
            again,
            "a second run replaces the first's files: {said_again}"
        );
    }

    #[test]
    fn a_broken_property_a_failed_test_and_a_missed_bug_each_fail() {
        let failing = COUNTER.replace("expect(x == 2)", "expect(x == 3)");
        let root = tree(&[(FILE, &failing)]);
        let checks = Checks {
            tests: &[TESTS],
            models: &[SEEDED],
            seeded: &[Seeded {
                model: MODEL,
                property: "atMostThree",
            }],
        };
        let (passed, said) = gate(root.path(), &checks, None);
        assert!(!passed, "{said}");
        assert!(said.contains("countsTest failed"), "{said}");
        assert!(
            said.contains("State 3:") && said.contains("x = 4"),
            "the trace that breaks the property: {said}"
        );
        for failure in [
            "  tests of tests: fix the spec, or the test",
            "  model seeded: the trace above is a behaviour that breaks the property",
            "  seeded bug model: TLC found no violation of atMostThree: a property no longer \
             catches the bug it exists for",
        ] {
            assert!(said.contains(failure), "{failure}: {said}");
        }
    }

    #[test]
    fn a_spec_that_does_not_typecheck_stops_the_gate() {
        let root = tree(&[
            (FILE, COUNTER),
            (
                "spec/broken/broken.qnt",
                "module broken { val x: int = true }\n",
            ),
        ]);
        let checks = Checks {
            tests: &[TESTS],
            models: &[MODEL],
            seeded: &[],
        };
        let (passed, said) = gate(root.path(), &checks, None);
        assert!(!passed, "{said}");
        assert!(
            said.contains("  typecheck spec/broken/broken.qnt: fix each type error above"),
            "{said}"
        );
        assert!(
            said.contains("xtask: quint typecheck spec/counter/counter.qnt"),
            "every file is typechecked: {said}"
        );
        assert!(!said.contains("quint test"), "nothing else runs: {said}");
    }

    #[test]
    fn a_tree_without_specs_fails() {
        let root = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let unused = || -> anyhow::Result<Tools> { panic!("no tools needed") };
        assert!(
            !run(
                root.path(),
                &RUNNER,
                &unused,
                Options { traces: None },
                &mut out
            )
            .unwrap()
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "xtask spec: the tree holds no specs (spec/)\n"
        );
        assert!(!present(root.path()), "no spec/");
    }

    #[test]
    fn a_server_that_cannot_start_says_why() {
        let root = tree(&[(FILE, COUNTER)]);
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(&bin.join("java"), "echo 'no JVM here' >&2\nexit 3\n");
        let tools = Tools {
            quint: pinned_for_tests("quint"),
            java: bin.join("java"),
            apalache: root.path().join("apalache"),
        };
        let checks = Checks {
            tests: &[],
            models: &[MODEL],
            seeded: &[],
        };
        let error = run(
            root.path(),
            &checks,
            &|| Ok(tools.clone()),
            Options { traces: None },
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("the Apalache server exited with exit status: 3 before it listened")
                && error.contains("no JVM here"),
            "{error}"
        );
    }

    #[test]
    fn the_server_listens_until_it_is_dropped_and_waits_only_so_long() {
        let root = tempfile::tempdir().unwrap();
        let tools = pinned();
        let workspace = Workspace::create(root.path(), &tools).unwrap();
        let server = super::Server::start(&tools, &workspace, super::SERVER_START_CHECKS).unwrap();
        let port = server.port;
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "it listens"
        );
        drop(server);
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "a dropped server is stopped"
        );

        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(&bin.join("java"), "exec sleep 30\n");
        let silent = Tools {
            java: bin.join("java"),
            ..pinned()
        };
        let silence = super::Server::start(&silent, &workspace, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(
            silence.contains("did not listen on port") && silence.contains("after 1 checks"),
            "{silence}"
        );
        let squatter = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = squatter.local_addr().unwrap().port();
        assert!(
            matches!(
                super::Server::start_on(&silent, &workspace, taken, 3),
                Err(super::Start::Failed(_))
            ),
            "another process answering on the port is not the server"
        );
    }

    #[test]
    fn the_tools_come_from_the_pins() {
        let tools =
            Tools::pinned(&|name| Ok(PathBuf::from(format!("/t/{name}/1/d/bin/{name}")))).unwrap();
        assert_eq!(tools.quint, Path::new("/t/quint/1/d/bin/quint"));
        assert_eq!(tools.java, Path::new("/t/jre/1/d/bin/jre"));
        assert_eq!(
            tools.apalache,
            Path::new("/t/apalache/1/d"),
            "Apalache's distribution holds its bin/"
        );
        let error = Tools::pinned(&|_| Ok(PathBuf::from("/")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/ is not in a bin/ directory"), "{error}");
        assert!(
            Tools::pinned(&|name| anyhow::bail!("{name} is missing")).is_err(),
            "a missing tool fails"
        );
    }

    #[test]
    fn the_runners_checks_name_its_files() {
        let root = crate::testing::checkout();
        for module in RUNNER
            .tests
            .iter()
            .chain(RUNNER.models)
            .chain(RUNNER.seeded.iter().map(|seeded| &seeded.model))
        {
            let text = fs::read_to_string(root.join(module.file)).unwrap();
            assert!(
                text.contains(&format!("module {} {{", module.main)),
                "{} has module {}",
                module.file,
                module.main
            );
        }
        let runner = fs::read_to_string(root.join("spec/runner/runner.qnt")).unwrap();
        for property in RUNNER
            .seeded
            .iter()
            .map(|seeded| seeded.property)
            .chain([super::INVARIANT, super::TEMPORAL])
        {
            assert!(
                runner.contains(&format!("val {property}: bool"))
                    || runner.contains(&format!("temporal {property} ")),
                "runner.qnt defines {property}"
            );
        }
    }

    #[test]
    fn what_tlc_found_is_read_from_quints_verdict() {
        assert_eq!(found(true, "[ok] No violation found"), Found::Nothing);
        assert_eq!(
            found(false, "State 1: x = 0\n[violation] Found an issue (5ms).\n"),
            Found::Violation
        );
        assert_eq!(
            found(false, "  [violation] Found an issue"),
            Found::Violation,
            "indented"
        );
        assert_eq!(
            found(
                false,
                "[failure] TLC encountered an error\nno [violation] here"
            ),
            Found::Error,
            "only Quint's own verdict line counts"
        );
    }

    #[test]
    fn quint_runs_with_fixed_seeds_and_writes_traces_by_model() {
        assert_eq!(
            test_args(&TESTS),
            [
                "test",
                "--backend=typescript",
                "--main=tests",
                "--seed=0x5eed",
                "--max-samples=100",
                FILE
            ]
        );
        assert_eq!(
            trace_args(&MODEL, Path::new("/traces")),
            [
                "run",
                "--backend=typescript",
                "--main=model",
                "--seed=0x5eed",
                "--invariant=safety",
                "--mbt",
                "--max-samples=100",
                "--n-traces=100",
                "--max-steps=100",
                "--out-itf=/traces/model_{seq}.itf.json",
                FILE
            ]
        );
        assert_eq!(
            server_args(Path::new("/a"), 4321),
            [
                "--sun-misc-unsafe-memory-access=allow",
                "-Xmx1g",
                "-jar",
                "/a/lib/apalache.jar",
                "server",
                "--port=4321"
            ]
        );
    }

    #[test]
    fn only_specs_and_pinned_tools_affect_the_gate() {
        assert!(affected_by(Path::new("spec/runner/runner.qnt")));
        assert!(affected_by(Path::new("xtask/tools.toml")));
        assert!(
            affected_by(Path::new("xtask/src/spec.rs")),
            "the gate's own definition"
        );
        assert!(!affected_by(Path::new("docs/spec/runner.md")));
        assert!(!affected_by(Path::new("specs/x.qnt")));
    }

    #[test]
    fn finds_every_quint_file_in_path_order() {
        let root = tree(&[
            ("spec/b/b.qnt", ""),
            ("spec/a/z.qnt", ""),
            ("spec/a/a.qnt", ""),
            ("spec/a/notes.md", ""),
        ]);
        let files: Vec<String> = quint_files(&root.path().join("spec"))
            .unwrap()
            .iter()
            .map(|path| {
                path.strip_prefix(root.path())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(files, ["spec/a/a.qnt", "spec/a/z.qnt", "spec/b/b.qnt"]);
    }

    #[test]
    fn the_workspace_is_laid_out_fresh_and_removable() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("target/spec/stale");
        fs::create_dir_all(&stale).unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o555)).unwrap();
        let tools = Tools {
            quint: PathBuf::from("unused"),
            java: PathBuf::from("unused"),
            apalache: PathBuf::from("/opt/apalache"),
        };
        let workspace = Workspace::create(root.path(), &tools).unwrap();
        assert!(!stale.exists(), "a read-only leftover is removed");
        assert_eq!(
            fs::read_link(workspace.quint_home.join("apalache-dist-pinned/apalache")).unwrap(),
            Path::new("/opt/apalache")
        );
        assert_eq!(
            fs::read_to_string(&workspace.tlc_config).unwrap(),
            super::TLC_CONFIG
        );
        assert!(workspace.server.is_dir() && workspace.tmp.is_dir());
        remove_tree(&root.path().join("target/spec")).unwrap();
        assert!(!root.path().join("target/spec").exists());
        remove_tree(&root.path().join("target/spec")).unwrap();
    }

    #[test]
    fn a_log_tail_is_its_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let lines: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
        fs::write(&log, lines.join("\n")).unwrap();
        let tail = log_tail(&log);
        assert!(
            tail.starts_with("line 6\n") && tail.ends_with("line 25"),
            "{tail}"
        );
        assert!(log_tail(&dir.path().join("none")).starts_with("unreadable ("));
    }
}
