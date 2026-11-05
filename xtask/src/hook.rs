//! Claude Code hooks: find the checkout the agent is working in, then act there.
//!
//! Claude Code starts every hook from the directory the session began in, but the agent may have
//! moved since, for example into a worktree. The hook input's `cwd` says where it is now; without
//! one, the session's project directory stands in. The hook resolves that directory's repository
//! root and then:
//!
//! - runs here when the root is this checkout;
//! - hands the input to that checkout's own `cargo xtask hook` when it is another worktree of this
//!   repository, so the checkout is held to its own toolchain, tools and gate;
//! - does nothing when the directory is in no repository, or in a different one.
//!
//! Claude Code cancels a hook at its `timeout` in `.claude/settings.json`: 300 seconds for
//! `PreToolUse` and 900 for `Stop`. A timed-out hook renders no decision, so the tool call runs or
//! the agent stops. The guard builds in `target/hook`, so it never waits on the agent's cargo lock.
//!
//! The `PreToolUse` guard catches accidents; it is not the enforcement. GitHub rulesets on `main`
//! (no force push, no deletion, required checks) and the abc-agent App's limited permissions are.
//!
//! The agent VM runs its installed xtask with `--standalone`: the guard judges by its rules alone
//! and the stop hook checks the session's project directory, without locating a checkout or
//! handing over to its xtask. Nothing of the checkout builds before either answers, so both work
//! when the checkout's workspace does not resolve (W19).
//!
//! When the `PreToolUse` guard cannot run, it blocks commands that mention git or gh and lets the
//! rest through, so a broken build never locks the agent out of the shell. When xtask cannot even
//! start, `xtask/hooks/pre-tool-use.sh` applies the same rule, and `xtask/hooks/stop.sh` honours
//! the BLOCKED declaration and the backstop's count itself.
//!
//! Claude Code shows nothing of a `Stop` hook that exits 0, so a stop the hook lets through with a
//! handoff prints `{"systemMessage": ...}` on stdout, which Claude Code shows the user; exit 2
//! keeps the agent working and gives it stderr.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;

use crate::{check, guard, task, tools, watchdog};

/// The `PreToolUse` hook script. A checkout without it predates these hooks, so its xtask cannot
/// take a hook over.
const HOOK_SCRIPT: &str = "xtask/hooks/pre-tool-use.sh";

/// The `Stop` and `SubagentStop` hook script, which keeps the BLOCKED declaration and the
/// backstop working when xtask cannot run.
#[cfg(test)]
const STOP_SCRIPT: &str = "xtask/hooks/stop.sh";

/// Marks a hook handed over by another checkout, which must not be handed on again.
const DELEGATED: &str = "XTASK_HOOK_DELEGATED";

/// The Claude Code hook events xtask handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Event {
    /// `PreToolUse` on Bash and Monitor: block force pushes and git or gh commands the guard
    /// cannot read.
    PreToolUse,
    /// `Stop` and `SubagentStop`: run `cargo xtask check --changed`, honour a BLOCKED
    /// declaration and, for the main agent, the watchdog.
    Stop,
}

impl Event {
    const fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre-tool-use",
            Self::Stop => "stop",
        }
    }
}

/// What a hook knows about where it runs.
#[derive(Debug)]
pub(crate) struct Context {
    /// The root of the checkout this xtask was built from.
    own_root: PathBuf,
    /// `$CLAUDE_PROJECT_DIR`: the directory the session started in.
    project_dir: Option<PathBuf>,
    /// Whether another checkout's xtask handed this hook over.
    delegated: bool,
    /// The cargo that runs another checkout's xtask.
    cargo: PathBuf,
    /// The claude that runs the stop hook's watchdog.
    claude: PathBuf,
    /// Whether this hook runs inside the watchdog's own claude session, where it must do nothing.
    inside_watchdog: bool,
    /// Whether the hook acts without locating a checkout: the guard on its rules alone, the stop
    /// hook on the project directory.
    standalone: bool,
}

impl Context {
    pub(crate) fn from_env(own_root: PathBuf, standalone: bool) -> Self {
        Self {
            standalone,
            own_root,
            project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
            delegated: std::env::var_os(DELEGATED).is_some(),
            cargo: PathBuf::from("cargo"),
            claude: PathBuf::from("claude"),
            inside_watchdog: std::env::var_os(watchdog::GUARD).is_some(),
        }
    }
}

/// The fields of the hook input this module reads.
#[derive(Deserialize)]
struct HookInput {
    cwd: Option<PathBuf>,
    /// `Stop` or `SubagentStop` for the stop hook.
    hook_event_name: Option<String>,
    /// The subagent that stops, for `SubagentStop`.
    agent_id: Option<String>,
    /// Whether the agent is already working on because a stop hook blocked it.
    #[serde(default)]
    stop_hook_active: bool,
}

impl HookInput {
    /// Whether this is the main agent's stop. Only it takes a BLOCKED declaration and counts in
    /// the main agent's count: a subagent's stop must not spend what the main agent declared.
    fn main_agent(&self) -> bool {
        self.hook_event_name.as_deref() == Some("Stop")
    }

    /// Whether the watchdog judges this stop's drift: the main agent's own, and not one a stop
    /// hook has already sent back to work, so the watchdog can never keep an agent going forever.
    fn watched(&self) -> bool {
        self.main_agent() && !self.stop_hook_active
    }
}

/// What the stop hook decides.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Let the agent stop.
    Allow,
    /// Let the agent stop, and show the user this handoff.
    Handoff(String),
    /// Keep the agent working; it reads why on stderr.
    Block,
}

impl Verdict {
    /// Prints the handoff for Claude Code and returns the hook's exit code.
    fn emit(self, out: &mut impl Write, user: &mut impl Write) -> anyhow::Result<u8> {
        match self {
            Self::Allow => Ok(0),
            Self::Handoff(message) => {
                writeln!(out, "{message}")?;
                writeln!(user, "{}", serde_json::json!({ "systemMessage": message }))?;
                Ok(0)
            }
            Self::Block => Ok(2),
        }
    }
}

/// Where the agent is working, relative to this checkout.
#[derive(Debug, PartialEq, Eq)]
enum Location {
    Here,
    /// Another worktree of this repository, by its root.
    Worktree(PathBuf),
    /// Outside this repository, for the given reason.
    Elsewhere(String),
}

/// Runs the hook for `event` on the hook input JSON and returns the exit code for Claude Code.
/// Messages for the agent go to `out`; the JSON Claude Code reads from a stop hook goes to `user`.
pub(crate) fn run(
    event: Event,
    input: &str,
    context: &Context,
    out: &mut impl Write,
    user: &mut impl Write,
) -> anyhow::Result<u8> {
    match event {
        Event::PreToolUse => match if context.standalone {
            guard(input, out)
        } else {
            act(event, input, context, out, &mut std::io::sink())
        } {
            Ok(code @ (0 | 2)) => Ok(code),
            Ok(code) => guard_unavailable(&format!("it exited {code}"), input, out),
            Err(error) => guard_unavailable(&format!("{error:#}"), input, out),
        },
        // The watchdog's own session stops without a check, so it never starts another watchdog.
        Event::Stop if context.inside_watchdog => Ok(0),
        // Whatever goes wrong, the stop is held and counted, never left to the script's guess.
        Event::Stop => match stop_event(input, context, out, user) {
            Ok(code) => Ok(code),
            Err(error) => failed_stop(&error, input, context, out, user),
        },
    }
}

/// The stop hook on the hook input, standalone on the project directory or in the checkout the
/// agent is in.
fn stop_event(
    input: &str,
    context: &Context,
    out: &mut impl Write,
    user: &mut impl Write,
) -> anyhow::Result<u8> {
    if !context.standalone {
        return act(Event::Stop, input, context, out, user);
    }
    let fields = stop_input(input)?;
    let Some(project_dir) = &context.project_dir else {
        bail!("the stop hook runs standalone on CLAUDE_PROJECT_DIR, which is not set");
    };
    let root = project_dir
        .canonicalize()
        .with_context(|| format!("could not resolve {}", project_dir.display()))?;
    stop_at(&root, &fields, context, out)?.emit(out, user)
}

