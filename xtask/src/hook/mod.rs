//! Claude Code hooks: find the checkout the agent is working in, then act there, and answer in
//! Claude Code's hook JSON.
//!
//! `xtask/hooks/hook.sh` builds this checkout's xtask and runs `cargo xtask hook <event>`, which
//! always exits 0 and prints its [`Answer`] on stdout: nothing to let the action go on, a
//! `systemMessage` to go on and tell the user something, or a block, which is
//! `{"decision": "block", "reason": ...}` for `Stop` and `SubagentStop` and
//! `hookSpecificOutput.permissionDecision: "deny"` for `PreToolUse`. No exit code carries a
//! decision, so a usage error, a panic or a build failure can never read as one: the launcher
//! answers for an xtask that cannot start.
//!
//! The answer's shape, the hook inputs this reads (`cwd`, `agent_id`, `stop_hook_active`) and the
//! rule that a timed-out hook's output is discarded are Claude Code's, as its hooks reference
//! specifies them: <https://code.claude.com/docs/en/hooks>. Check a change here against it.
//!
//! Claude Code starts every hook from the directory the session began in, but the agent may have
//! moved since, for example into a worktree. The hook input's `cwd` says where it is now; without
//! one, the session's project directory stands in. The hook resolves that directory's repository
//! root and then:
//!
//! - runs here when the root is this checkout;
//! - hands the input to that checkout's own xtask when it is another worktree of this repository,
//!   so the checkout is held to its own toolchain, tools and gate;
//! - does nothing when the directory is in no repository, or in a different one.
//!
//! The `PreToolUse` guard catches accidents; it is not the enforcement. GitHub rulesets on `main`
//! (no force push, no deletion, required checks) and the abc-agent App's limited permissions are.
//! When the guard cannot run, it blocks commands that mention git or gh and lets the rest through,
//! so a broken guard never locks the agent out of the shell.
//!
//! The stop hook drives the state machine in `crate::stop`: it runs `check`, the BLOCKED judge and
//! the drift watchdog when a decision asks for them, and answers with the decision it comes to.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::stop::{self, Agent, AgentId, Block, Decision, Ruling};
use crate::{guard, tools, watchdog};

/// The launcher every hook in `.claude/settings.json` runs. A checkout without it predates these
/// hooks, so its xtask cannot take a hook over.
pub(crate) const LAUNCHER: &str = "xtask/hooks/hook.sh";

/// The cargo arguments that run a checkout's xtask hook, before the event's name: xtask's own
/// workspace, built into `target/hook` so a hook never waits on the agent's build lock. The
/// launcher runs the same command.
const XTASK_HOOK: [&str; 9] = [
    "run",
    "--quiet",
    "--locked",
    "--manifest-path",
    "xtask/Cargo.toml",
    "--target-dir",
    "target/hook",
    "--",
    "hook",
];

/// The status the launcher reads as "the check ran and failed, but xtask could not keep its state
/// to count the hold": the launcher counts it, with what xtask printed as the reason.
pub(crate) const UNKEPT_STATUS: u8 = 3;

/// How long a stop's `check` may take, from its start, a wait on cargo's build lock included. The
/// hook's own timeout in `.claude/settings.json` is 900 s and a hook that times out passes
/// silently, because Claude Code discards what a cancelled hook prints; this leaves the watchdog
/// (`watchdog::TIMEOUT`, 240 s) and the build of xtask room before it.
const CHECK_DEADLINE: Duration = Duration::from_secs(600);

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
}

