//! Repository automation for abc, run as `cargo xtask <command>`.

mod annotation;
mod app;
mod broker;
mod check;
mod coverage;
mod e2e;
mod fuzz;
mod gate;
mod git;
mod github;
mod guard;
mod hook;
mod integrity;
mod mutants;
mod postgres;
mod pr;
mod review;
mod rulesets;
mod shell;
mod stop;
mod task;
#[cfg(test)]
mod testing;
mod tools;
mod vm;
mod watchdog;
mod web;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Repository automation for abc")]
struct Cli {
    /// Check the tree at PATH instead of the repository xtask was built from. CI runs the base
    /// commit's xtask on a pull request's tree this way, so the pull request cannot change the
    /// gates that judge it; the pinned tools still come from xtask's own repository.
    #[arg(long, global = true, value_name = "PATH")]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Task,
}

/// What `cargo xtask rulesets` does.
#[derive(Clone, Copy, Debug, Subcommand)]
enum RulesetAction {
    /// Check .github/rulesets/main.json against the CI workflows.
    Check,
    /// Put the ruleset on GitHub with your own gh login, replacing the rulesets it supersedes.
    Apply,
}

/// What `cargo xtask task` does with the current task.
#[derive(Debug, Subcommand)]
enum TaskAction {
    /// Record the task you are starting, for the stop hook's watchdog.
    Set {
        /// What the task is, in a sentence or two.
        text: String,
    },
    /// Print the recorded task.
    Show,
    /// Forget the recorded task.
    Clear,
    /// Declare the task BLOCKED when what stops it is outside your control. The main agent's next
    /// stop asks the watchdog once whether it is, then lets the stop through with this handoff.
    Blocked {
        /// What is missing and what was tried.
        text: String,
    },
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Check formatting, run clippy, the tests, the doctests and rustdoc: the gate stop hooks and
    /// CI share.
    Check {
        /// Check only the crates affected by changes since the merge base with the default branch
        /// (upstream/HEAD, else origin/HEAD), including uncommitted and untracked files.
        #[arg(long)]
        changed: bool,
        /// When another gate holds the gate lock, exit at once with status 75 (`EX_TEMPFAIL`),
        /// naming it, instead of waiting for it; the stop hook runs check this way.
        #[arg(long)]
        no_wait: bool,
    },
    /// Run the tests under coverage and require 90% of the changed, instrumented lines covered.
    Coverage {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
    },
    /// Run cargo-mutants on the Rust lines changed since the merge base and fail on missed mutants.
    Mutants {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
        /// How many mutants to build and test at once.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
        jobs: u32,
        /// Test only this share of the mutants, as `k/n` with k from 0 to n - 1; CI splits the
        /// run into shards this way.
        #[arg(long, value_parser = mutants::Shard::parse)]
        shard: Option<mutants::Shard>,
    },
    /// Lint the fuzz workspace (`fuzz/`), then fuzz protocol message decoding for a while with
    /// the pinned cargo-fuzz; fails when a lint fails or the fuzzer finds an input that breaks it.
    Fuzz {
        /// How long to fuzz, in seconds.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
        seconds: u64,
    },
    /// Report deleted, skipped or weakened tests and changed gate files since the merge base.
    Integrity {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
        /// In CI, the pull request whose `integrity-approved` label can accept the findings. Its
        /// description is read from the event payload at `GITHUB_EVENT_PATH`.
        #[arg(long)]
        pr: Option<u64>,
        /// A file holding the pull request description, whose "## Test changes" section must give
        /// a reason for each flagged test change; read instead of the event payload.
        #[arg(long)]
        description: Option<PathBuf>,
    },
    /// git's credential helper: answers `get` for github.com with an agent App token on stdout.
    GitCredential {
        /// The operation git asks for: get, store or erase.
        operation: String,
    },
    /// Run gh as the agent App, with a fresh installation token from the broker in `GH_TOKEN`.
    Gh {
        /// Arguments passed to gh.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Pull requests as the agent App: open or update one after the checks integrity, the review
    /// and the rulesets would make, or list what fails on one. One App token per command.
    Pr {
        #[command(subcommand)]
        action: pr::Action,
    },
    /// Serve agent App tokens to one agent VM (D3): the one process that reads the App's key.
    /// `cargo xtask vm start` starts it, one per session, from a build of main installed outside
    /// any checkout: `cargo install --locked --git https://github.com/getabc-dev/abc --branch main
    /// xtask --root ~/.local/share/abc/broker`. It refuses to run from inside a checkout, a guard
    /// against mistakes, and listens on the session's sockets in `~/.config/abc/agent-broker/`.
    Broker {
        /// The session's name, such as abc-my-change.
        #[arg(long)]
        session: String,
    },
    /// Run agent sessions in microVMs, one per worktree (D37). Run it from the main clone.
    Vm {
        #[command(subcommand)]
        action: vm::Action,
    },
    /// Have Gemini review the changes since the merge base against AGENTS.md (D8).
    Review {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
    },
    /// Record, show or clear the task this worktree's agent is working on.
    #[command(name = "task")]
    Work {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Check or apply the branch rules for main in .github/rulesets/main.json.
    Rulesets {
        #[command(subcommand)]
        action: RulesetAction,
    },
    /// Build the abc binary with the web app embedded: web/ with the pinned pnpm, then
    /// `cargo build --release -p abc`, into target/release/abc.
    Build,
    /// Run the pinned pnpm in web/, with the pinned node first on PATH, such as
    /// `cargo xtask pnpm install` or `cargo xtask pnpm run dev`.
    Pnpm {
        /// Arguments passed to pnpm.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run the end-to-end tests: build abc, serve it on a fresh Postgres database behind a
    /// stand-in for the Tailscale sidecar, and load the web app in Chromium with Playwright.
    E2e,
    /// Install the tools pinned in xtask/tools.toml into .tools/, verifying each download.
    Setup {
        /// Install the tools the tests run (`for-tests` in xtask/tools.toml), with any named.
        #[arg(long)]
        for_tests: bool,
        /// Install only these tools; all of them when none are named and not --for-tests.
        tools: Vec<String>,
    },
    /// Check that every pinned tool, or each one named, is installed at its pinned version.
    Doctor {
        /// Check the tools the tests run (`for-tests` in xtask/tools.toml), with any named.
        #[arg(long)]
        for_tests: bool,
        /// Check only these tools; all of them when none are named and not --for-tests.
        tools: Vec<String>,
    },
    /// Record the sha256 of every pinned tool's archive for every target in xtask/tools.toml.
    Pin,
    /// Run a pinned tool from .tools/ in the repository root.
    Tool {
        /// The tool's name in xtask/tools.toml.
        name: String,
        /// Arguments passed to the tool.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run a Claude Code hook on the hook input from stdin, in the checkout the agent is in. It
    /// answers in Claude Code's hook JSON on stdout and exits 0; xtask/hooks/hook.sh runs it.
    Hook {
        /// The hook event.
        #[arg(value_enum)]
        event: hook::Event,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = u8::try_from(error.exit_code()).unwrap_or(1);
            return match error.print() {
                // With the terminal gone there is nowhere left to report; the code still tells.
                Ok(()) | Err(_) => ExitCode::from(code),
            };
        }
    };
    let invocation = match Invocation::new(cli.root) {
        Ok(invocation) => invocation,
        Err(error) => return report_error(&error),
    };
    // Not a held lock: the broker's threads write to stderr too, and would wait on it forever.
    match run(cli.command, &invocation, &mut io::stderr()) {
        Ok(code) => code,
        Err(error) => match writeln!(io::stderr().lock(), "error: {error:#}") {
            // With stderr gone there is nowhere left to report; the exit code still fails.
            Ok(()) | Err(_) => ExitCode::FAILURE,
        },
    }
}

fn report_error(error: &anyhow::Error) -> ExitCode {
    match writeln!(io::stderr().lock(), "error: {error:#}") {
        // With stderr gone there is nowhere left to report; the exit code still fails.
        Ok(()) | Err(_) => ExitCode::FAILURE,
    }
}

/// Where a command runs and what it can reach.
struct Invocation<'a> {
    /// The repository xtask was built from, whose pinned tools it runs.
    home: PathBuf,
    /// The tree a gate checks: `home`, or the tree given with `--root`.
    root: PathBuf,
    /// A pinned tool's executable, by name.
    tool: &'a dyn Fn(&str) -> anyhow::Result<PathBuf>,
    /// An environment variable.
    env: &'a dyn Fn(&str) -> Option<OsString>,
}

impl Invocation<'static> {
    fn new(root: Option<PathBuf>) -> anyhow::Result<Self> {
        let home = repo_root()?;
        let root = match root {
            Some(path) => std::fs::canonicalize(&path)
                .with_context(|| format!("--root {} does not exist", path.display()))?,
            None => home.clone(),
        };
        Ok(Self {
            home,
            root,
            tool: &pinned_tool,
            env: &env_var,
        })
    }
}

/// A tool pinned in the repository xtask was built from.
fn pinned_tool(name: &str) -> anyhow::Result<PathBuf> {
    tools::pinned(&repo_root()?, name)
}

fn env_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// Success when a gate passed, failure otherwise.
fn verdict(passed: bool) -> ExitCode {
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Runs `task`; a heavy gate holds the gate lock while it runs, so gates run one at a time.
fn run(
    task: Task,
    invocation: &Invocation<'_>,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let Some((gate, wait)) = gate_of(&task) else {
        return perform(task, invocation, report);
    };
    match gate::take(&invocation.root, gate, wait, report)? {
        gate::Taken::Held(held) => {
            let result = perform(task, invocation, report);
            drop(held);
            result
        }
        gate::Taken::Busy(holder) => {
            writeln!(
                report,
                "{}: not started: {holder} holds the gate lock ({})",
                gate.name(),
                gate::LOCK
            )?;
            Ok(ExitCode::from(gate::BUSY_STATUS))
        }
    }
}

fn perform(
    task: Task,
    invocation: &Invocation<'_>,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let Invocation {
        home, root, tool, ..
    } = invocation;
    let env = |name: &str| (invocation.env)(name);
    let mode = annotation::Mode::from_env(&env);
    match task {
        Task::Check { changed, .. } => Ok(verdict(check(root, changed, tool, report)?)),
        Task::Coverage { base } => Ok(verdict(coverage::run(
            root,
            &base,
            &coverage::Tools {
                llvm_cov: tool("cargo-llvm-cov")?,
                nextest: tool("cargo-nextest")?,
                web: web::Toolchain::pinned(tool)?,
            },
            mode,
            report,
        )?)),
        Task::Mutants { base, jobs, shard } => Ok(verdict(mutants::run(
            root,
            &base,
            &mutants::Options { jobs, shard, mode },
            &tool("cargo-mutants")?,
            &tool("cargo-nextest")?,
            &web::Toolchain::pinned(tool)?,
            report,
        )?)),
        Task::Fuzz { seconds } => Ok(verdict(fuzz::run(
            root,
            &check::cargo(),
            &tool("cargo-fuzz")?,
            seconds,
            report,
        )?)),
        Task::Integrity {
            base,
            pr,
            description,
        } => Ok(verdict(integrity(
            root,
            &base,
            pr,
            description.as_deref(),
            &env,
            mode,
            report,
        )?)),
        Task::Review { base } => Ok(verdict(review::run_from(
            root,
            &base,
            &env,
            report,
            &mut review::retrying(
                review::post_gemini,
                std::thread::sleep,
                &review::RETRY_DELAYS,
                io::stderr(),
            ),
        )?)),
        Task::GitCredential { operation } => {
            git_credential(&operation)?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Broker { session } => {
            serve_tokens(&std::env::current_exe()?, &user_home(&env)?, &session)
        }
        Task::Vm { action } => run_vm(action, home, &env, report),
        Task::Gh { args } => app::run_gh(
            || broker::token(app::Scope::Gh),
            app::GITHUB_API,
            Path::new("gh"),
            &args,
            report,
        ),
        Task::Pr { action } => run_pr(action, root, &env, report),
        Task::Rulesets { action } => run_rulesets(action, root, report),
        Task::Work { action } => run_task(action, root, report),
        Task::Setup {
            for_tests,
            tools: only,
        } => {
            tools::setup(home, &only, for_tests, &mut io::stdout().lock())?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Doctor {
            for_tests,
            tools: only,
        } => tools::doctor(home, &only, for_tests, &mut io::stdout().lock()),
        Task::Pin => {
            tools::pin(home, &mut io::stdout().lock())?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Tool { name, args } => tools::run(&tool(&name)?, root, &args),
        Task::Build => run_web(WebTask::Build, root, tool, report),
        Task::Pnpm { args } => run_web(WebTask::Pnpm(args), root, tool, report),
        Task::E2e => run_web(WebTask::E2e, root, tool, report),
        Task::Hook { event } => run_hook(event, home, report),
    }
}

/// The gate lock `task` takes, if it is a heavy gate, and whether it waits for another gate.
const fn gate_of(task: &Task) -> Option<(gate::Gate, gate::Wait)> {
    match task {
        Task::Check { no_wait, .. } => Some((
            gate::Gate::Check,
            if *no_wait {
                gate::Wait::No
            } else {
                gate::Wait::Yes
            },
        )),
        Task::Coverage { .. } => Some((gate::Gate::Coverage, gate::Wait::Yes)),
        Task::Mutants { .. } => Some((gate::Gate::Mutants, gate::Wait::Yes)),
        Task::Fuzz { .. } => Some((gate::Gate::Fuzz, gate::Wait::Yes)),
        Task::E2e => Some((gate::Gate::E2e, gate::Wait::Yes)),
        Task::Integrity { .. }
        | Task::GitCredential { .. }
        | Task::Gh { .. }
        | Task::Pr { .. }
        | Task::Broker { .. }
        | Task::Vm { .. }
        | Task::Review { .. }
        | Task::Work { .. }
        | Task::Rulesets { .. }
        | Task::Build
        | Task::Pnpm { .. }
        | Task::Setup { .. }
        | Task::Doctor { .. }
        | Task::Pin
        | Task::Tool { .. }
        | Task::Hook { .. } => None,
    }
}

/// `cargo xtask hook EVENT`: reads the hook input on stdin and prints the answer, in JSON, on
/// stdout. A stop whose check failed with no state to count it in prints its reason and exits
/// [`hook::UNKEPT_STATUS`], for the launcher to count.
/// The commands that run the web app's toolchain.
enum WebTask {
    Build,
    Pnpm(Vec<OsString>),
    E2e,
}

fn run_web(
    task: WebTask,
    root: &Path,
    tool: &dyn Fn(&str) -> anyhow::Result<PathBuf>,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let toolchain = web::Toolchain::pinned(tool)?;
    match task {
        WebTask::Build => Ok(verdict(web::build_binary(root, &toolchain, report)?)),
        WebTask::Pnpm(args) => {
            anyhow::ensure!(web::present(root), "{}", web::MISSING);
            let status = toolchain
                .pnpm(root)?
                .args(&args)
                .status()
                .context("could not run pnpm")?;
            Ok(ExitCode::from(tools::status_code(status)))
        }
        WebTask::E2e => Ok(verdict(e2e::run(
            root,
            &toolchain,
            &tool("cargo-sqlx")?,
            report,
        )?)),
    }
}

fn run_hook(event: hook::Event, home: &Path, report: &mut impl Write) -> anyhow::Result<ExitCode> {
    let mut input = String::new();
    let mut log = Vec::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        // Input that cannot be read is answered as unreadable input.
        writeln!(log, "xtask hook: could not read the hook input: {error}")?;
        input.clear();
    }
    let answer = hook::run(
        event,
        &input,
        &hook::Context::from_env(home.to_path_buf()),
        &mut log,
    );
    report.write_all(&log)?;
    if let hook::Answer::Held(reason) = &answer {
        writeln!(io::stdout().lock(), "{reason}")?;
        return Ok(ExitCode::from(hook::UNKEPT_STATUS));
    }
    if let Some(json) = answer.json(event) {
        writeln!(io::stdout().lock(), "{json}")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask check` on `root`, with the pinned tools `tool` finds and the warm Postgres.
fn check(
    root: &Path,
    changed: bool,
    tool: &dyn Fn(&str) -> anyhow::Result<PathBuf>,
    report: &mut impl Write,
) -> anyhow::Result<bool> {
    let postgres = check::Postgres {
        warm: &postgres::warm,
        sqlx: &|| tool("cargo-sqlx"),
    };
    let tools = check::Tools {
        nextest: tool("cargo-nextest")?,
        shellcheck: tool("shellcheck")?,
        web: Some(web::Toolchain::pinned(tool)?),
    };
    check::run(
        root,
        check::REPOSITORY,
        changed,
        &tools,
        &postgres,
        None,
        report,
    )
}

/// `cargo xtask integrity`: in CI, for pull request `pr`, with GitHub Actions' variables from
/// `env`; the description from the event payload or the `description` file.
fn integrity(
    root: &Path,
    base: &str,
    pr: Option<u64>,
    description: Option<&Path>,
    env: &impl Fn(&str) -> Option<OsString>,
    mode: annotation::Mode,
    report: &mut impl Write,
) -> anyhow::Result<bool> {
    let text = |name: &str| env(name).and_then(|value| value.into_string().ok());
    let pull_request = pr
        .map(|number| integrity::PullRequest::from_lookup(number, text))
        .transpose()?;
    let payload = pr.and_then(|_| env("GITHUB_EVENT_PATH").map(PathBuf::from));
    let event = integrity::event(description, payload)?;
    integrity::run(root, base, pull_request.as_ref(), &event, mode, report)
}

/// `cargo xtask pr`: checks `action` in `root`, then carries it out as the agent App with one
/// token from the broker `env` names; what it says goes to stdout.
fn run_pr(
    action: pr::Action,
    root: &Path,
    env: &impl Fn(&str) -> Option<OsString>,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    let mut said = String::new();
    let prepared = pr::prepare(action, root, &mut said);
    write!(io::stdout().lock(), "{said}")?;
    let Some(plan) = prepared? else {
        return Ok(ExitCode::SUCCESS);
    };
    let repo = github::Repo::of(root)?;
    let mut done = String::new();
    let result = github::with_token(
        || broker::token_from(env, app::Scope::Gh),
        |token| app::revoke(app::GITHUB_API, token),
        report,
        |token| {
            pr::send(
                plan,
                root,
                &repo,
                &mut github::http(app::GITHUB_API, token),
                &mut done,
            )
        },
    );
    write!(io::stdout().lock(), "{done}")?;
    Ok(verdict(result?))
}

/// `cargo xtask rulesets`: checks the rulesets in `root` against CI, or applies them with the
/// user's own gh.
fn run_rulesets(
    action: RulesetAction,
    root: &Path,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    match action {
        RulesetAction::Check => {
            rulesets::check(root)?;
            writeln!(report, "rulesets: {} matches CI", rulesets::RULESET)?;
        }
        RulesetAction::Apply => {
            rulesets::apply(root, Path::new("gh"), &mut io::stdout().lock())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask task`: records, prints or forgets the task in `root`.
fn run_task(action: TaskAction, root: &Path, report: &mut impl Write) -> anyhow::Result<ExitCode> {
    match action {
        TaskAction::Set { text } => task::set(root, &text, report)?,
        TaskAction::Show => match task::read(root)? {
            Some(text) => writeln!(io::stdout().lock(), "{text}")?,
            None => writeln!(report, "no task recorded")?,
        },
        TaskAction::Clear => task::clear(root)?,
        TaskAction::Blocked { text } => task::block(root, &text, report)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask git-credential`: answers git's request on stdin with an App token on stdout.
fn git_credential(operation: &str) -> anyhow::Result<()> {
    let mut request = String::new();
    io::stdin()
        .read_to_string(&mut request)
        .context("could not read git's credential request")?;
    app::credential(operation, &request, &mut io::stdout().lock(), || {
        broker::token(app::Scope::Git)
    })
}

/// The user's home directory.
fn user_home(env: &impl Fn(&str) -> Option<OsString>) -> anyhow::Result<PathBuf> {
    env("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `cargo xtask broker`, run from `exe`: serves session `session` tokens signed with the App's
/// key on its sockets under `home`, refusing to run from a checkout, until it is stopped.
fn serve_tokens(exe: &Path, home: &Path, session: &str) -> anyhow::Result<ExitCode> {
    broker::refuse_checkout(exe)?;
    broker::check_session(session)?;
    let app = app::App::load(&app::config_dir()?)?;
    let files = broker::files(home, session);
    broker::serve(
        session,
        &files,
        |scope| app.token(app::GITHUB_API, scope),
        |token| app::revoke(app::GITHUB_API, token),
        broker::MAX_PER_HOUR,
        &mut io::stderr(),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// `cargo xtask vm`: the billing check inside a VM, or a session command on the host, from the
/// main clone `repo`.
fn run_vm(
    action: vm::Action,
    repo: &Path,
    env: &impl Fn(&str) -> Option<OsString>,
    report: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    if matches!(action, vm::Action::Billing) {
        vm::billing(env)?;
        return Ok(ExitCode::SUCCESS);
    }
    vm::refuse_linked_worktree(repo)?;
    let home = user_home(env)?;
    let host = vm::Host::this(repo, &home, env)?;
    vm::require_kvm(host.os, Path::new("/dev/kvm"))?;
    let msb = vm::microsandbox(tools::pinned(repo, "msb")?, &home, env)?;
    vm::run(action, &host, &msb, env, report)
}

/// The repository root: the workspace that contains xtask.
fn repo_root() -> anyhow::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask is not inside the repository")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::CommandFactory;

    use clap::Parser as _;

    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;

    use super::{
        Cli, Invocation, RulesetAction, Task, TaskAction, repo_root, run, serve_tokens, user_home,
        verdict,
    };
    use crate::testing::{git, repository_with_change, script};

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// Fake pinned tools in `dir`: cargo-llvm-cov reports every changed line uncovered and
    /// cargo-mutants reports a missed mutant.
    fn fake_tools(dir: &Path) {
        script(
            &dir.join("cargo-llvm-cov"),
            "while [ $# -gt 0 ]; do [ \"$1\" = --output-path ] && out=$2; shift; done\nprintf 'SF:%s/src/a.rs\\nDA:1,0\\nend_of_record\\n' \"$PWD\" > \"$out\"\n",
        );
        script(
            &dir.join("cargo-mutants"),
            "while [ $# -gt 0 ]; do [ \"$1\" = --output ] && out=$2; shift; done\nmkdir -p \"$out/mutants.out\"\necho 'src/a.rs:1:1: replace a with ()' > \"$out/mutants.out/missed.txt\"\nexit 2\n",
        );
        script(&dir.join("cargo-nextest"), "exit 0\n");
        script(&dir.join("shellcheck"), "exit 0\n");
        script(&dir.join("cargo-fuzz"), "exit 77\n");
        // The fixtures hold no web app, so node and pnpm are resolved but never run.
        script(&dir.join("node"), "exit 9\n");
        script(&dir.join("pnpm"), "exit 9\n");
    }

    fn dispatch(
        task: Task,
        home: &Path,
        root: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<ExitCode> {
        let tools = home.join("tools");
        let tool = move |name: &str| -> anyhow::Result<PathBuf> {
            let path = tools.join(name);
            anyhow::ensure!(path.exists(), "{name} is not a fake tool");
            Ok(path)
        };
        let invocation = Invocation {
            home: home.to_path_buf(),
            root: root.to_path_buf(),
            tool: &tool,
            env,
        };
        run(task, &invocation, &mut Vec::new())
    }

    #[test]
    fn the_web_commands_run_the_pinned_pnpm() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("tools")).unwrap();
        fake_tools(&home.join("tools"));
        let root = crate::testing::repository(&temp.path().join("root"));
        let no_env = |_: &str| None;
        let pnpm = || Task::Pnpm {
            args: vec!["--version".into()],
        };
        let missing = dispatch(pnpm(), &home, &root, &no_env)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("no web/package.json"), "{missing}");
        fs::create_dir(root.join("web")).unwrap();
        fs::write(root.join("web/package.json"), "{}\n").unwrap();
        assert_eq!(
            dispatch(pnpm(), &home, &root, &no_env).unwrap(),
            ExitCode::from(9),
            "pnpm's exit code comes back"
        );
        assert_eq!(
            dispatch(Task::Build, &home, &root, &no_env).unwrap(),
            ExitCode::FAILURE,
            "the fake pnpm fails the install, so no binary is built"
        );
        let error = dispatch(Task::E2e, &home, &root, &no_env)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cargo-sqlx is not a fake tool"), "{error}");
    }

    #[test]
    fn vm_commands_run_from_the_main_clone_and_the_billing_check_in_the_vm() {
        let temp = tempfile::tempdir().unwrap();
        let main = crate::testing::repository(&temp.path().join("main"));
        let linked = temp.path().join("linked");
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
        let vm = |action| Task::Vm { action };
        let key = |name: &str| (name == "ANTHROPIC_API_KEY").then(|| OsString::from("k"));
        let billing = format!(
            "{:#}",
            dispatch(vm(crate::vm::Action::Billing), &linked, &linked, &key).unwrap_err()
        );
        assert!(
            billing.contains("ANTHROPIC_API_KEY in the environment"),
            "{billing}"
        );
        let clean = |_: &str| None;
        assert_eq!(
            dispatch(vm(crate::vm::Action::Billing), &linked, &linked, &clean).unwrap(),
            ExitCode::SUCCESS,
            "the billing check runs anywhere"
        );
        let linked_error = format!(
            "{:#}",
            dispatch(vm(crate::vm::Action::Ls), &linked, &linked, &clean).unwrap_err()
        );
        assert!(
            linked_error.contains("run it from the main clone"),
            "{linked_error}"
        );
        let no_home = format!(
            "{:#}",
            dispatch(vm(crate::vm::Action::Ls), &main, &main, &clean).unwrap_err()
        );
        assert_eq!(no_home, "HOME is not set");
    }

    #[test]
    fn finds_the_users_home() {
        let set = |name: &str| (name == "HOME").then(|| OsString::from("/home/m"));
        assert_eq!(user_home(&set).unwrap(), Path::new("/home/m"));
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(
            format!("{:#}", user_home(&empty).unwrap_err()),
            "HOME is not set"
        );
    }

    #[test]
    fn the_broker_refuses_a_build_in_a_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = crate::testing::repository(&temp.path().join("abc"));
        let error = format!(
            "{:#}",
            serve_tokens(&checkout.join("target/debug/xtask"), temp.path(), "abc-w").unwrap_err()
        );
        assert!(error.contains("installed outside any checkout"), "{error}");
        let named = format!(
            "{:#}",
            serve_tokens(&temp.path().join("bin/xtask"), temp.path(), "../x").unwrap_err()
        );
        assert!(named.contains("is not a session's name"), "{named}");
    }

    #[test]
    fn pr_checks_locally_first_and_asks_the_broker_for_one_token() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = crate::testing::clone_with_origin(temp.path());
        let no_env = |_: &str| None;
        let pr = |action| Task::Pr { action };
        assert_eq!(
            dispatch(
                pr(crate::pr::Action::TestChanges),
                &checkout,
                &checkout,
                &no_env
            )
            .unwrap(),
            ExitCode::SUCCESS,
            "the section needs no token"
        );
        let unread = format!(
            "{:#}",
            dispatch(
                pr(crate::pr::Action::Open {
                    title: "t".to_owned(),
                    body: temp.path().join("missing.md"),
                }),
                &checkout,
                &checkout,
                &no_env
            )
            .unwrap_err()
        );
        assert!(
            unread.starts_with("could not read the description"),
            "{unread}"
        );
        let checks = || pr(crate::pr::Action::Checks { number: 7 });
        let local = format!(
            "{:#}",
            dispatch(checks(), &checkout, &checkout, &no_env).unwrap_err()
        );
        assert!(
            local.starts_with("origin is not a github.com repository"),
            "{local}"
        );
        git(
            &checkout,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );
        let no_broker = format!(
            "{:#}",
            dispatch(checks(), &checkout, &checkout, &no_env).unwrap_err()
        );
        assert!(
            no_broker.contains("is not set: agent App tokens come from the token broker"),
            "the broker comes from the invocation's environment: {no_broker}"
        );
    }

    #[test]
    fn each_gate_routes_to_its_gate_and_fails_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("tools")).unwrap();
        fake_tools(&home.join("tools"));
        let root = repository_with_change(
            &temp.path().join("root"),
            &[("src/a.rs", "fn a() {}\n"), ("deny.toml", "\n")],
        );
        let no_env = |_: &str| None;
        let base = || "base".to_owned();
        for (gate, task) in [
            (
                "integrity",
                Task::Integrity {
                    base: base(),
                    pr: None,
                    description: None,
                },
            ),
            ("coverage", Task::Coverage { base: base() }),
            ("fuzz", Task::Fuzz { seconds: 1 }),
            (
                "mutants",
                Task::Mutants {
                    base: base(),
                    jobs: 1,
                    shard: None,
                },
            ),
            (
                "check",
                Task::Check {
                    changed: false,
                    no_wait: false,
                },
            ),
        ] {
            assert_eq!(
                dispatch(task, &home, &root, &no_env).unwrap(),
                ExitCode::FAILURE,
                "{gate} fails the tree at --root"
            );
        }
        assert!(
            dispatch(Task::Review { base: base() }, &home, &root, &no_env).is_err(),
            "review fails closed without a key"
        );
        let merge_queue = |name: &str| (name == "GITHUB_EVENT_NAME").then(|| "merge_group".into());
        assert_eq!(
            dispatch(Task::Review { base: base() }, &home, &root, &merge_queue).unwrap(),
            ExitCode::SUCCESS,
            "review passes the merge queue, reading the event from the environment"
        );
        assert!(
            dispatch(
                Task::Rulesets {
                    action: RulesetAction::Check
                },
                &home,
                &root,
                &no_env
            )
            .is_err(),
            "rulesets check reads the tree at --root, which has none"
        );
        assert_eq!(
            dispatch(
                Task::Tool {
                    name: "cargo-mutants".to_owned(),
                    args: vec!["--output".into(), "target/t".into()],
                },
                &home,
                &root,
                &no_env
            )
            .unwrap(),
            ExitCode::from(2),
            "a tool's exit code comes back"
        );
        assert!(
            root.join("target/t/mutants.out/missed.txt").is_file(),
            "it ran in --root"
        );
        assert!(
            dispatch(
                Task::Integrity {
                    base: base(),
                    pr: Some(7),
                    description: None,
                },
                &home,
                &root,
                &no_env
            )
            .is_err(),
            "a pull request run needs GitHub Actions' variables"
        );
    }

    #[test]
    fn a_clean_tree_passes_its_gates() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("tools")).unwrap();
        fake_tools(&home.join("tools"));
        let root = repository_with_change(&temp.path().join("root"), &[("README.md", "x\n")]);
        let no_env = |_: &str| None;
        for task in [
            Task::Integrity {
                base: "base".to_owned(),
                pr: None,
                description: None,
            },
            Task::Coverage {
                base: "base".to_owned(),
            },
            Task::Mutants {
                base: "base".to_owned(),
                jobs: 1,
                shard: None,
            },
        ] {
            assert_eq!(
                dispatch(task, &home, &root, &no_env).unwrap(),
                ExitCode::SUCCESS,
                "nothing to judge"
            );
        }
        assert_eq!(
            dispatch(
                Task::Rulesets {
                    action: RulesetAction::Check
                },
                &repo_root().unwrap(),
                &repo_root().unwrap(),
                &no_env
            )
            .unwrap(),
            ExitCode::SUCCESS,
            "the checked-in ruleset is in step with CI"
        );
        assert_eq!(verdict(true), ExitCode::SUCCESS, "a passing gate");
        assert_eq!(verdict(false), ExitCode::FAILURE, "a failing gate");
    }

    #[test]
    fn check_passes_a_clean_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("tools")).unwrap();
        fake_tools(&home.join("tools"));
        let root = temp.path().join("root");
        // Both workspaces of a repository, each one crate, and its scripts.
        for (dir, name) in [
            ("", "fixture-dispatch"),
            ("xtask", "fixture-dispatch-xtask"),
        ] {
            let crate_dir = root.join(dir);
            fs::create_dir_all(crate_dir.join("src")).unwrap();
            fs::write(
                crate_dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n"),
            )
            .unwrap();
            fs::write(
                crate_dir.join("Cargo.lock"),
                format!("version = 4\n\n[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            fs::write(
                crate_dir.join("src/lib.rs"),
                "//! A fixture crate.\n\n/// Adds one.\n#[must_use]\npub fn add_one(value: u32) -> u32 {\n    value.saturating_add(1)\n}\n",
            )
            .unwrap();
        }
        for script in crate::check::SCRIPTS {
            let path = root.join(script);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "#!/bin/sh\n").unwrap();
        }
        fs::copy(
            repo_root().unwrap().join("rust-toolchain.toml"),
            root.join("rust-toolchain.toml"),
        )
        .unwrap();
        assert_eq!(
            dispatch(
                Task::Check {
                    changed: false,
                    no_wait: false,
                },
                &home,
                &root,
                &|_: &str| { None }
            )
            .unwrap(),
            ExitCode::SUCCESS,
            "check passes a clean tree with the pinned nextest and shellcheck it finds"
        );
    }

    #[test]
    fn a_real_invocation_reads_this_checkout_and_the_environment() {
        let here = Invocation::new(None).unwrap();
        assert_eq!(
            here.root,
            repo_root().unwrap(),
            "the tree defaults to xtask's own"
        );
        assert_eq!(here.home, here.root, "and so do the tools");
        assert_eq!(
            (here.env)("PATH"),
            std::env::var_os("PATH"),
            "the process environment"
        );
        assert!(
            (here.tool)("no-such-tool").is_err(),
            "only pinned tools resolve"
        );
        let temp = tempfile::tempdir().unwrap();
        let elsewhere = Invocation::new(Some(temp.path().to_path_buf())).unwrap();
        assert_eq!(
            elsewhere.root,
            temp.path().canonicalize().unwrap(),
            "--root, canonical"
        );
        assert_eq!(
            elsewhere.home,
            repo_root().unwrap(),
            "the tools stay xtask's own"
        );
        assert!(
            Invocation::new(Some(temp.path().join("absent"))).is_err(),
            "a --root that does not exist"
        );
        assert_eq!(
            super::report_error(&anyhow::anyhow!("boom")),
            ExitCode::FAILURE,
            "an error fails the process"
        );
    }

    #[test]
    fn setup_installs_tools_and_never_touches_git_config() {
        let temp = tempfile::tempdir().unwrap();
        let home = repository_with_change(
            &temp.path().join("home"),
            &[("xtask/tools.toml", "tool = []\n")],
        );
        let config = git(&home, &["config", "--local", "--list"]);
        assert_eq!(
            dispatch(
                Task::Setup {
                    for_tests: false,
                    tools: Vec::new()
                },
                &home,
                &home,
                &|_| None
            )
            .unwrap(),
            ExitCode::SUCCESS,
            "nothing to install"
        );
        assert_eq!(
            git(&home, &["config", "--local", "--list"]),
            config,
            "agent sessions get the App's credentials from their environment, not git config"
        );
        assert!(
            dispatch(
                Task::Doctor {
                    for_tests: false,
                    tools: vec!["no-such-tool".to_owned()]
                },
                &home,
                &home,
                &|_| None
            )
            .is_err(),
            "a command's error comes back"
        );
    }

    #[test]
    fn records_the_task_in_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = repository_with_change(&temp.path().join("root"), &[("README.md", "x\n")]);
        dispatch(
            Task::Work {
                action: TaskAction::Set {
                    text: "Fix the gates".to_owned(),
                },
            },
            &root,
            &root,
            &|_| None,
        )
        .unwrap();
        assert_eq!(
            crate::task::read(&root).unwrap().as_deref(),
            Some("Fix the gates"),
            "in .abc/task.md"
        );
        dispatch(
            Task::Work {
                action: TaskAction::Clear,
            },
            &root,
            &root,
            &|_| None,
        )
        .unwrap();
        assert_eq!(crate::task::read(&root).unwrap(), None, "cleared");
    }

    #[test]
    fn a_hook_takes_only_its_event() {
        let args = ["xtask", "hook", "post-tool-use", "x"];
        let error = Cli::try_parse_from(args).unwrap_err();
        assert!(
            error.use_stderr(),
            "{args:?} is a usage error, reported on stderr and never as an answer"
        );
    }

    #[test]
    fn the_xtask_workspace_shares_the_nextest_configuration() {
        let root = repo_root().unwrap();
        assert_eq!(
            fs::read_link(root.join("xtask/.config")).unwrap(),
            Path::new("../.config"),
            "nextest reads .config/ from the workspace root; xtask's is the repository's"
        );
    }

    #[test]
    fn the_xtask_workspace_keeps_the_main_workspace_lints_and_rust_version() {
        let root = repo_root().unwrap();
        let read = |path: &str| -> serde_json::Value {
            toml_edit::de::from_str(&fs::read_to_string(root.join(path)).unwrap()).unwrap()
        };
        let main = read("Cargo.toml");
        let xtask = read("xtask/Cargo.toml");
        assert!(
            main["workspace"]["lints"].is_object(),
            "the main workspace sets lints"
        );
        assert_eq!(
            xtask["lints"], main["workspace"]["lints"],
            "xtask/Cargo.toml's [lints] must equal Cargo.toml's [workspace.lints]"
        );
        assert!(
            main["workspace"]["package"]["rust-version"].is_string(),
            "the main workspace sets rust-version"
        );
        assert_eq!(
            xtask["package"]["rust-version"], main["workspace"]["package"]["rust-version"],
            "xtask/Cargo.toml's rust-version must equal the main workspace's"
        );
        assert!(
            xtask["workspace"].is_object(),
            "xtask is a workspace of its own"
        );
        assert!(
            !main["workspace"]["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|member| member
                    .as_str()
                    .is_some_and(|member| member.starts_with("xtask"))),
            "and not a member of the main one"
        );
    }

    #[test]
    fn workspace_rust_version_is_the_pinned_toolchain() {
        let root = repo_root().unwrap();
        let toolchain: toml_edit::DocumentMut =
            fs::read_to_string(root.join("rust-toolchain.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let manifest: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            manifest["workspace"]["package"]["rust-version"].as_str(),
            toolchain["toolchain"]["channel"].as_str(),
            "Cargo.toml's rust-version must equal the channel in rust-toolchain.toml"
        );
    }
}