/// A stop hook that failed with `error` keeps the agent working, and counts as a blocked stop so
/// the backstop still ends the run. Input that cannot be read counts as the main agent's stop.
fn failed_stop(
    error: &anyhow::Error,
    input: &str,
    context: &Context,
    out: &mut impl Write,
    user: &mut impl Write,
) -> anyhow::Result<u8> {
    writeln!(out, "xtask stop hook: it failed: {error:#}")?;
    let fields = stop_input(input).unwrap_or_else(|_| HookInput {
        cwd: None,
        hook_event_name: Some(String::from("Stop")),
        agent_id: None,
        stop_hook_active: false,
    });
    let root = if context.standalone {
        context.project_dir.clone()
    } else {
        match locate(
            fields.cwd.as_deref(),
            context.project_dir.as_deref(),
            &context.own_root,
        ) {
            Ok(Location::Worktree(root)) => Some(root),
            _ => None,
        }
    }
    .unwrap_or_else(|| context.own_root.clone());
    keep_working(&root, &fields)?.emit(out, user)
}

/// The stop hook's input.
fn stop_input(input: &str) -> anyhow::Result<HookInput> {
    serde_json::from_str(input).context("could not read the Stop hook input")
}

/// When the guard cannot run, blocks a command that mentions git or gh and lets any other through
/// with a warning.
fn guard_unavailable(reason: &str, input: &str, out: &mut impl Write) -> anyhow::Result<u8> {
    let command = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(fields) => fields
            .pointer("/tool_input/command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(_) => Some(String::from("git")),
    };
    if command.as_deref().is_some_and(guard::mentions_git_or_gh) {
        writeln!(
            out,
            "Blocked: the git guard could not run ({reason}), so git and gh commands are blocked \
             until it can. Commands that do not mention git or gh still run."
        )?;
        Ok(2)
    } else {
        writeln!(
            out,
            "warning: the git guard could not run ({reason}); allowing a command that does not \
             mention git or gh"
        )?;
        Ok(0)
    }
}

/// Finds where the agent works and runs the hook there.
fn act(
    event: Event,
    input: &str,
    context: &Context,
    out: &mut impl Write,
    user: &mut impl Write,
) -> anyhow::Result<u8> {
    let fields = match event {
        Event::Stop => stop_input(input)?,
        Event::PreToolUse => match serde_json::from_str::<HookInput>(input) {
            Ok(fields) => fields,
            // The guard blocks input it cannot read and gives the parse error as the reason.
            Err(_) => return guard(input, out),
        },
    };
    let location = locate(
        fields.cwd.as_deref(),
        context.project_dir.as_deref(),
        &context.own_root,
    )?;
    match location {
        Location::Here => match event {
            Event::PreToolUse => guard(input, out),
            Event::Stop => stop_at(&context.own_root, &fields, context, out)?.emit(out, user),
        },
        // GitHub is the gate, so a checkout that cannot take the hook is noted, never locked out.
        Location::Worktree(root) if !root.join(HOOK_SCRIPT).is_file() => {
            writeln!(
                out,
                "xtask hook: {} has no {HOOK_SCRIPT}; nothing is checked there",
                root.display()
            )?;
            Ok(0)
        }
        Location::Worktree(root) => {
            ensure!(
                !context.delegated,
                "{} was handed this hook, but the agent is in another checkout: {}",
                context.own_root.display(),
                root.display()
            );
            delegate(&context.cargo, &root, event, input)
        }
        Location::Elsewhere(reason) => {
            if event == Event::Stop {
                writeln!(out, "xtask hook: {reason}; no checks run")?;
            }
            Ok(0)
        }
    }
}

/// The stop hook in the checkout at `root`, with its pinned tools, the real check and the
/// watchdog's claude.
fn stop_at(
    root: &Path,
    fields: &HookInput,
    context: &Context,
    out: &mut impl Write,
) -> anyhow::Result<Verdict> {
    stop(
        root,
        fields,
        out,
        |out| {
            check::run(
                root,
                check::REPOSITORY,
                true,
                &check::Tools::pinned(root)?,
                &check::Postgres {
                    warm: &crate::postgres::warm,
                    sqlx: &|| tools::pinned(root, "cargo-sqlx"),
                },
                None,
                out,
            )
        },
        |declaration, out| {
            watchdog::blocker(root, &context.claude, watchdog::TIMEOUT, declaration, out)
        },
        |out| watchdog::check(root, &context.claude, watchdog::TIMEOUT, out),
    )
}

/// How many stops in a row the stop hook blocks before it lets one through anyway.
const MAX_BLOCKED_STOPS: u32 = 5;

/// The stop hook: PASS, FAIL or BLOCKED (D39), so a run that can never go green still ends.
///
/// Only the main agent's stop takes a BLOCKED declaration; the main agent counts its blocked
/// stops in one count and each subagent in its own, by `agent_id`, so a subagent never spends
/// what the main agent declared or counted.
///
/// - A BLOCKED declaration (`cargo xtask task blocked`) is appended to `.abc/blocked-log.md` and
///   taken, so it covers one stop. `blocker` asks the watchdog, for every declaration, whether
///   what blocks the agent is outside its control. When it is not, the stop is sent back with the
///   reason and counts as a blocked stop; otherwise, or once [`MAX_BLOCKED_STOPS`] blocked stops
///   are counted, the stop goes through with the handoff.
/// - Otherwise `check` decides; a `check` that cannot run fails. A pass resets the count and,
///   when the stop is watched, hands over to the drift watchdog; a failure keeps the agent
///   working ([`keep_working`]).
fn stop<W: Write>(
    root: &Path,
    fields: &HookInput,
    out: &mut W,
    check: impl FnOnce(&mut W) -> anyhow::Result<bool>,
    blocker: impl FnOnce(&str, &mut W) -> anyhow::Result<Option<String>>,
    watchdog: impl FnOnce(&mut W) -> anyhow::Result<u8>,
) -> anyhow::Result<Verdict> {
    let main = fields.main_agent();
    if main && let Some(declaration) = task::take_blocked(root)? {
        let count = task::blocked_stops(root)?;
        let sent_back = if count >= MAX_BLOCKED_STOPS {
            None
        } else {
            blocker(&declaration, out)?
        };
        if let Some(reason) = sent_back {
            task::log_blocked(root, &declaration, &format!("sent back: {reason}"))?;
            task::count_blocked_stop(root)?;
            writeln!(
                out,
                "xtask stop hook: the BLOCKED declaration was sent back: {reason}\nKeep working. \
                 If what stops you is truly outside your control, declare it again and say what \
                 makes it so."
            )?;
            return Ok(Verdict::Block);
        }
        let outcome = if count >= MAX_BLOCKED_STOPS {
            format!("stop allowed, not judged: {count} blocked stops in a row")
        } else {
            String::from("stop allowed")
        };
        task::log_blocked(root, &declaration, &outcome)?;
        task::reset_blocked_stops(root)?;
        return Ok(Verdict::Handoff(format!(
            "BLOCKED, stop allowed. Handoff: {declaration}"
        )));
    }
    let passed = match check(out) {
        Ok(passed) => passed,
        Err(error) => {
            writeln!(out, "xtask stop hook: the check could not run: {error:#}")?;
            false
        }
    };
    if passed {
        if main {
            task::reset_blocked_stops(root)?;
        } else {
            task::reset_subagent_stops(root, fields.agent_id.as_deref().unwrap_or_default())?;
        }
        let drifted = fields.watched() && watchdog(out)? != 0;
        return Ok(if drifted {
            Verdict::Block
        } else {
            Verdict::Allow
        });
    }
    keep_working(root, fields)
}

/// A stop that did not pass blocks, and counts. After [`MAX_BLOCKED_STOPS`] blocked stops in a
/// row, the main agent's or one subagent's, the stop goes through anyway, with a handoff that
/// says so; the main agent's is recorded in `.abc/stop-backstop.md`.
fn keep_working(root: &Path, fields: &HookInput) -> anyhow::Result<Verdict> {
    if fields.main_agent() {
        let count = task::blocked_stops(root)?;
        if count >= MAX_BLOCKED_STOPS {
            task::reset_blocked_stops(root)?;
            task::record_backstop(root, count)?;
            return Ok(Verdict::Handoff(format!(
                "The check failed and {count} stops in a row were blocked with no BLOCKED \
                 declaration, so this stop is allowed; recorded in {}. Next time declare it: \
                 cargo xtask task blocked \"<what is missing, what was tried>\"",
                task::BACKSTOP
            )));
        }
        task::count_blocked_stop(root)?;
        return Ok(Verdict::Block);
    }
    let agent = fields.agent_id.as_deref().unwrap_or_default();
    let count = task::subagent_stops(root, agent)?;
    if count >= MAX_BLOCKED_STOPS {
        task::reset_subagent_stops(root, agent)?;
        return Ok(Verdict::Handoff(format!(
            "A subagent's check failed and {count} of its stops in a row were blocked, so this \
             stop is allowed."
        )));
    }
    task::count_subagent_stop(root, agent)?;
    Ok(Verdict::Block)
}