impl Context {
    pub(crate) fn from_env(own_root: PathBuf) -> Self {
        Self {
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
#[derive(Debug, Default, Deserialize)]
struct HookInput {
    cwd: Option<PathBuf>,
    /// `Stop` or `SubagentStop` for the stop hook.
    hook_event_name: Option<String>,
    /// The subagent the hook runs for: `SubagentStop`'s, and a `PreToolUse` in a subagent has one
    /// too. The main agent's hooks have none.
    agent_id: Option<String>,
    /// The tool call, for `PreToolUse`.
    tool_input: Option<ToolInput>,
    /// Whether the agent is already working on because a stop hook blocked it.
    #[serde(default)]
    stop_hook_active: bool,
}

/// The part of a tool call's input the hook reads.
#[derive(Debug, Default, Deserialize)]
struct ToolInput {
    /// What a Bash call runs.
    command: Option<String>,
}

/// Why a subagent may not run `cargo xtask task blocked`.
const MAIN_DECLARES: &str = "Blocked: only the main agent declares a task BLOCKED. A subagent \
     finishes its work, or tells the main agent what stops it.";

/// Whether `command` runs `xtask task blocked`, as `cargo xtask task blocked` does.
fn declares_blocked(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
        .filter(|word| !word.is_empty())
        .collect();
    words.windows(3).any(|w| w == ["xtask", "task", "blocked"])
}

impl HookInput {
    /// Whose stop this is: a subagent's for `SubagentStop`, and otherwise the main agent's.
    fn agent(&self) -> Agent {
        if self.hook_event_name.as_deref() == Some("SubagentStop") {
            Agent::Sub(AgentId::new(self.agent_id.clone().unwrap_or_default()))
        } else {
            Agent::Main
        }
    }
}

/// A hook's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Say nothing: the tool call goes on to Claude Code's own permission checks, or the agent
    /// stops.
    Allow,
    /// Go on, and show the user this message.
    Notify(String),
    /// Refuse the tool call, or keep the agent working; Claude Code gives the agent the reason.
    Block(String),
    /// The stop's check failed, and xtask could not keep its state to count the hold. Not JSON:
    /// the hook exits [`UNKEPT_STATUS`] with this on stdout, and the launcher counts the hold.
    Held(String),
}

/// The JSON a hook prints, with only the fields xtask uses (Claude Code's hooks reference).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Printed {
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    system_message: Option<String>,
    /// `Stop` and `SubagentStop`: `block` keeps the agent working.
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<StopDecision>,
    /// Why, for the agent, with `decision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    hook_specific_output: Option<PreToolUseOutput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StopDecision {
    Block,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreToolUseOutput {
    hook_event_name: PreToolUseName,
    permission_decision: PermissionDecision,
    permission_decision_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum PreToolUseName {
    PreToolUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PermissionDecision {
    Deny,
}

impl Answer {
    /// The answer as the one line of JSON a hook for `event` prints; `None` when it says nothing.
    pub(crate) fn json(&self, event: Event) -> Option<String> {
        let output = match (self, event) {
            (Self::Allow | Self::Held(_), _) => return None,
            (Self::Notify(message), _) => Printed {
                system_message: Some(message.clone()),
                ..Printed::default()
            },
            (Self::Block(reason), Event::Stop) => Printed {
                decision: Some(StopDecision::Block),
                reason: Some(reason.clone()),
                ..Printed::default()
            },
            (Self::Block(reason), Event::PreToolUse) => Printed {
                hook_specific_output: Some(PreToolUseOutput {
                    hook_event_name: PreToolUseName::PreToolUse,
                    permission_decision: PermissionDecision::Deny,
                    permission_decision_reason: reason.clone(),
                }),
                ..Printed::default()
            },
        };
        Some(serde_json::json!(output).to_string())
    }

    /// The answer a hook for `event` printed: what [`Answer::json`] writes.
    fn parse(event: Event, printed: &str) -> anyhow::Result<Self> {
        let printed = printed.trim();
        if printed.is_empty() {
            return Ok(Self::Allow);
        }
        let output: Printed =
            serde_json::from_str(printed).context("the hook did not answer in its JSON")?;
        match (event, output) {
            (
                _,
                Printed {
                    system_message: Some(message),
                    decision: None,
                    reason: None,
                    hook_specific_output: None,
                },
            ) => Ok(Self::Notify(message)),
            (
                Event::Stop,
                Printed {
                    system_message: None,
                    decision: Some(StopDecision::Block),
                    reason: Some(reason),
                    hook_specific_output: None,
                },
            ) => Ok(Self::Block(reason)),
            (
                Event::PreToolUse,
                Printed {
                    system_message: None,
                    decision: None,
                    reason: None,
                    hook_specific_output: Some(denied),
                },
            ) => Ok(Self::Block(denied.permission_decision_reason)),
            (event, output) => bail!("a {} hook cannot answer {output:?}", event.name()),
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

/// Runs the hook for `event` on the hook input JSON and returns its answer, whatever goes wrong.
/// Notes the agent need not read go to `log`, which the hook prints on stderr.
pub(crate) fn run(event: Event, input: &str, context: &Context, log: &mut Vec<u8>) -> Answer {
    match event {
        Event::PreToolUse => match guard_where(input, context, log) {
            Ok(answer) => answer,
            Err(error) => guard_unavailable(&format!("{error:#}"), input),
        },
        // The watchdog's own session stops without a check, so it never starts another watchdog.
        Event::Stop if context.inside_watchdog => Answer::Allow,
        Event::Stop => stop_hook(input, context, log),
    }
}

/// The guard, in the checkout the agent is in.
fn guard_where(input: &str, context: &Context, log: &mut Vec<u8>) -> anyhow::Result<Answer> {
    let Ok(fields) = serde_json::from_str::<HookInput>(input) else {
        // The guard blocks input it cannot read and gives the parse error as the reason.
        return Ok(guard(input));
    };
    let declares = fields
        .tool_input
        .as_ref()
        .and_then(|tool| tool.command.as_deref())
        .is_some_and(declares_blocked);
    if fields.agent_id.is_some() && declares {
        return Ok(Answer::Block(String::from(MAIN_DECLARES)));
    }
    match locate(
        fields.cwd.as_deref(),
        context.project_dir.as_deref(),
        &context.own_root,
    )? {
        Location::Here => Ok(guard(input)),
        // GitHub is the gate, so a checkout that cannot take the hook is noted, never locked out.
        Location::Worktree(root) if !root.join(LAUNCHER).is_file() => Ok(note(
            log,
            &format!(
                "xtask hook: {} has no {LAUNCHER}; nothing is checked there",
                root.display()
            ),
        )),
        Location::Worktree(root) => {
            ensure!(
                !context.delegated,
                "{} was handed this hook, but the agent is in another checkout: {}",
                context.own_root.display(),
                root.display()
            );
            delegate(&context.cargo, &root, Event::PreToolUse, input)
        }
        Location::Elsewhere(_) => Ok(Answer::Allow),
    }
}

fn guard(input: &str) -> Answer {
    match guard::decide(input) {
        guard::Decision::Allow => Answer::Allow,
        guard::Decision::Block(message) => Answer::Block(message),
    }
}

/// When the guard cannot run, blocks a command that mentions git or gh and lets any other through
/// with a warning.
fn guard_unavailable(reason: &str, input: &str) -> Answer {
    let command = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(fields) => fields
            .pointer("/tool_input/command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(_) => Some(String::from("git")),
    };
    if command.as_deref().is_some_and(guard::mentions_git_or_gh) {
        Answer::Block(format!(
            "Blocked: the git guard could not run ({reason}), so git and gh commands are blocked \
             until it can. Commands that do not mention git or gh still run."
        ))
    } else {
        Answer::Notify(format!(
            "warning: the git guard could not run ({reason}); allowing a command that does not \
             mention git or gh"
        ))
    }
}

/// The stop hook: in the checkout the agent is in, handed over to that checkout's xtask when it is
/// another worktree. Input that cannot be read, a checkout that cannot be found and a hand-over
/// that fails all hold the stop as a check that failed, in the state of the checkout the agent is
/// in where it is known and this one otherwise, so the cap still ends the run.
fn stop_hook(input: &str, context: &Context, log: &mut Vec<u8>) -> Answer {
    let (fields, root, failure) = match serde_json::from_str::<HookInput>(input) {
        Err(error) => (
            HookInput::default(),
            context.own_root.clone(),
            Some(format!("could not read the Stop hook input: {error}")),
        ),
        Ok(fields) => {
            let location = locate(
                fields.cwd.as_deref(),
                context.project_dir.as_deref(),
                &context.own_root,
            );
            let (root, failure) = match location {
                Ok(Location::Here) => (context.own_root.clone(), None),
                Ok(Location::Worktree(root)) if !root.join(LAUNCHER).is_file() => {
                    return note(
                        log,
                        &format!(
                            "xtask hook: {} has no {LAUNCHER}; nothing is checked there",
                            root.display()
                        ),
                    );
                }
                Ok(Location::Worktree(root)) if context.delegated => {
                    let failure = format!(
                        "{} was handed this hook, but the agent is in another checkout: {}",
                        context.own_root.display(),
                        root.display()
                    );
                    (root, Some(failure))
                }
                Ok(Location::Worktree(root)) => {
                    match delegate(&context.cargo, &root, Event::Stop, input) {
                        Ok(answer) => return answer,
                        Err(error) => (root, Some(format!("{error:#}"))),
                    }
                }
                Ok(Location::Elsewhere(reason)) => {
                    return note(log, &format!("xtask hook: {reason}; no checks run"));
                }
                Err(error) => (context.own_root.clone(), Some(format!("{error:#}"))),
            };
            (fields, root, failure)
        }
    };
    let mut real = Real {
        root: &root,
        claude: &context.claude,
        failure,
    };
    stop_with(&root, &fields, &mut real, log)
}

/// The stop of the agent `fields` names, through `steps`. Without its state xtask cannot count held
/// stops, so a failure to keep it still runs the check, once, and says PASS or FAIL: a pass lets the
/// stop through with a note, and a failure is [`Answer::Held`], which the launcher counts under its
/// own cap.
fn stop_with(root: &Path, fields: &HookInput, steps: &mut impl Steps, log: &mut Vec<u8>) -> Answer {
    let mut remembered = Remembered { steps, check: None };
    let error = match drive(root, fields, &mut remembered, log) {
        Ok(answer) => return answer,
        Err(error) => error,
    };
    let mut said = Vec::new();
    let passed = remembered.check(&mut said).unwrap_or(false);
    let said = String::from_utf8_lossy(&said);
    let kept = format!(
        "xtask stop hook: it could not keep its state in {}: {error:#}",
        root.display()
    );
    if passed {
        Answer::Notify(format!(
            "{said}check: PASS. {kept}; this stop is allowed, and the drift watchdog did not run."
        ))
    } else {
        Answer::Held(format!("{said}check: FAIL. {kept}."))
    }
}

/// Steps that run the check once: asked again, they replay what it said, so a stop that fails
/// half way through never runs a long check twice.
struct Remembered<'a, S> {
    steps: &'a mut S,
    check: Option<(bool, Vec<u8>)>,
}

impl<S: Steps> Steps for Remembered<'_, S> {
    fn check(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        if let Some((passed, said)) = &self.check {
            out.extend_from_slice(said);
            return Ok(*passed);
        }
        let mut said = Vec::new();
        let passed = match self.steps.check(&mut said) {
            Ok(passed) => passed,
            Err(error) => {
                writeln!(said, "xtask stop hook: the check could not run: {error:#}")?;
                false
            }
        };
        out.extend_from_slice(&said);
        self.check = Some((passed, said));
        Ok(passed)
    }

    fn judge(&mut self, declaration: &str, out: &mut Vec<u8>) -> anyhow::Result<Ruling> {
        self.steps.judge(declaration, out)
    }

    fn watch(&mut self, out: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
        self.steps.watch(out)
    }
}

/// Writes `text` to `log` and lets the action go on.
fn note(log: &mut Vec<u8>, text: &str) -> Answer {
    log.extend_from_slice(text.as_bytes());
    log.push(b'\n');
    Answer::Allow
}

/// What the stop hook runs when a decision asks for it.
trait Steps {
    /// Whether `check` passes; what it prints goes to `out`.
    fn check(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool>;
    /// The judge's ruling on `declaration`.
    fn judge(&mut self, declaration: &str, out: &mut Vec<u8>) -> anyhow::Result<Ruling>;
    /// Why the drift watchdog keeps the agent working, if it does.
    fn watch(&mut self, out: &mut Vec<u8>) -> anyhow::Result<Option<String>>;
}

/// The real steps in the checkout at `root`: its pinned tools, the real check and the watchdog's
/// claude. With a `failure`, the check fails for that reason without running.
struct Real<'a> {
    root: &'a Path,
    claude: &'a Path,
    failure: Option<String>,
}

impl Steps for Real<'_> {
    fn check(&mut self, out: &mut Vec<u8>) -> anyhow::Result<bool> {
        if let Some(failure) = &self.failure {
            bail!("{failure}");
        }
        let mut command = Command::new(std::env::current_exe().context("could not find xtask")?);
        command
            .arg("--root")
            .arg(self.root)
            .args(["check", "--changed"])
            .current_dir(self.root);
        run_bounded(command, CHECK_DEADLINE, out)
    }

    fn judge(&mut self, declaration: &str, out: &mut Vec<u8>) -> anyhow::Result<Ruling> {
        let sent_back =
            watchdog::blocker(self.root, self.claude, watchdog::TIMEOUT, declaration, out)?;
        Ok(sent_back.map_or(Ruling::Outside, Ruling::SentBack))
    }

    fn watch(&mut self, out: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
        watchdog::check(self.root, self.claude, watchdog::TIMEOUT, out)
    }
}

/// Runs `command`, which prints its findings on stdout and stderr, for at most `deadline`: the
/// time it waits on other builds counts. What it printed goes to `out`. It passes when it exits 0.
/// At the deadline its whole process group is killed and the run fails, with the reason in `out`,
/// because Claude Code discards what a hook that times out printed.
fn run_bounded(
    mut command: Command,
    deadline: Duration,
    out: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    let (mut reader, writer) = std::io::pipe().context("could not make a pipe")?;
    command
        .stdin(Stdio::null())
        .stdout(writer.try_clone().context("could not share the pipe")?)
        .stderr(writer)
        .process_group(0);
    let mut child = command.spawn().context("could not run the check")?;
    // The command holds the pipe's write end until it is dropped, and the reader ends with it.
    drop(command);
    let printed = std::thread::spawn(move || {
        let mut printed = Vec::new();
        reader.read_to_end(&mut printed).map(|_| printed)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("could not wait for the check")? {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            // `-s KILL -- -PID` signals the group the check and its cargo children are in; the
            // check itself is killed too, should the group's signal fail.
            if let Err(error) = Command::new("kill")
                .args(["-s", "KILL", "--"])
                .arg(format!("-{}", child.id()))
                .status()
            {
                writeln!(
                    out,
                    "xtask stop hook: could not signal the check's group: {error}"
                )?;
            }
            child.kill().context("could not stop the check")?;
            child.wait().context("could not reap the check")?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let printed = printed
        .join()
        .map_err(|_panic| anyhow::anyhow!("the check's reader panicked"))?
        .context("could not read what the check printed")?;
    out.extend_from_slice(&printed);
    let Some(status) = status else {
        writeln!(
            out,
            "xtask stop hook: check did not finish in {} s; it was stopped, and counts as failed",
            deadline.as_secs()
        )?;
        return Ok(false);
    };
    Ok(status.success())
}

/// How to declare a task BLOCKED, for the main agent.
const DECLARE: &str = "If what stops you is outside your control, declare it: cargo xtask task \
                       blocked \"<what is missing, what was tried>\"";

/// Drives the stop of the agent `fields` names through the state machine of the checkout at
/// `root`, running each step a decision asks for, and answers with the decision it comes to.
fn drive(
    root: &Path,
    fields: &HookInput,
    steps: &mut impl Steps,
    log: &mut Vec<u8>,
) -> anyhow::Result<Answer> {
    let agent = fields.agent();
    let mut said = Vec::new();
    let mut decision = stop::apply(
        root,
        stop::Event::Stop {
            agent: agent.clone(),
            stop_hook_active: fields.stop_hook_active,
        },
        &mut said,
    )?;
    loop {
        let event = match decision {
            Decision::RunCheck => {
                let passed = match steps.check(&mut said) {
                    Ok(passed) => passed,
                    Err(error) => {
                        writeln!(said, "xtask stop hook: the check could not run: {error:#}")?;
                        false
                    }
                };
                stop::Event::Checked {
                    agent: agent.clone(),
                    passed,
                }
            }
            Decision::Judge(declaration) => {
                stop::Event::Judged(match steps.judge(&declaration, &mut said) {
                    Ok(ruling) => ruling,
                    Err(error) => Ruling::SentBack(format!("the judge could not run: {error:#}")),
                })
            }
            Decision::Watch => stop::Event::Watched(match steps.watch(&mut said) {
                Ok(drift) => drift,
                Err(error) => {
                    writeln!(said, "xtask watchdog: {error:#}; skipped")?;
                    None
                }
            }),
            Decision::Recorded
            | Decision::Refused(_)
            | Decision::Ignored
            | Decision::Allow
            | Decision::Honour { .. }
            | Decision::Backstop { .. }
            | Decision::Block(_) => return conclude(root, &agent, decision, &said, log),
        };
        decision = stop::apply(root, event, &mut said)?;
    }
}

/// The answer for the stop's final `decision`, after what its steps `said`, recorded in the logs
/// of the checkout at `root`. What a stop let through said goes to `log`.
fn conclude(
    root: &Path,
    agent: &Agent,
    decision: Decision,
    said: &[u8],
    log: &mut Vec<u8>,
) -> anyhow::Result<Answer> {
    let said = String::from_utf8_lossy(said).into_owned();
    let through = |sink: &mut Vec<u8>, answer: Answer| {
        sink.extend_from_slice(said.as_bytes());
        answer
    };
    Ok(match decision {
        Decision::Block(Block::CheckFailed) => Answer::Block(match agent {
            Agent::Main => {
                format!("{said}xtask stop hook: the check did not pass; keep working. {DECLARE}")
            }
            Agent::Sub(_) => {
                format!("{said}xtask stop hook: the check did not pass; keep working.")
            }
        }),
        Decision::Block(Block::SentBack {
            declaration,
            reason,
        }) => {
            stop::log_blocked(root, &declaration, &format!("sent back: {reason}"))?;
            Answer::Block(format!(
                "{said}xtask stop hook: the BLOCKED declaration was sent back: {reason}\nKeep \
                 working. If what stops you is truly outside your control, declare it again and \
                 say what makes it so."
            ))
        }
        Decision::Block(Block::Drifted(reason)) => Answer::Block(format!("{said}{reason}")),
        Decision::Honour {
            declaration,
            unjudged_after,
        } => {
            let outcome = match unjudged_after {
                None => String::from("stop allowed"),
                Some(count) => format!("stop allowed, not judged: {count} blocked stops in a row"),
            };
            stop::log_blocked(root, &declaration, &outcome)?;
            through(
                log,
                Answer::Notify(format!("BLOCKED, stop allowed. Handoff: {declaration}")),
            )
        }
        Decision::Backstop {
            agent: Agent::Main,
            count,
        } => {
            stop::record_backstop(root, count)?;
            through(
                log,
                Answer::Notify(format!(
                    "The check failed and {count} stops in a row were blocked with no BLOCKED \
                     declaration, so this stop is allowed; recorded in {}. Next time declare it: \
                     cargo xtask task blocked \"<what is missing, what was tried>\"",
                    stop::BACKSTOP
                )),
            )
        }
        Decision::Backstop {
            agent: Agent::Sub(_),
            count,
        } => through(
            log,
            Answer::Notify(format!(
                "A subagent's check failed and {count} of its stops in a row were blocked, so \
                 this stop is allowed."
            )),
        ),
        Decision::Allow => through(log, Answer::Allow),
        // Another hook changed the state between two steps of this one; the stop goes through
        // rather than wait on a state it cannot follow.
        Decision::Recorded
        | Decision::Refused(_)
        | Decision::Ignored
        | Decision::RunCheck
        | Decision::Judge(_)
        | Decision::Watch => {
            let answer = through(log, Answer::Allow);
            note(
                log,
                "xtask stop hook: the stop state changed under this hook; the stop is allowed",
            );
            answer
        }
    })
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

/// Hands the hook to the checkout at `root`, which builds and runs its own xtask with its own
/// toolchain, and reads its answer. An xtask that cannot start there is an error.
fn delegate(cargo: &Path, root: &Path, event: Event, input: &str) -> anyhow::Result<Answer> {
    let mut child = Command::new(cargo)
        .args(XTASK_HOOK)
        .arg(event.name())
        .current_dir(root)
        // Let the checkout's rust-toolchain.toml choose the toolchain, not the one running here.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env(DELEGATED, OsStr::new("1"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run cargo in {}", root.display()))?;
    tools::feed(&mut child, &mut input.as_bytes()).context("could not pass the hook input on")?;
    let output = child
        .wait_with_output()
        .context("could not wait for the delegated hook")?;
    if event == Event::Stop && output.status.code() == Some(i32::from(UNKEPT_STATUS)) {
        return Ok(Answer::Held(
            String::from_utf8(output.stdout).context("the delegated hook printed non-UTF-8")?,
        ));
    }
    ensure!(
        output.status.success(),
        "the xtask of {} could not run (it exited {})",
        root.display(),
        tools::status_code(output.status)
    );
    let printed =
        String::from_utf8(output.stdout).context("the delegated hook printed non-UTF-8")?;
    Answer::parse(event, &printed).with_context(|| {
        format!(
            "the xtask of {} could not run: it answered {printed:?}",
            root.display()
        )
    })
}

#[cfg(test)]
mod tests;