fn guard(input: &str, out: &mut impl Write) -> anyhow::Result<u8> {
    match guard::decide(input) {
        guard::Decision::Allow => Ok(0),
        guard::Decision::Block(message) => {
            writeln!(out, "{message}")?;
            Ok(2)
        }
    }
}

/// Finds the checkout containing `cwd`, or the project directory when there is no `cwd`.
fn locate(
    cwd: Option<&Path>,
    project_dir: Option<&Path>,
    own_root: &Path,
) -> anyhow::Result<Location> {
    let Some(dir) = cwd.or(project_dir) else {
        bail!("the hook input has no cwd and CLAUDE_PROJECT_DIR is not set");
    };
    ensure!(
        dir.is_dir(),
        "the agent's working directory {} does not exist",
        dir.display()
    );
    let Some(root) = git_path(dir, &["rev-parse", "--show-toplevel"])? else {
        return Ok(Location::Elsewhere(format!(
            "{} is not in a git repository",
            dir.display()
        )));
    };
    let own_root = own_root
        .canonicalize()
        .with_context(|| format!("could not resolve {}", own_root.display()))?;
    if root == own_root {
        return Ok(Location::Here);
    }
    let common_dir = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    if git_path(&root, &common_dir)? == git_path(&own_root, &common_dir)? {
        Ok(Location::Worktree(root))
    } else {
        Ok(Location::Elsewhere(format!(
            "{} is not a checkout of this repository",
            root.display()
        )))
    }
}

/// Runs git in `dir` and resolves the path it prints. `None` when git refuses, as it does outside a
/// repository.
fn git_path(dir: &Path, args: &[&str]) -> anyhow::Result<Option<PathBuf>> {
    let output = git_in(dir)
        .args(args)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let printed = String::from_utf8(output.stdout).context("git printed a non-UTF-8 path")?;
    let path = Path::new(printed.trim_end_matches(['\n', '\r']));
    path.canonicalize()
        .map(Some)
        .with_context(|| format!("could not resolve {}", path.display()))
}

/// Git run in `dir`, resolving the repository from the directory alone, never from an inherited
/// `GIT_DIR` or `GIT_WORK_TREE`.
fn git_in(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    command
}

/// Hands the hook to the checkout at `root`, which runs it with its own xtask and toolchain.
fn delegate(cargo: &Path, root: &Path, event: Event, input: &str) -> anyhow::Result<u8> {
    let mut child = Command::new(cargo)
        .args(["xtask", "hook", event.name()])
        .current_dir(root)
        // Let the checkout's rust-toolchain.toml choose the toolchain, not the one running here.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env(DELEGATED, OsStr::new("1"))
        .envs(
            (event == Event::PreToolUse)
                .then(|| ("CARGO_TARGET_DIR", root.join("target/hook"))),
        )
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run cargo xtask in {}", root.display()))?;
    tools::feed(&mut child, &mut input.as_bytes()).context("could not pass the hook input on")?;
    let status = child
        .wait()
        .context("could not wait for the delegated hook")?;
    Ok(tools::status_code(status))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use tempfile::TempDir;

    use super::{Context, Event, Location, Verdict, git_in, guard_unavailable, locate, run};
    use crate::testing::{git, repository};

    /// A repository with a worktree nested inside it, where Claude Code puts its worktrees.
    fn repository_with_nested_worktree() -> (TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let main = repository(&temp.path().join("abc"));
        git(
            &main,
            &["worktree", "add", "--quiet", ".claude/worktrees/feature"],
        );
        let worktree = main.join(".claude/worktrees/feature");
        fs::create_dir_all(worktree.join("crates/deep")).unwrap();
        fs::create_dir_all(worktree.join("xtask/hooks")).unwrap();
        fs::write(worktree.join(super::HOOK_SCRIPT), "").unwrap();
        (temp, main, worktree.canonicalize().unwrap())
    }

    fn context(own_root: &Path, project_dir: Option<&Path>) -> Context {
        Context {
            own_root: own_root.to_path_buf(),
            project_dir: project_dir.map(Path::to_path_buf),
            delegated: false,
            cargo: PathBuf::from("cargo"),
            claude: PathBuf::from("claude"),
            inside_watchdog: false,
            standalone: false,
        }
    }

    /// A script at `dir/cargo` that drains its input, records `CARGO_TARGET_DIR` in
    /// `dir/target-dir`, and exits with `code`.
    fn fake_cargo(dir: &Path, code: u8) -> PathBuf {
        let path = dir.join("cargo");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf %s \"$CARGO_TARGET_DIR\" > \"$(dirname \"$0\")/target-dir\"\nexit {code}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run_pre_tool_use(context: &Context, cwd: Option<&Path>, command: &str) -> (u8, String) {
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            &input(cwd, command),
            context,
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    fn input(cwd: Option<&Path>, command: &str) -> String {
        let mut fields = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
        });
        if let Some(cwd) = cwd {
            fields["cwd"] = serde_json::Value::from(cwd.to_str().unwrap());
        }
        fields.to_string()
    }

    #[test]
    fn a_nested_worktree_is_located_by_its_own_root() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let deep = worktree.join("crates/deep");
        assert_eq!(
            locate(Some(&deep), Some(&main), &main).unwrap(),
            Location::Worktree(worktree.clone()),
            "from the main checkout, the worktree is another checkout of the same repository"
        );
        assert_eq!(
            locate(Some(&deep), Some(&main), &worktree).unwrap(),
            Location::Here,
            "the worktree's own xtask runs the hook itself"
        );
        assert_eq!(
            locate(Some(&main.join(".claude")), None, &main).unwrap(),
            Location::Here,
            "a directory of the main checkout outside the worktree belongs to the main checkout"
        );
    }

    #[test]
    fn an_absent_cwd_falls_back_to_the_project_directory() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        assert_eq!(
            locate(None, Some(&main), &main).unwrap(),
            Location::Here,
            "the project directory stands in for cwd"
        );
        assert_eq!(
            locate(None, Some(&worktree), &main).unwrap(),
            Location::Worktree(worktree.clone()),
            "a project directory in a worktree hands the hook over"
        );
        let error = locate(None, None, &main).unwrap_err().to_string();
        assert!(error.contains("CLAUDE_PROJECT_DIR"), "{error}");
    }

    #[test]
    fn a_cwd_takes_precedence_over_the_project_directory() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        assert_eq!(
            locate(Some(&worktree), Some(&main), &main).unwrap(),
            Location::Worktree(worktree),
            "the agent's cwd wins"
        );
    }

    #[test]
    fn the_guard_reads_the_project_directory_when_cwd_is_absent() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            &input(None, "git push --force"),
            &context(&main, Some(&main)),
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(code, 2, "a force push in the project directory is blocked");
        assert!(
            String::from_utf8(out).unwrap().contains("force-pushes"),
            "the agent is told why"
        );
    }

    #[test]
    fn outside_any_repository_the_guard_allows_without_checks() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let outside = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            &input(Some(outside.path()), "git push --force"),
            &context(&main, Some(&main)),
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(code, 0, "nothing is checked outside the repository");
        assert!(out.is_empty(), "and nothing is said");
    }

    #[test]
    fn outside_any_repository_stop_passes_with_a_note() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let outside = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = run(
            Event::Stop,
            &input(Some(outside.path()), ""),
            &context(&main, Some(&main)),
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();
        let note = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "the agent may stop");
        assert_eq!(note.lines().count(), 1, "one line: {note:?}");
        assert!(
            note.contains("is not in a git repository; no checks run"),
            "{note:?}"
        );
    }

    #[test]
    fn another_repository_is_not_checked() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let other = repository(&temp.path().join("other"));
        assert_eq!(
            locate(Some(&other), Some(&main), &main).unwrap(),
            Location::Elsewhere(format!(
                "{} is not a checkout of this repository",
                other.display()
            )),
            "another repository's xtask is never run"
        );
    }

    #[test]
    fn a_missing_cwd_blocks_git_and_allows_the_rest() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let gone = temp.path().join("removed-worktree");
        let context = context(&main, Some(&main));

        let (push_code, push_out) = run_pre_tool_use(&context, Some(&gone), "git push origin main");
        assert_eq!(
            push_code, 2,
            "git is blocked when the guard cannot locate the agent: {push_out}"
        );
        assert!(
            push_out.contains("does not exist"),
            "the reason is given: {push_out}"
        );

        let (listing_code, listing_out) = run_pre_tool_use(&context, Some(&gone), "ls -la");
        assert_eq!(listing_code, 0, "other commands still run: {listing_out}");
        assert!(
            listing_out.starts_with("warning: "),
            "with a warning: {listing_out}"
        );

        let mut said = Vec::new();
        let code = run(
            Event::Stop,
            &input(Some(&gone), ""),
            &context,
            &mut said,
            &mut std::io::sink(),
        )
        .unwrap();
        let said = String::from_utf8(said).unwrap();
        assert_eq!(code, 2, "stop is held: {said}");
        assert!(said.contains("does not exist"), "and says why: {said}");
    }

    #[test]
    fn a_worktree_without_xtask_hooks_is_noted_not_checked() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        fs::remove_file(worktree.join(super::HOOK_SCRIPT)).unwrap();
        let context = context(&main, Some(&main));
        let (code, out) = run_pre_tool_use(&context, Some(&worktree), "git push -f");
        assert_eq!(code, 0, "an older checkout is not locked out: {out}");
        assert_eq!(out.lines().count(), 1, "one line: {out}");
        assert!(out.contains("nothing is checked there"), "{out}");
        let mut stop_out = Vec::new();
        let stop = run(
            Event::Stop,
            &input(Some(&worktree), ""),
            &context,
            &mut stop_out,
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(stop, 0, "the agent may stop");
        assert!(
            String::from_utf8(stop_out)
                .unwrap()
                .contains("nothing is checked there"),
            "with a note"
        );
    }

    #[test]
    fn a_worktree_whose_xtask_fails_blocks_git() {
        let (temp, main, worktree) = repository_with_nested_worktree();
        let mut broken = context(&main, Some(&main));
        broken.cargo = fake_cargo(temp.path(), 101);

        let (merge_code, merge_out) = run_pre_tool_use(&broken, Some(&worktree), "gh pr merge 1");
        assert_eq!(merge_code, 2, "{merge_out}");
        assert!(merge_out.contains("it exited 101"), "{merge_out}");
        let (build_code, build_out) = run_pre_tool_use(&broken, Some(&worktree), "cargo build");
        assert_eq!(build_code, 0, "{build_out}");

        broken.cargo = fake_cargo(temp.path(), 2);
        assert_eq!(
            run_pre_tool_use(&broken, Some(&worktree), "git push -f").0,
            2,
            "the worktree's own block passes through"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("target-dir")).unwrap(),
            worktree.join("target/hook").display().to_string(),
            "the worktree's guard builds apart from its cargo lock"
        );
    }

    #[test]
    fn feeding_a_child_that_exits_without_reading_is_not_an_error() {
        // A megabyte overflows any pipe buffer, so the write meets the closed pipe for certain.
        let input = "x".repeat(1 << 20);
        let mut child = Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let taken = crate::tools::feed(&mut child, &mut input.as_bytes()).unwrap();
        assert!(!taken, "the child took none of it");
        assert_eq!(
            child.wait().unwrap().code(),
            Some(3),
            "the child's exit status decides, not the broken pipe"
        );
    }

    /// The shells hooks must work under: the system `sh` and dash, Debian's and Ubuntu's `sh`.
    const SHELLS: [&str; 2] = ["sh", "/bin/dash"];

    /// Runs `args` under `shell` with `input` on stdin, the project directory `project` and
    /// `cargo` from `bin` first on `PATH`; returns the exit code and stderr.
    fn run_shell(
        shell: &str,
        args: &[&std::ffi::OsStr],
        project: &Path,
        bin: &Path,
        input: &str,
    ) -> (Option<i32>, String) {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let mut child = Command::new(shell)
            .args(args)
            .env("PATH", path)
            .env("CLAUDE_PROJECT_DIR", project)
            // These tests run in an agent VM too, which sets this; the script's VM path has its own.
            .env_remove("ABC_VM")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("{shell} must be installed: {error}"));
        crate::tools::feed(&mut child, &mut input.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Runs the hook script under each shell, with `cargo` on `PATH` exiting `code`, and checks
    /// that every shell agrees; returns the exit code and the first shell's stderr.
    fn run_hook_script(code: u8, input: &str) -> (Option<i32>, String) {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), code);
        let script = crate::repo_root().unwrap().join(super::HOOK_SCRIPT);
        let results: Vec<(Option<i32>, String)> = SHELLS
            .iter()
            .map(|shell| {
                run_shell(
                    shell,
                    &[script.as_os_str()],
                    temp.path(),
                    temp.path(),
                    input,
                )
            })
            .collect();
        assert!(
            results.windows(2).all(|pair| pair[0].0 == pair[1].0),
            "the shells disagree: {results:?}"
        );
        results.into_iter().next().unwrap()
    }

    /// What one run of the stop script did.
    #[derive(Debug, PartialEq, Eq)]
    struct ScriptRun {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    }

    /// Runs `xtask/hooks/stop.sh` under `shell` on `input`, in the project directory `project`,
    /// with `bin` first on `PATH` and `ABC_VM` set when `vm`.
    fn run_stop_script(
        shell: &str,
        project: &Path,
        bin: &Path,
        input: &str,
        vm: bool,
    ) -> ScriptRun {
        let script = crate::repo_root().unwrap().join(super::STOP_SCRIPT);
        let mut command = Command::new(shell);
        command
            .arg(&script)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("CLAUDE_PROJECT_DIR", project)
            .env_remove("ABC_VM")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if vm {
            command.env("ABC_VM", "1");
        }
        let mut child = command.spawn().unwrap();
        crate::tools::feed(&mut child, &mut input.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        ScriptRun {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }

    /// Writes an executable script `bin/name` with `body`.
    fn executable(bin: &Path, name: &str, body: &str) {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_stop_script_passes_on_the_verdict_and_only_the_handoff_on_stdout() {
        for shell in SHELLS {
            let (project, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            for code in [0, 2] {
                executable(
                    bin.path(),
                    "cargo",
                    &format!(
                        "cat >/dev/null\necho \"$*\"\necho 'from a test'\necho '{{\"systemMessage\":\"handed over\"}}'\nexit {code}\n"
                    ),
                );
                let ran = run_stop_script(shell, project.path(), bin.path(), MAIN, false);
                assert_eq!(ran.code, Some(code), "{shell}: {ran:?}");
                assert_eq!(
                    ran.stdout, "{\"systemMessage\":\"handed over\"}\n",
                    "{shell}"
                );
                assert_eq!(ran.stderr, "xtask hook stop\nfrom a test\n", "{shell}");
            }
            executable(bin.path(), "cargo", "cat >/dev/null\nexit 2\n");
            let quiet = run_stop_script(shell, project.path(), bin.path(), MAIN, false);
            assert_eq!(
                (quiet.code, quiet.stdout.as_str(), quiet.stderr.as_str()),
                (Some(2), "", ""),
                "{shell}"
            );
            assert!(
                !project.path().join(".abc").exists(),
                "xtask kept the files"
            );
        }
    }

    /// A repository to run the stop script on, with its canonical root.
    fn stop_project() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = repository(&temp.path().join("abc")).canonicalize().unwrap();
        (temp, root)
    }

    /// The hook input of the main agent's stop in `cwd`.
    fn main_in(cwd: &Path) -> String {
        format!(
            r#"{{"hook_event_name": "Stop", "cwd": "{}"}}"#,
            cwd.display()
        )
    }

    #[test]
    fn in_an_agent_vm_the_stop_script_runs_the_images_xtask_standalone() {
        let bin = tempfile::tempdir().unwrap();
        executable(
            bin.path(),
            "cargo",
            "cat >/dev/null\necho cargo >&2\nexit 2\n",
        );
        let image = |body: &str| {
            executable(bin.path(), "abc-xtask", &format!("cat >/dev/null\n{body}"));
        };
        for shell in SHELLS {
            let (_temp, project) = stop_project();
            let run = || run_stop_script(shell, &project, bin.path(), MAIN, true);
            image("echo \"abc-xtask $*\" >&2\nexit 0\n");
            let ran = run();
            assert_eq!(
                (ran.code, ran.stderr.as_str()),
                (Some(0), "abc-xtask hook stop --standalone\n"),
                "{shell}: the checkout's xtask is not built"
            );
            for code in [126, 127] {
                image(&format!("echo \"abc-xtask $*\" >&2\nexit {code}\n"));
                let cannot_run = run();
                assert_eq!(
                    (cannot_run.code, cannot_run.stderr.as_str()),
                    (Some(2), "abc-xtask hook stop --standalone\ncargo\n"),
                    "{shell}: an xtask that cannot run falls back to the checkout's ({code})"
                );
            }
            image("echo \"error: unexpected argument '--standalone' found\" >&2\nexit 2\n");
            let old_image = run();
            assert_eq!(
                (old_image.code, old_image.stderr.as_str()),
                (Some(2), "cargo\n"),
                "{shell}: an image that predates the flag falls back too"
            );
            image("echo \"error: unexpected argument '--standalone' found\" >&2\nexit 1\n");
            let old_exit_one = run();
            assert_eq!(
                (old_exit_one.code, old_exit_one.stderr.as_str()),
                (Some(2), "cargo\n"),
                "{shell}: the usage error exits 1 in xtask's own CLI"
            );
            image("echo \"error: something else\" >&2\nexit 2\n");
            let blocked = run();
            assert_eq!(
                (blocked.code, blocked.stderr.as_str()),
                (Some(2), "error: something else\n"),
                "{shell}: any other block is xtask's own"
            );
            image("echo crashed >&2\nexit 101\n");
            let crashed = run();
            assert_eq!(crashed.code, Some(2), "{shell}: {crashed:?}");
            assert!(
                crashed.stderr.starts_with("crashed\n") && !crashed.stderr.contains("cargo\n"),
                "{shell}: a crash is counted, not retried: {crashed:?}"
            );
            assert_eq!(crate::task::blocked_stops(&project).unwrap(), 1, "{shell}");
        }
    }

    #[test]
    fn when_xtask_cannot_run_the_stop_script_never_honours_a_declaration_unjudged() {
        for shell in SHELLS {
            let (_temp, project) = stop_project();
            let bin = tempfile::tempdir().unwrap();
            fake_cargo(bin.path(), 101);
            crate::task::block(&project, "no \"network\" \\ here\n\ttried twice").unwrap();
            let held = run_stop_script(shell, &project, bin.path(), MAIN, false);
            assert_eq!((held.code, held.stdout.as_str()), (Some(2), ""), "{shell}");
            assert!(held.stderr.contains("was not judged"), "{shell}: {held:?}");
            assert!(
                project.join(crate::task::BLOCKED).is_file(),
                "{shell}: kept"
            );
            assert_eq!(crate::task::blocked_stops(&project).unwrap(), 1, "{shell}");
            assert!(!project.join(crate::task::BLOCKED_LOG).exists(), "{shell}");
            for _ in 0..4 {
                crate::task::count_blocked_stop(&project).unwrap();
            }
            let ran = run_stop_script(shell, &project, bin.path(), MAIN, false);
            assert_eq!(ran.code, Some(0), "{shell}: {ran:?}");
            let json: serde_json::Value = serde_json::from_str(&ran.stdout).unwrap();
            assert_eq!(
                json["systemMessage"],
                "BLOCKED, stop allowed (the stop hook's xtask could not run (it exited 101); \
                 5 stops in a row were blocked and the declaration could not be judged). \
                 Handoff: no \"network\" \\ here\n tried twice",
                "{shell}"
            );
            assert!(!project.join(crate::task::BLOCKED).exists(), "taken");
            assert_eq!(crate::task::blocked_stops(&project).unwrap(), 0, "reset");
            let log = fs::read_to_string(project.join(crate::task::BLOCKED_LOG)).unwrap();
            assert!(
                log.contains("stop allowed, not judged") && log.contains("tried twice"),
                "{shell}: {log}"
            );
        }
    }

    #[test]
    fn when_xtask_cannot_run_the_stop_script_counts_and_lets_the_sixth_through() {
        for shell in SHELLS {
            let (_temp, project) = stop_project();
            let bin = tempfile::tempdir().unwrap();
            fake_cargo(bin.path(), 101);
            fs::create_dir_all(project.join(".abc")).unwrap();
            fs::write(project.join(".abc/stop-blocks"), "garbled\n").unwrap();
            for count in 1..=5 {
                let ran = run_stop_script(shell, &project, bin.path(), MAIN, false);
                assert_eq!((ran.code, ran.stdout.as_str()), (Some(2), ""), "{shell}");
                assert!(ran.stderr.contains(".abc/blocked.md"), "{shell}: {ran:?}");
                assert_eq!(crate::task::blocked_stops(&project).unwrap(), count);
            }
            let sixth = run_stop_script(shell, &project, bin.path(), MAIN, false);
            assert_eq!(sixth.code, Some(0), "{shell}: {sixth:?}");
            let json: serde_json::Value = serde_json::from_str(&sixth.stdout).unwrap();
            assert!(
                json["systemMessage"]
                    .as_str()
                    .unwrap()
                    .contains("5 stops in a row"),
                "{shell}: {json}"
            );
            assert_eq!(crate::task::blocked_stops(&project).unwrap(), 0);
            let note = fs::read_to_string(project.join(crate::task::BACKSTOP)).unwrap();
            assert!(note.contains("after 5 blocked stops"), "{shell}: {note}");
            let leftovers: Vec<_> = fs::read_dir(project.join(".abc"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().contains("stop-blocks."))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    #[test]
    fn when_xtask_cannot_run_each_subagent_is_counted_apart_and_let_through() {
        for shell in SHELLS {
            let (_temp, project) = stop_project();
            let bin = tempfile::tempdir().unwrap();
            fake_cargo(bin.path(), 101);
            crate::task::block(&project, "the main agent's").unwrap();
            let input = r#"{"hook_event_name": "SubagentStop", "agent_id": "a/1", "last_assistant_message": "\"hook_event_name\":\"Stop\""}"#;
            for count in 1..=5 {
                let ran = run_stop_script(shell, &project, bin.path(), input, false);
                assert_eq!((ran.code, ran.stdout.as_str()), (Some(2), ""), "{shell}");
                assert!(!ran.stderr.contains("blocked.md"), "{shell}: {ran:?}");
                assert_eq!(
                    crate::task::subagent_stops(&project, "a/1").unwrap(),
                    count,
                    "{shell}"
                );
            }
            let sixth = run_stop_script(shell, &project, bin.path(), input, false);
            assert_eq!(sixth.code, Some(0), "{shell}: {sixth:?}");
            assert!(
                sixth.stdout.contains("A subagent stop was held 5 times"),
                "{shell}"
            );
            assert_eq!(crate::task::subagent_stops(&project, "a/1").unwrap(), 0);
            let unnamed = r#"{"hook_event_name": "SubagentStop"}"#;
            run_stop_script(shell, &project, bin.path(), unnamed, false);
            assert_eq!(
                crate::task::subagent_stops(&project, "").unwrap(),
                1,
                "{shell}"
            );
            assert!(project.join(crate::task::BLOCKED).is_file(), "{shell}");
            assert_eq!(crate::task::blocked_stops(&project).unwrap(), 0);
            assert!(!project.join(crate::task::BLOCKED_LOG).exists());
        }
    }

    #[test]
    fn when_xtask_cannot_run_the_stop_script_keeps_its_files_where_the_agent_is() {
        for shell in SHELLS {
            let (temp, main, worktree) = repository_with_nested_worktree();
            let bin = tempfile::tempdir().unwrap();
            fake_cargo(bin.path(), 101);
            let ran = run_stop_script(shell, &main, bin.path(), &main_in(&worktree), false);
            assert_eq!(ran.code, Some(2), "{shell}: {ran:?}");
            assert_eq!(crate::task::blocked_stops(&worktree).unwrap(), 1, "{shell}");
            assert_eq!(crate::task::blocked_stops(&main).unwrap(), 0, "{shell}");
            let gone = main_in(&main.join("gone"));
            run_stop_script(shell, &main, bin.path(), &gone, false);
            assert_eq!(
                crate::task::blocked_stops(&main).unwrap(),
                1,
                "{shell}: a cwd that does not exist falls back to the project directory"
            );
            let other = repository(&temp.path().join("other"));
            let elsewhere = run_stop_script(shell, &main, bin.path(), &main_in(&other), false);
            assert_eq!(elsewhere.code, Some(0), "{shell}: {elsewhere:?}");
            assert!(elsewhere.stderr.contains("not a checkout"), "{shell}");
            assert!(!other.join(".abc").exists(), "{shell}");
            let outside = tempfile::tempdir().unwrap();
            let nowhere =
                run_stop_script(shell, &main, bin.path(), &main_in(outside.path()), false);
            assert_eq!(nowhere.code, Some(0), "{shell}: {nowhere:?}");
            assert!(
                nowhere.stderr.contains("not in a git repository"),
                "{shell}"
            );
        }
    }

    /// Every hook command in `.claude/settings.json`, with its event.
    fn hook_commands() -> Vec<(String, String)> {
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        settings["hooks"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(event, groups)| {
                groups.as_array().unwrap().iter().flat_map(move |group| {
                    group["hooks"].as_array().unwrap().iter().map(move |hook| {
                        (event.clone(), hook["command"].as_str().unwrap().to_owned())
                    })
                })
            })
            .collect()
    }

    #[test]
    fn a_handed_over_hook_names_its_event_as_the_command_line_does() {
        use clap::ValueEnum as _;
        for event in Event::value_variants() {
            let name = event.to_possible_value().unwrap();
            assert_eq!(event.name(), name.get_name(), "{event:?}");
        }
    }

    #[test]
    fn hook_commands_note_a_checkout_without_xtask_and_let_it_be() {
        let empty = tempfile::tempdir().unwrap();
        let commands = hook_commands();
        assert_eq!(
            commands.len(),
            3,
            "PreToolUse, Stop and SubagentStop: {commands:?}"
        );
        for (event, command) in commands {
            for shell in SHELLS {
                let (code, stderr) = run_shell(
                    shell,
                    &["-c".as_ref(), command.as_ref()],
                    empty.path(),
                    empty.path(),
                    r#"{"tool_name":"Bash","tool_input":{"command":"git push -f"}}"#,
                );
                assert_eq!(code, Some(0), "{event} under {shell}: {stderr}");
                assert_eq!(stderr.lines().count(), 1, "{event} under {shell}: {stderr}");
                assert!(stderr.starts_with("xtask hook: "), "{event}: {stderr}");
            }
        }
    }

    #[test]
    fn locating_ignores_an_inherited_repository_override() {
        let git = git_in(Path::new("/"));
        let removed: Vec<_> = git
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        assert_eq!(
            removed,
            ["GIT_DIR", "GIT_WORK_TREE"],
            "a hook run inside another git command must not inherit its repository"
        );
    }

    #[test]
    fn unparseable_input_blocks_when_the_guard_cannot_run() {
        let mut out = Vec::new();
        let code = guard_unavailable("the build failed", "not json", &mut out).unwrap();
        assert_eq!(code, 2, "no command can be read, so it may be git");
        assert!(
            String::from_utf8(out).unwrap().starts_with("Blocked: "),
            "and the agent is told"
        );
    }

    #[test]
    fn settings_wire_the_hooks_to_xtask() {
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let hooks = &settings["hooks"];
        assert_eq!(
            hooks["PreToolUse"][0]["matcher"], "Bash|Monitor",
            "both tools that run shell commands are guarded"
        );
        let guard = &hooks["PreToolUse"][0]["hooks"][0];
        assert_eq!(
            guard["command"],
            "if [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/pre-tool-use.sh\" ]; then sh \"$CLAUDE_PROJECT_DIR/xtask/hooks/pre-tool-use.sh\"; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no xtask/hooks/pre-tool-use.sh; the git guard is off\" >&2; fi",
            "the guard runs the hook script"
        );
        assert_eq!(guard["timeout"], 300, "the guard is time-boxed");
        let stop_command = "if [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/stop.sh\" ]; then sh \"$CLAUDE_PROJECT_DIR/xtask/hooks/stop.sh\"; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no xtask/hooks/stop.sh; no checks run\" >&2; fi";
        for event in ["Stop", "SubagentStop"] {
            let stop = &hooks[event][0]["hooks"][0];
            assert_eq!(
                stop["command"], stop_command,
                "{event} runs the stop script"
            );
            assert_eq!(stop["timeout"], 900, "{event} is time-boxed");
        }
        assert!(
            settings.get("env").is_none() && settings.get("permissions").is_none(),
            "sessions run in agent VMs, which hold no credential but the broker's tokens (D37)"
        );
        assert_eq!(
            hooks["SubagentStop"][0]["matcher"],
            "^(?!(?:Explore|Plan|claude-code-guide|statusline-setup|web-fetch)$).+",
            "read-only and internal subagents are exempt"
        );
    }

    #[test]
    fn in_an_agent_vm_the_hook_script_checks_the_billing_first() {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), 0);
        let checked = temp.path().join("checked");
        let script = crate::repo_root().unwrap().join(super::HOOK_SCRIPT);
        let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        let run = |vm: bool| {
            let path = format!(
                "{}:{}",
                temp.path().display(),
                std::env::var("PATH").unwrap()
            );
            let mut command = Command::new("sh");
            command
                .arg(&script)
                .env("PATH", path)
                .env("CLAUDE_PROJECT_DIR", temp.path())
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .stdout(Stdio::null());
            if vm {
                command.env("ABC_VM", "1");
            } else {
                command.env_remove("ABC_VM");
            }
            let mut child = command.spawn().unwrap();
            crate::tools::feed(&mut child, &mut input.as_bytes()).unwrap();
            let output = child.wait_with_output().unwrap();
            (
                output.status.code(),
                String::from_utf8(output.stderr).unwrap(),
            )
        };
        crate::testing::script(
            &temp.path().join("abc-xtask"),
            &format!(
                "echo \"$*\" > {}\necho 'found: ANTHROPIC_API_KEY in the environment' >&2\nexit 1\n",
                checked.display()
            ),
        );
        let (code, stderr) = run(true);
        assert_eq!(code, Some(2), "a finding blocks the tool call: {stderr}");
        assert!(stderr.contains("ANTHROPIC_API_KEY"), "{stderr}");
        assert_eq!(fs::read_to_string(&checked).unwrap(), "vm billing\n");
        fs::remove_file(&checked).unwrap();
        assert_eq!(
            run(false).0,
            Some(0),
            "outside a VM there is nothing to check"
        );
        assert!(!checked.exists());
        crate::testing::script(&temp.path().join("abc-xtask"), "exit 0\n");
        assert_eq!(run(true).0, Some(0), "a clean VM goes on to the guard");
    }

    #[test]
    fn the_hook_script_blocks_git_when_xtask_cannot_run() {
        let git_push = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"git push origin main"}}"#;
        let (code, stderr) = run_hook_script(101, git_push);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains("could not run"), "{stderr}");
        assert_eq!(
            run_hook_script(1, git_push).0,
            Some(2),
            "exit 1 is a failure to run the guard, not its verdict"
        );

        let quoted =
            r#"{"tool_name":"Bash","tool_input":{"command":"echo \"x\" && gh api repos/o/r"}}"#;
        assert_eq!(run_hook_script(101, quoted).0, Some(2), "escaped quotes");
    }

    #[test]
    fn the_hook_script_lets_other_commands_through_when_xtask_cannot_run() {
        let listing = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"ls .github .gitignore"}}"#;
        let (code, stderr) = run_hook_script(101, listing);
        assert_eq!(
            code,
            Some(0),
            "a git path in cwd is not a git command: {stderr}"
        );
        assert!(stderr.starts_with("warning: "), "{stderr}");

        let socket = r#"{"tool_name":"Monitor","tool_input":{"ws":{"url":"wss://example.com"}}}"#;
        assert_eq!(run_hook_script(101, socket).0, Some(0), "no command at all");
    }

    #[test]
    fn the_hook_script_builds_xtask_apart_from_the_agents_builds() {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), 0);
        let script = crate::repo_root().unwrap().join(super::HOOK_SCRIPT);
        run_shell("sh", &[script.as_os_str()], temp.path(), temp.path(), "{}");
        assert_eq!(
            fs::read_to_string(temp.path().join("target-dir")).unwrap(),
            temp.path().join("target/hook").display().to_string(),
            "the guard never waits on the lock of target/"
        );
    }

    #[test]
    fn the_hook_script_passes_on_the_guards_verdict() {
        let input = r#"{"tool_name":"Bash","tool_input":{"command":"git push -f"}}"#;
        assert_eq!(run_hook_script(2, input).0, Some(2), "a block");
        assert_eq!(run_hook_script(0, input).0, Some(0), "an allow");
    }

    #[test]
    fn the_watchdog_judges_only_the_main_agents_first_stop() {
        let fields = |json: &str| serde_json::from_str::<super::HookInput>(json).unwrap();
        assert!(
            fields(r#"{"hook_event_name": "Stop", "stop_hook_active": false}"#).watched(),
            "the main agent stopping"
        );
        assert!(
            !fields(r#"{"hook_event_name": "Stop", "stop_hook_active": true}"#).watched(),
            "an agent a stop hook already sent back is never blocked again"
        );
        assert!(
            !fields(r#"{"hook_event_name": "SubagentStop"}"#).watched(),
            "subagents have no task of their own"
        );
        assert!(!fields("{}").watched(), "an unnamed event is not watched");
    }

    /// The hook input of the main agent's first stop.
    const MAIN: &str = r#"{"hook_event_name": "Stop", "stop_hook_active": false}"#;
    /// The hook input of the main agent's stop after a stop hook sent it back.
    const MAIN_AGAIN: &str = r#"{"hook_event_name": "Stop", "stop_hook_active": true}"#;
    /// The hook input of a subagent's stop.
    const SUBAGENT: &str =
        r#"{"hook_event_name": "SubagentStop", "agent_id": "a1", "stop_hook_active": false}"#;
    /// The hook input of another subagent's stop.
    const OTHER_SUBAGENT: &str =
        r#"{"hook_event_name": "SubagentStop", "agent_id": "b2", "stop_hook_active": false}"#;

    /// What one stop did: the verdict, what it said, and whether the blocker and drift watchdogs
    /// ran.
    #[derive(Debug)]
    struct Stopped {
        verdict: Verdict,
        said: String,
        judged: bool,
        watched: bool,
    }

    /// Runs the stop decision for `input` with a check that answers `check`, a blocker judge that
    /// answers `sent_back`, and a drift watchdog that answers `drift`.
    fn stop_with(
        root: &Path,
        input: &str,
        check: anyhow::Result<bool>,
        sent_back: Option<&str>,
        drift: u8,
    ) -> Stopped {
        let fields = serde_json::from_str::<super::HookInput>(input).unwrap();
        let mut out = Vec::new();
        let (mut judged, mut watched) = (false, false);
        let verdict = super::stop(
            root,
            &fields,
            &mut out,
            |_| check,
            |declaration, _| {
                assert_ne!(declaration, "");
                judged = true;
                Ok(sent_back.map(str::to_owned))
            },
            |_| {
                watched = true;
                Ok(drift)
            },
        )
        .unwrap();
        Stopped {
            verdict,
            said: String::from_utf8(out).unwrap(),
            judged,
            watched,
        }
    }

    fn passes(root: &Path, input: &str) -> Stopped {
        stop_with(root, input, Ok(true), None, 0)
    }

    fn fails(root: &Path, input: &str) -> Stopped {
        stop_with(root, input, Ok(false), None, 0)
    }

    #[test]
    fn a_passing_stop_goes_to_the_watchdog_when_watched() {
        let dir = tempfile::tempdir().unwrap();
        let first = passes(dir.path(), MAIN);
        assert_eq!((first.verdict, first.watched), (Verdict::Allow, true));
        let drifted = stop_with(dir.path(), MAIN, Ok(true), None, 2);
        assert_eq!(drifted.verdict, Verdict::Block, "drift keeps it working");
        for input in [MAIN_AGAIN, SUBAGENT] {
            let unwatched = stop_with(dir.path(), input, Ok(true), None, 2);
            assert_eq!(
                (unwatched.verdict, unwatched.watched),
                (Verdict::Allow, false),
                "{input}"
            );
        }
    }

    #[test]
    fn a_failing_stop_is_blocked_until_the_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fails(dir.path(), MAIN).verdict, Verdict::Block, "fail");
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 1);
        assert_eq!(fails(dir.path(), MAIN_AGAIN).verdict, Verdict::Block);
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 2);
        assert_eq!(passes(dir.path(), MAIN_AGAIN).verdict, Verdict::Allow);
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 0, "reset");
    }

    #[test]
    fn a_check_that_cannot_run_counts_as_a_blocked_stop() {
        let dir = tempfile::tempdir().unwrap();
        let stopped = stop_with(
            dir.path(),
            MAIN,
            Err(anyhow::anyhow!("Postgres is down")),
            None,
            0,
        );
        assert_eq!(stopped.verdict, Verdict::Block);
        assert!(
            stopped
                .said
                .contains("the check could not run: Postgres is down"),
            "{}",
            stopped.said
        );
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 1);
    }

    #[test]
    fn a_subagent_stop_counts_apart_and_is_let_through_after_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        crate::task::block(dir.path(), "no network; tried twice").unwrap();
        crate::task::count_blocked_stop(dir.path()).unwrap();
        let failed = fails(dir.path(), SUBAGENT);
        assert_eq!((failed.verdict, failed.judged), (Verdict::Block, false));
        let erred = stop_with(dir.path(), SUBAGENT, Err(anyhow::anyhow!("down")), None, 0);
        assert_eq!(erred.verdict, Verdict::Block);
        assert_eq!(crate::task::subagent_stops(dir.path(), "a1").unwrap(), 2);
        assert_eq!(crate::task::subagent_stops(dir.path(), "b2").unwrap(), 0);
        for _ in 0..3 {
            assert_eq!(fails(dir.path(), SUBAGENT).verdict, Verdict::Block);
        }
        assert_eq!(
            fails(dir.path(), OTHER_SUBAGENT).verdict,
            Verdict::Block,
            "another subagent has its own count"
        );
        let sixth = fails(dir.path(), SUBAGENT);
        let Verdict::Handoff(message) = sixth.verdict else {
            panic!("{sixth:?}");
        };
        assert!(message.contains("5 of its stops in a row"), "{message}");
        assert_eq!(crate::task::subagent_stops(dir.path(), "a1").unwrap(), 0);
        assert_eq!(
            fails(dir.path(), SUBAGENT).verdict,
            Verdict::Block,
            "starts over"
        );
        assert_eq!(passes(dir.path(), SUBAGENT).verdict, Verdict::Allow);
        assert_eq!(
            crate::task::subagent_stops(dir.path(), "a1").unwrap(),
            0,
            "reset"
        );
        assert_eq!(
            crate::task::blocked_stops(dir.path()).unwrap(),
            1,
            "the main agent's count is neither advanced nor reset"
        );
        assert!(
            dir.path().join(crate::task::BLOCKED).is_file(),
            "the declaration is still the main agent's"
        );
        assert!(!dir.path().join(crate::task::BLOCKED_LOG).exists());
        let unnamed = fails(
            dir.path(),
            r#"{"hook_event_name": "SubagentStop", "stop_hook_active": false}"#,
        );
        assert_eq!(
            unnamed.verdict,
            Verdict::Block,
            "an unnamed subagent counts too"
        );
        assert_eq!(crate::task::subagent_stops(dir.path(), "").unwrap(), 1);
        let main = fails(dir.path(), MAIN);
        assert!(matches!(main.verdict, Verdict::Handoff(_)), "{main:?}");
    }

    #[test]
    fn a_genuine_blocked_declaration_ends_the_stop_with_a_handoff_and_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        crate::task::count_blocked_stop(dir.path()).unwrap();
        crate::task::block(dir.path(), "no network; tried twice").unwrap();
        let stopped = stop_with(
            dir.path(),
            MAIN_AGAIN,
            Err(anyhow::anyhow!("the check must not run")),
            None,
            2,
        );
        assert_eq!(
            stopped.verdict,
            Verdict::Handoff("BLOCKED, stop allowed. Handoff: no network; tried twice".to_owned())
        );
        assert!(stopped.judged, "the watchdog was asked");
        assert!(!stopped.watched, "the drift watchdog was not");
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 0, "reset");
        let log = fs::read_to_string(dir.path().join(crate::task::BLOCKED_LOG)).unwrap();
        assert!(
            log.contains(": stop allowed\n\nno network; tried twice\n"),
            "{log}"
        );
        assert_eq!(
            fails(dir.path(), MAIN).verdict,
            Verdict::Block,
            "it covered one stop"
        );
    }

    #[test]
    fn every_blocked_declaration_is_judged_and_a_refused_one_counts() {
        let dir = tempfile::tempdir().unwrap();
        crate::task::block(dir.path(), "the check fails").unwrap();
        let sent_back = stop_with(
            dir.path(),
            MAIN,
            Ok(false),
            Some("a failing check is the work"),
            0,
        );
        assert_eq!(sent_back.verdict, Verdict::Block);
        assert!(
            sent_back
                .said
                .contains("sent back: a failing check is the work"),
            "{}",
            sent_back.said
        );
        assert!(!dir.path().join(crate::task::BLOCKED).exists(), "taken");
        assert_eq!(
            crate::task::blocked_stops(dir.path()).unwrap(),
            1,
            "counted"
        );
        crate::task::block(dir.path(), "the check fails, and I mean it").unwrap();
        let again = stop_with(dir.path(), MAIN_AGAIN, Ok(false), Some("still the work"), 0);
        assert!(again.judged, "the second declaration is judged too");
        assert_eq!(again.verdict, Verdict::Block);
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 2);
        crate::task::block(dir.path(), "outside my control").unwrap();
        let accepted = stop_with(dir.path(), MAIN_AGAIN, Ok(false), None, 0);
        assert!(accepted.judged);
        assert!(
            matches!(accepted.verdict, Verdict::Handoff(_)),
            "{accepted:?}"
        );
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 0, "reset");
        let log = fs::read_to_string(dir.path().join(crate::task::BLOCKED_LOG)).unwrap();
        assert!(
            log.contains(": sent back: a failing check is the work\n\nthe check fails\n")
                && log.contains(": sent back: still the work\n\nthe check fails, and I mean it\n")
                && log.contains(": stop allowed\n\noutside my control\n"),
            "{log}"
        );
    }

    #[test]
    fn refused_declarations_cannot_hold_a_stop_for_ever() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..5 {
            crate::task::block(dir.path(), "again").unwrap();
            let held = stop_with(dir.path(), MAIN, Ok(false), Some("work"), 0);
            assert_eq!((held.verdict, held.judged), (Verdict::Block, true));
        }
        crate::task::block(dir.path(), "again").unwrap();
        let sixth = stop_with(dir.path(), MAIN, Ok(false), Some("work"), 0);
        assert!(!sixth.judged, "the backstop does not wait for the watchdog");
        assert!(matches!(sixth.verdict, Verdict::Handoff(_)), "{sixth:?}");
        assert_eq!(crate::task::blocked_stops(dir.path()).unwrap(), 0);
        let log = fs::read_to_string(dir.path().join(crate::task::BLOCKED_LOG)).unwrap();
        assert!(
            log.contains(": stop allowed, not judged: 5 blocked stops in a row\n\nagain\n"),
            "{log}"
        );
    }

    #[test]
    fn the_backstop_lets_the_sixth_blocked_stop_through_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..5 {
            assert_eq!(fails(dir.path(), MAIN).verdict, Verdict::Block);
        }
        let sixth = fails(dir.path(), MAIN);
        let Verdict::Handoff(message) = sixth.verdict else {
            panic!("{sixth:?}");
        };
        assert!(!sixth.watched);
        assert!(message.contains("5 stops in a row"), "{message}");
        let note = fs::read_to_string(dir.path().join(crate::task::BACKSTOP)).unwrap();
        assert!(note.contains("after 5 blocked stops"), "{note}");
        assert_eq!(
            fails(dir.path(), MAIN).verdict,
            Verdict::Block,
            "the count starts over"
        );
    }

    #[test]
    fn a_handoff_reaches_the_user_as_a_system_message() {
        let (mut out, mut user) = (Vec::new(), Vec::new());
        let code = Verdict::Handoff("BLOCKED: \"no\" network".to_owned())
            .emit(&mut out, &mut user)
            .unwrap();
        assert_eq!(code, 0);
        let json: serde_json::Value = serde_json::from_slice(&user).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"systemMessage": "BLOCKED: \"no\" network"})
        );
        assert_eq!(String::from_utf8(out).unwrap(), "BLOCKED: \"no\" network\n");
        let (mut said, mut shown) = (Vec::new(), Vec::new());
        assert_eq!(Verdict::Allow.emit(&mut said, &mut shown).unwrap(), 0);
        assert_eq!(Verdict::Block.emit(&mut said, &mut shown).unwrap(), 2);
        assert!(said.is_empty() && shown.is_empty(), "nothing to say");
    }

    #[test]
    fn standalone_stop_runs_on_the_project_directory() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let mut standalone = context(&main, Some(&worktree));
        standalone.standalone = true;
        crate::task::block(&worktree, "no network; tried twice").unwrap();
        let (mut out, mut user) = (Vec::new(), Vec::new());
        let code = run(Event::Stop, MAIN, &standalone, &mut out, &mut user).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
        assert!(
            String::from_utf8_lossy(&user).contains("no network; tried twice"),
            "the worktree's declaration, though xtask's own root is the main checkout"
        );
        assert!(worktree.join(crate::task::BLOCKED_LOG).is_file());
        standalone.project_dir = None;
        out.clear();
        let without_project = run(Event::Stop, MAIN, &standalone, &mut out, &mut user).unwrap();
        let said = String::from_utf8_lossy(&out).into_owned();
        assert_eq!(without_project, 2, "{said}");
        assert!(said.contains("CLAUDE_PROJECT_DIR"), "{said}");
        assert_eq!(
            crate::task::blocked_stops(&main).unwrap(),
            1,
            "counted in xtask's own"
        );
        standalone.project_dir = Some(main.join("gone"));
        assert_eq!(
            run(Event::Stop, MAIN, &standalone, &mut out, &mut user).unwrap(),
            2
        );
    }

    #[test]
    fn the_watchdogs_own_session_stops_without_checks() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let mut inside = context(&main, Some(&main));
        inside.inside_watchdog = true;
        inside.cargo = fake_cargo(temp.path(), 101);
        let mut out = Vec::new();
        assert_eq!(
            run(
                Event::Stop,
                "not json",
                &inside,
                &mut out,
                &mut std::io::sink()
            )
            .unwrap(),
            0,
            "no check, no watchdog, no recursion"
        );
        assert!(out.is_empty(), "and nothing to say");
    }

    #[test]
    fn a_handed_over_hook_is_not_handed_on() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let mut handed_over = context(&main, Some(&main));
        handed_over.delegated = true;
        let mut said = Vec::new();
        let stopped = run(
            Event::Stop,
            &input(Some(&worktree), ""),
            &handed_over,
            &mut said,
            &mut std::io::sink(),
        )
        .unwrap();
        let said = String::from_utf8(said).unwrap();
        assert_eq!(stopped, 2, "{said}");
        assert!(said.contains("another checkout"), "{said}");
        assert_eq!(
            crate::task::subagent_stops(&worktree, "").unwrap(),
            1,
            "counted in the checkout the agent is in"
        );
        assert_eq!(crate::task::subagent_stops(&main, "").unwrap(), 0);
        let (code, out) = run_pre_tool_use(&handed_over, Some(&worktree), "git status");
        assert_eq!(code, 2, "the guard fails closed for git: {out}");
    }

    #[test]
    fn unreadable_input_blocks_the_guard_and_fails_stop() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            "not json",
            &context(&main, Some(&main)),
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(code, 2, "the guard blocks input it cannot read");
        let mut said = Vec::new();
        let stopped = run(
            Event::Stop,
            "not json",
            &context(&main, Some(&main)),
            &mut said,
            &mut std::io::sink(),
        )
        .unwrap();
        let said = String::from_utf8(said).unwrap();
        assert_eq!(stopped, 2, "a stop hook that fails holds the stop: {said}");
        assert!(
            said.contains("could not read the Stop hook input"),
            "{said}"
        );
        assert_eq!(
            crate::task::blocked_stops(&main).unwrap(),
            1,
            "and counts it"
        );
    }
}
