//! The stop hook's decisions (D39): PASS, FAIL or BLOCKED, so a run that can never go green still
//! ends.
//!
//! [`decide`] is one pure function from the state and an event to a decision and the next state.
//! The hook (`crate::hook`) does the IO around it: it reads the hook input, runs `check` and the
//! watchdog when a decision asks for them, and feeds what they found back in as events. Each step
//! loads the state, decides and saves it whole under a lock ([`apply`]), so a hook that is killed
//! half way leaves a state the next stop starts from cleanly.
//!
//! The main agent and each subagent have a record of their own, and only the main agent's has a
//! BLOCKED declaration: [`Agent`] says whose stop an event is, so a subagent's stop cannot touch
//! the main agent's declaration or count.
//!
//! - The main agent's stop with a declaration asks the judge (the watchdog's `blocker`) whether
//!   what blocks it is outside its control. Outside: the stop goes through with the declaration
//!   as the handoff. Not: the stop is sent back and counts as a blocked stop. A declaration is
//!   taken by the stop that judges it, so it covers one stop.
//! - Any other stop runs `check`. A pass resets the count and lets the stop through, after the
//!   drift watchdog for the main agent's first stop since one was let through; a failure blocks
//!   it and counts.
//! - Once an agent has [`MAX_BLOCKED_STOPS`] blocked stops in a row, its next stop goes through
//!   whatever happens: with the declaration as the handoff, unjudged, if it has one, and
//!   otherwise as the backstop, which the main agent's log records.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// How many stops in a row an agent's stops are blocked before the next one is let through.
pub(crate) const MAX_BLOCKED_STOPS: u32 = 5;

/// The state of every agent's stops, relative to the worktree root; `.abc/` is ignored by git.
pub(crate) const STATE: &str = ".abc/stop-state.json";

/// The lock that keeps two hooks from changing the state at once.
const LOCK: &str = ".abc/stop-state.lock";

/// The kept log of every BLOCKED declaration and what the stop hook did with it.
pub(crate) const BLOCKED_LOG: &str = ".abc/blocked-log.md";

/// Where the stop hook records, kept, each time its backstop let the main agent's stop through.
pub(crate) const BACKSTOP: &str = ".abc/stop-backstop.md";

/// A subagent, by the `agent_id` Claude Code gives its `SubagentStop`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct AgentId(String);

impl AgentId {
    pub(crate) const fn new(id: String) -> Self {
        Self(id)
    }
}

/// Whose stop an event is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Agent {
    Main,
    Sub(AgentId),
}

/// Every agent's record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    main: Main,
    /// Subagents with a stop under way or blocked stops counted; one that was let through has no
    /// record.
    subagents: BTreeMap<AgentId, Sub>,
}

/// The main agent's record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Main {
    /// Its blocked stops in a row.
    blocked: u32,
    /// The BLOCKED declaration its next stop takes.
    declaration: Option<String>,
    /// A stop the drift watchdog was to judge found another gate running, so its check did not
    /// run and the watchdog was not asked: the next stop that runs `check` is watched in its
    /// place, though the hook sent this one back.
    unwatched: bool,
    round: MainRound,
}

/// Where the main agent's stop is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case", deny_unknown_fields)]
enum MainRound {
    /// No stop is under way.
    #[default]
    Idle,
    /// `check` runs; the drift watchdog judges a pass when `watched`.
    Checking { watched: bool },
    /// The judge rules on the declaration the stop took.
    Judging { declaration: String },
    /// The drift watchdog judges a stop whose check passed.
    Watching,
}

/// A subagent's record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sub {
    /// Its blocked stops in a row.
    blocked: u32,
    /// Whether its `check` runs.
    checking: bool,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// `cargo xtask task set`: a new task, which starts every record afresh; a check already under
    /// way keeps running, and its result still counts.
    TaskSet,
    /// `cargo xtask task blocked`: `agent` declares its task BLOCKED. Only the main agent may, and
    /// only one declaration waits for its stop at a time.
    Declared { agent: Agent, text: String },
    /// An agent stops. `stop_hook_active` is Claude Code's: the agent is working on because a
    /// stop hook kept it.
    Stop {
        agent: Agent,
        stop_hook_active: bool,
    },
    /// The agent's `check` ran, or found another gate running.
    Checked { agent: Agent, result: CheckResult },
    /// The judge ruled on the main agent's declaration.
    Judged(Ruling),
    /// The drift watchdog judged the main agent's passing stop: why it drifted, if it did.
    Watched(Option<String>),
}

/// What the stop's `check` came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Passed,
    /// It failed or could not run.
    Failed,
    /// Another gate held the gate lock (`crate::gate`), so it did not start.
    GateBusy,
}

/// What the judge rules on a BLOCKED declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ruling {
    /// What blocks the agent is outside its control.
    Outside,
    /// It is not, for this reason; a judge that could not rule sends it back too.
    SentBack(String),
}

/// What the hook does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The state took the event; there is nothing to do.
    Recorded,
    /// The event was refused, for this reason, and nothing changed.
    Refused(String),
    /// The event does not fit where the agent's stop is, so nothing changed.
    Ignored,
    /// Run `check` for the agent that stops, then send [`Event::Checked`].
    RunCheck,
    /// Ask the judge about this declaration, then send [`Event::Judged`].
    Judge(String),
    /// Ask the drift watchdog, then send [`Event::Watched`].
    Watch,
    /// Let the stop through.
    Allow,
    /// Let the main agent's stop through with its declaration as the handoff: judged outside its
    /// control, or, with `unjudged_after`, unjudged after that many blocked stops.
    Honour {
        declaration: String,
        unjudged_after: Option<u32>,
    },
    /// Let the stop through after `count` blocked stops in a row.
    Backstop { agent: Agent, count: u32 },
    /// Keep the agent working.
    Block(Block),
}

/// Why a stop is blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    /// `check` failed or could not run.
    CheckFailed,
    /// Another gate held the gate lock all the while the hook waited for it (`hook::GATE_WAIT`),
    /// so `check` did not start: the agent waits for it. Counted like a failure, so a gate that
    /// never ends cannot hold the agent forever; the wait keeps a long gate from using up the
    /// count in seconds.
    GateBusy,
    /// The judge sent the declaration back.
    SentBack { declaration: String, reason: String },
    /// The drift watchdog found the work off its task.
    Drifted(String),
}

/// The decision on `event` in `state`, and the state after it.
pub(crate) fn decide(mut state: State, event: Event) -> (Decision, State) {
    let decision = match event {
        Event::TaskSet => {
            state = state.restarted();
            Decision::Recorded
        }
        Event::Declared {
            agent: Agent::Sub(_),
            ..
        } => Decision::Refused(String::from(
            "only the main agent declares a task BLOCKED; a subagent finishes its work or reports \
             what stops it to the main agent",
        )),
        Event::Declared {
            agent: Agent::Main,
            text,
        } => state.main.declare(text),
        Event::Stop {
            agent: Agent::Main,
            stop_hook_active,
        } => state.main.stop(stop_hook_active),
        Event::Stop {
            agent: Agent::Sub(id),
            ..
        } => {
            state.subagents.entry(id).or_default().checking = true;
            Decision::RunCheck
        }
        Event::Checked {
            agent: Agent::Main,
            result,
        } => state.main.checked(result),
        Event::Checked {
            agent: Agent::Sub(id),
            result,
        } => sub_checked(&mut state.subagents, id, result),
        Event::Judged(ruling) => state.main.judged(ruling),
        Event::Watched(drift) => state.main.watched(drift),
    };
    (decision, state)
}

impl State {
    /// The state after a new task: every count and declaration gone, but the checks and watching
    /// under way kept, so a stop whose check fails is not let through because the task changed.
    fn restarted(self) -> Self {
        let round = match self.main.round {
            round @ (MainRound::Checking { .. } | MainRound::Watching) => round,
            MainRound::Idle | MainRound::Judging { .. } => MainRound::Idle,
        };
        Self {
            main: Main {
                round,
                // The watchdog still owes the work a look.
                unwatched: self.main.unwatched,
                ..Main::default()
            },
            subagents: self
                .subagents
                .into_iter()
                .filter(|(_, sub)| sub.checking)
                .map(|(id, _)| {
                    (
                        id,
                        Sub {
                            blocked: 0,
                            checking: true,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl Main {
    /// Takes `text` as the declaration the next stop judges, unless one already waits for its
    /// stop or is being judged: dropping either would lose a handoff.
    fn declare(&mut self, text: String) -> Decision {
        if self.declaration.is_some() || matches!(self.round, MainRound::Judging { .. }) {
            return Decision::Refused(String::from(
                "a BLOCKED declaration already waits for its stop to be judged; stop, and declare \
                 again if it is sent back",
            ));
        }
        self.declaration = Some(text);
        Decision::Recorded
    }

    fn stop(&mut self, stop_hook_active: bool) -> Decision {
        // A judging the last hook did not finish leaves its declaration to this stop.
        let unjudged = match std::mem::take(&mut self.round) {
            MainRound::Judging { declaration } => Some(declaration),
            MainRound::Idle | MainRound::Checking { .. } | MainRound::Watching => None,
        };
        match self.declaration.take().or(unjudged) {
            Some(declaration) if self.blocked >= MAX_BLOCKED_STOPS => {
                let count = std::mem::take(&mut self.blocked);
                Decision::Honour {
                    declaration,
                    unjudged_after: Some(count),
                }
            }
            Some(declaration) => {
                self.round = MainRound::Judging {
                    declaration: declaration.clone(),
                };
                Decision::Judge(declaration)
            }
            None => {
                // Only the first stop since one was let through: a stop the hook sent back, or
                // whose run of blocks continues, is never judged for drift again; unless a busy
                // gate kept the watchdog from judging the one that was to be.
                self.round = MainRound::Checking {
                    watched: (!stop_hook_active && self.blocked == 0)
                        || std::mem::take(&mut self.unwatched),
                };
                Decision::RunCheck
            }
        }
    }

    fn checked(&mut self, result: CheckResult) -> Decision {
        let MainRound::Checking { watched } = self.round else {
            return Decision::Ignored;
        };
        self.round = MainRound::Idle;
        match result {
            CheckResult::Passed => {
                self.blocked = 0;
                if watched {
                    self.round = MainRound::Watching;
                    return Decision::Watch;
                }
                Decision::Allow
            }
            CheckResult::Failed => self.block(Block::CheckFailed, Agent::Main),
            CheckResult::GateBusy => {
                self.unwatched |= watched;
                self.block(Block::GateBusy, Agent::Main)
            }
        }
    }

    fn judged(&mut self, ruling: Ruling) -> Decision {
        let MainRound::Judging { declaration } = std::mem::take(&mut self.round) else {
            return Decision::Ignored;
        };
        match ruling {
            Ruling::Outside => {
                self.blocked = 0;
                Decision::Honour {
                    declaration,
                    unjudged_after: None,
                }
            }
            Ruling::SentBack(reason) => self.block(
                Block::SentBack {
                    declaration,
                    reason,
                },
                Agent::Main,
            ),
        }
    }

    fn watched(&mut self, drift: Option<String>) -> Decision {
        if self.round != MainRound::Watching {
            return Decision::Ignored;
        }
        self.round = MainRound::Idle;
        match drift {
            None => Decision::Allow,
            Some(reason) => self.block(Block::Drifted(reason), Agent::Main),
        }
    }

    /// Blocks the stop and counts it, or lets it through once the count is at the cap.
    fn block(&mut self, why: Block, agent: Agent) -> Decision {
        if self.blocked >= MAX_BLOCKED_STOPS {
            let count = std::mem::take(&mut self.blocked);
            // The stop goes through, and the next one is a first stop, which is watched anyway.
            self.unwatched = false;
            return Decision::Backstop { agent, count };
        }
        self.blocked = self.blocked.saturating_add(1);
        Decision::Block(why)
    }
}

fn sub_checked(
    subagents: &mut BTreeMap<AgentId, Sub>,
    id: AgentId,
    result: CheckResult,
) -> Decision {
    let Some(sub) = subagents.get_mut(&id).filter(|sub| sub.checking) else {
        return Decision::Ignored;
    };
    sub.checking = false;
    let why = match result {
        CheckResult::Passed => {
            subagents.remove(&id);
            return Decision::Allow;
        }
        CheckResult::Failed => Block::CheckFailed,
        CheckResult::GateBusy => Block::GateBusy,
    };
    if sub.blocked >= MAX_BLOCKED_STOPS {
        let count = sub.blocked;
        subagents.remove(&id);
        return Decision::Backstop {
            agent: Agent::Sub(id),
            count,
        };
    }
    sub.blocked = sub.blocked.saturating_add(1);
    Decision::Block(why)
}

/// Decides `event` on the state of the worktree at `root` and saves the state it leads to, under
/// the state's lock. A state file that cannot be read is replaced by a fresh state, and `note`
/// says so: holding every stop on a broken file would be the loop the cap exists to prevent.
pub(crate) fn apply(
    root: &Path,
    event: Event,
    note: &mut dyn std::io::Write,
) -> anyhow::Result<Decision> {
    let lock_path = root.join(LOCK);
    create_parent(&lock_path)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("could not lock {}", lock_path.display()))?;
    let state = match load(root) {
        Ok(state) => state,
        Err(error) => {
            writeln!(
                note,
                "xtask stop hook: {error:#}; starting from a fresh state"
            )?;
            State::default()
        }
    };
    let (decision, state) = decide(state, event);
    save(root, &state)?;
    Ok(decision)
}

/// The state saved at `root`; a fresh one when there is none.
fn load(root: &Path) -> anyhow::Result<State> {
    let path = root.join(STATE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not a stop state", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Writes `state` atomically: a temporary file beside it, renamed over it, so a reader never sees
/// half a file.
fn save(root: &Path, state: &State) -> anyhow::Result<()> {
    let path = root.join(STATE);
    create_parent(&path)?;
    let temporary = root.join(format!("{STATE}.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(state).context("could not encode the stop state")?;
    fs::write(&temporary, format!("{text}\n"))
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not move {} into place", temporary.display()))
}

/// Creates the directory `path` is in.
fn create_parent(path: &Path) -> anyhow::Result<()> {
    match path.parent() {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))
        }
        None => Ok(()),
    }
}

/// Appends `text` to `file`, creating it, as one write.
fn append(root: &Path, file: &str, text: &str) -> anyhow::Result<()> {
    let path = root.join(file);
    create_parent(&path)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut log| log.write_all(text.as_bytes()))
        .with_context(|| format!("could not append to {}", path.display()))
}

/// A log entry's heading: the time in seconds since the Unix epoch and what happened.
fn entry(what: &str) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    format!("\n## Unix time {seconds}: {what}\n\n")
}

/// Appends a BLOCKED declaration and what the stop hook did with it to the kept log.
pub(crate) fn log_blocked(root: &Path, declaration: &str, outcome: &str) -> anyhow::Result<()> {
    append(
        root,
        BLOCKED_LOG,
        &format!("{}{declaration}\n", entry(outcome)),
    )
}

/// Appends to the kept record that the backstop let the main agent's stop through after `count`
/// blocked ones.
pub(crate) fn record_backstop(root: &Path, count: u32) -> anyhow::Result<()> {
    append(
        root,
        BACKSTOP,
        &format!(
            "{}The stop hook let a stop through after {count} blocked stops in a row, with no \
             BLOCKED declaration.\n",
            entry("backstop")
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};

    use super::{
        Agent, AgentId, Block, CheckResult, Decision, Event, MAX_BLOCKED_STOPS, Main, MainRound,
        Ruling, State, apply, decide,
    };

    fn sub(id: &str) -> Agent {
        Agent::Sub(AgentId::new(id.to_owned()))
    }

    fn declare(text: String) -> Event {
        Event::Declared {
            agent: Agent::Main,
            text,
        }
    }

    fn stop(agent: Agent) -> Event {
        Event::Stop {
            agent,
            stop_hook_active: false,
        }
    }

    /// The decisions of `events` in turn, from a fresh state.
    fn run(events: Vec<Event>) -> (Vec<Decision>, State) {
        let mut state = State::default();
        let mut decisions = Vec::new();
        for event in events {
            let (decision, next) = decide(state, event);
            decisions.push(decision);
            state = next;
        }
        (decisions, state)
    }

    #[test]
    fn a_passing_first_stop_is_watched_and_let_through() {
        let (decisions, state) = run(vec![
            stop(Agent::Main),
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Passed,
            },
            Event::Watched(None),
        ]);
        assert_eq!(
            decisions,
            [Decision::RunCheck, Decision::Watch, Decision::Allow],
            "check, then the watchdog, then through"
        );
        assert_eq!(state, State::default(), "nothing left over");
    }

    #[test]
    fn a_stop_the_hook_sent_back_is_not_watched() {
        let (decisions, _) = run(vec![
            Event::Stop {
                agent: Agent::Main,
                stop_hook_active: true,
            },
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Passed,
            },
        ]);
        assert_eq!(
            decisions,
            [Decision::RunCheck, Decision::Allow],
            "no watchdog"
        );
    }

    #[test]
    fn drift_blocks_once_and_counts() {
        let (decisions, state) = run(vec![
            stop(Agent::Main),
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Passed,
            },
            Event::Watched(Some("off task".to_owned())),
            stop(Agent::Main),
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Passed,
            },
        ]);
        assert_eq!(
            decisions,
            [
                Decision::RunCheck,
                Decision::Watch,
                Decision::Block(Block::Drifted("off task".to_owned())),
                Decision::RunCheck,
                Decision::Allow,
            ],
            "the next stop, even with stop_hook_active unset, is not judged again"
        );
        assert_eq!(state.main.blocked, 0, "the pass reset the count");
    }

    #[test]
    fn failing_stops_are_blocked_until_the_cap_and_the_next_goes_through() {
        let mut events = Vec::new();
        for _ in 0..=MAX_BLOCKED_STOPS {
            events.push(stop(Agent::Main));
            events.push(Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Failed,
            });
        }
        let (decisions, state) = run(events);
        let ends: Vec<&Decision> = decisions.iter().skip(1).step_by(2).collect();
        let blocked = Decision::Block(Block::CheckFailed);
        assert_eq!(
            ends,
            [
                &blocked,
                &blocked,
                &blocked,
                &blocked,
                &blocked,
                &Decision::Backstop {
                    agent: Agent::Main,
                    count: MAX_BLOCKED_STOPS
                }
            ],
            "five held, the sixth through"
        );
        assert_eq!(state.main.blocked, 0, "the backstop resets the count");
    }

    /// PR 5a: the stop hook's `check` started while the agent's `coverage` ran. Now a stop that
    /// finds another gate running is sent back to wait for it, and counts, so a gate that never
    /// ends cannot keep the agent from stopping forever.
    #[test]
    fn a_stop_while_another_gate_runs_is_sent_back_and_counts() {
        for agent in [Agent::Main, sub("a1")] {
            let mut events = Vec::new();
            for _ in 0..=MAX_BLOCKED_STOPS {
                events.push(stop(agent.clone()));
                events.push(Event::Checked {
                    agent: agent.clone(),
                    result: CheckResult::GateBusy,
                });
            }
            let (decisions, state) = run(events);
            let ends: Vec<&Decision> = decisions.iter().skip(1).step_by(2).collect();
            let waiting = Decision::Block(Block::GateBusy);
            assert_eq!(
                ends,
                [
                    &waiting,
                    &waiting,
                    &waiting,
                    &waiting,
                    &waiting,
                    &Decision::Backstop {
                        agent: agent.clone(),
                        count: MAX_BLOCKED_STOPS
                    }
                ],
                "{agent:?}: sent back to wait five times, the sixth through"
            );
            assert_eq!(state, State::default(), "{agent:?}: nothing left over");
        }
        let (decisions, state) = run(vec![
            stop(Agent::Main),
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::GateBusy,
            },
            stop(Agent::Main),
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Passed,
            },
            Event::Watched(None),
        ]);
        assert_eq!(
            decisions[decisions.len() - 2..],
            [Decision::Watch, Decision::Allow],
            "once the gate is done, a passing check goes to the drift watchdog the busy stop never \
             reached, though the hook sent that one back, and then through: {decisions:?}"
        );
        assert_eq!(state.main.blocked, 0, "and the wait no longer counts");
    }

    #[test]
    fn a_declaration_is_judged_and_honoured_when_outside_control() {
        let (decisions, state) = run(vec![
            declare("no network".to_owned()),
            stop(Agent::Main),
            Event::Judged(Ruling::Outside),
        ]);
        assert_eq!(
            decisions,
            [
                Decision::Recorded,
                Decision::Judge("no network".to_owned()),
                Decision::Honour {
                    declaration: "no network".to_owned(),
                    unjudged_after: None
                },
            ],
            "judged, then honoured"
        );
        assert_eq!(state, State::default(), "taken");
    }

    #[test]
    fn a_refused_declaration_counts_and_is_gone() {
        let (decisions, state) = run(vec![
            declare("the check fails".to_owned()),
            stop(Agent::Main),
            Event::Judged(Ruling::SentBack("fix it".to_owned())),
            stop(Agent::Main),
        ]);
        assert_eq!(
            decisions.get(2),
            Some(&Decision::Block(Block::SentBack {
                declaration: "the check fails".to_owned(),
                reason: "fix it".to_owned()
            })),
            "sent back with the reason"
        );
        assert_eq!(
            decisions.get(3),
            Some(&Decision::RunCheck),
            "the next stop runs the check: the declaration covered one stop"
        );
        assert_eq!(state.main.blocked, 1, "counted");
    }

    #[test]
    fn at_the_cap_a_declaration_goes_through_unjudged() {
        let mut events = Vec::new();
        for _ in 0..MAX_BLOCKED_STOPS {
            events.push(stop(Agent::Main));
            events.push(Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Failed,
            });
        }
        events.push(declare("stuck".to_owned()));
        events.push(stop(Agent::Main));
        let (decisions, state) = run(events);
        assert_eq!(
            decisions.last(),
            Some(&Decision::Honour {
                declaration: "stuck".to_owned(),
                unjudged_after: Some(MAX_BLOCKED_STOPS)
            }),
            "no judge at the cap"
        );
        assert_eq!(state, State::default(), "taken and reset");
    }

    #[test]
    fn a_judging_left_unfinished_goes_to_the_next_stop() {
        let (decisions, _) = run(vec![
            declare("no network".to_owned()),
            stop(Agent::Main),
            stop(Agent::Main),
        ]);
        assert_eq!(
            decisions.last(),
            Some(&Decision::Judge("no network".to_owned())),
            "a hook killed while the judge ran loses no declaration"
        );
    }

    #[test]
    fn events_out_of_turn_change_nothing() {
        for event in [
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Failed,
            },
            Event::Checked {
                agent: sub("a"),
                result: CheckResult::Failed,
            },
            Event::Judged(Ruling::Outside),
            Event::Watched(Some("drift".to_owned())),
        ] {
            let (decision, state) = decide(State::default(), event.clone());
            assert_eq!(decision, Decision::Ignored, "{event:?}");
            assert_eq!(state, State::default(), "{event:?}");
        }
    }

    #[test]
    fn a_subagent_counts_apart_and_is_let_through_after_the_cap() {
        let mut events = vec![declare("the main agent's".to_owned())];
        for _ in 0..=MAX_BLOCKED_STOPS {
            events.push(stop(sub("a")));
            events.push(Event::Checked {
                agent: sub("a"),
                result: CheckResult::Failed,
            });
        }
        let (decisions, state) = run(events);
        assert_eq!(
            decisions.last(),
            Some(&Decision::Backstop {
                agent: sub("a"),
                count: MAX_BLOCKED_STOPS
            }),
            "the sixth goes through"
        );
        assert_eq!(
            state.main.declaration.as_deref(),
            Some("the main agent's"),
            "the main agent's declaration is untouched"
        );
        assert!(state.subagents.is_empty(), "and the subagent's record gone");
    }

    #[test]
    fn a_new_task_starts_afresh() {
        let (_, state) = run(vec![
            declare("x".to_owned()),
            stop(sub("a")),
            Event::Checked {
                agent: sub("a"),
                result: CheckResult::Failed,
            },
            Event::TaskSet,
        ]);
        assert_eq!(state, State::default(), "every record");
    }

    #[test]
    fn only_a_missing_state_is_fresh_and_one_that_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            super::load(dir.path()).unwrap(),
            State::default(),
            "no file, no state yet"
        );
        std::fs::create_dir_all(dir.path().join(super::STATE)).unwrap();
        let error = format!("{:#}", super::load(dir.path()).unwrap_err());
        assert!(error.contains("could not read"), "{error}");
    }

    #[test]
    fn the_state_is_saved_whole_and_a_broken_file_starts_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut note = Vec::new();
        assert_eq!(
            apply(dir.path(), declare("stuck".to_owned()), &mut note).unwrap(),
            Decision::Recorded
        );
        assert_eq!(
            apply(dir.path(), stop(Agent::Main), &mut note).unwrap(),
            Decision::Judge("stuck".to_owned()),
            "the declaration was saved"
        );
        let files: BTreeSet<String> = std::fs::read_dir(dir.path().join(".abc"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            files,
            BTreeSet::from(["stop-state.json".to_owned(), "stop-state.lock".to_owned()]),
            "renamed into place, no temporary file left"
        );
        assert!(note.is_empty(), "nothing to say");
        std::fs::write(dir.path().join(super::STATE), "garbled").unwrap();
        assert_eq!(
            apply(dir.path(), stop(Agent::Main), &mut note).unwrap(),
            Decision::RunCheck,
            "a broken state is replaced"
        );
        let said = String::from_utf8(note).unwrap();
        assert!(said.contains("is not a stop state"), "{said}");
    }

    #[test]
    fn the_logs_are_appended() {
        let dir = tempfile::tempdir().unwrap();
        super::log_blocked(dir.path(), "one", "stop allowed").unwrap();
        super::log_blocked(dir.path(), "two", "sent back: no").unwrap();
        let log = std::fs::read_to_string(dir.path().join(super::BLOCKED_LOG)).unwrap();
        assert!(log.contains(": stop allowed\n\none\n"), "{log}");
        assert!(log.contains(": sent back: no\n\ntwo\n"), "{log}");
        super::record_backstop(dir.path(), 5).unwrap();
        let record = std::fs::read_to_string(dir.path().join(super::BACKSTOP)).unwrap();
        assert!(
            record.contains("after 5 blocked stops in a row"),
            "{record}"
        );
    }

    /// An event of a random sequence. Declarations are numbered, so each is told apart.
    #[derive(Debug, Clone)]
    enum Step {
        TaskSet,
        Declare { agent: u8 },
        Stop { agent: u8, active: bool },
        Checked { agent: u8, result: CheckResult },
        Judged { outside: bool },
        Watched { drifted: bool },
    }

    fn agent(index: u8) -> Agent {
        match index {
            0 => Agent::Main,
            other => sub(&format!("sub-{other}")),
        }
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        let step = prop_oneof![
            1 => Just(Step::TaskSet),
            2 => (0..3_u8).prop_map(|agent| Step::Declare { agent }),
            6 => (0..3_u8, any::<bool>()).prop_map(|(agent, active)| Step::Stop { agent, active }),
            8 => (0..3_u8, prop_oneof![
                3 => Just(CheckResult::Passed),
                5 => Just(CheckResult::Failed),
                2 => Just(CheckResult::GateBusy),
            ])
                .prop_map(|(agent, result)| Step::Checked { agent, result }),
            3 => any::<bool>().prop_map(|outside| Step::Judged { outside }),
            2 => any::<bool>().prop_map(|drifted| Step::Watched { drifted }),
        ];
        prop::collection::vec(step, 1..120)
    }

    /// A rule for deciding: [`decide`], or a reimplementation of an old one.
    type Rule = fn(State, Event) -> (Decision, State);

    /// Whether a decision lets the stop through.
    fn lets_through(decision: &Decision) -> bool {
        matches!(
            decision,
            Decision::Allow | Decision::Honour { .. } | Decision::Backstop { .. }
        )
    }

    /// The event of `step`, with the index of the agent whose stop it is part of, if any.
    /// `declared` numbers the declarations.
    fn event_of(step: &Step, declared: &mut u32) -> (Event, Option<u8>) {
        match step {
            Step::TaskSet => (Event::TaskSet, None),
            Step::Declare { agent: index } => {
                *declared = declared.saturating_add(1);
                (
                    Event::Declared {
                        agent: agent(*index),
                        text: format!("declaration {declared}"),
                    },
                    None,
                )
            }
            Step::Stop {
                agent: index,
                active,
            } => (
                Event::Stop {
                    agent: agent(*index),
                    stop_hook_active: *active,
                },
                Some(*index),
            ),
            Step::Checked {
                agent: index,
                result,
            } => (
                Event::Checked {
                    agent: agent(*index),
                    result: *result,
                },
                Some(*index),
            ),
            Step::Judged { outside } => (
                Event::Judged(if *outside {
                    Ruling::Outside
                } else {
                    Ruling::SentBack("not outside".to_owned())
                }),
                Some(0),
            ),
            Step::Watched { drifted } => (
                Event::Watched(drifted.then(|| "drifted".to_owned())),
                Some(0),
            ),
        }
    }

    /// What the invariants follow across a sequence.
    #[derive(Default)]
    struct Watch {
        declared: u32,
        honoured: BTreeSet<String>,
        /// Blocked stops in a row, by agent, since the last let through or new task.
        held: BTreeMap<u8, u32>,
        /// The agent whose stop the latest decision ends.
        stopping: Option<u8>,
        /// A main stop the watchdog was to judge found another gate running.
        owed: bool,
    }

    /// Checks that a declaration is kept, or refused and changes nothing.
    fn check_declaration(
        event: &Event,
        decision: &Decision,
        (before, after): (&State, &State),
    ) -> Result<(), TestCaseError> {
        if let Event::Declared { agent, text } = event {
            let waiting = before.main.declaration.is_some()
                || matches!(before.main.round, MainRound::Judging { .. });
            if *agent == Agent::Main && !waiting {
                prop_assert_eq!(decision, &Decision::Recorded, "a declaration was refused");
                prop_assert_eq!(
                    after.main.declaration.as_ref(),
                    Some(text),
                    "a declaration was not kept"
                );
            } else {
                prop_assert!(
                    matches!(decision, Decision::Refused(_)),
                    "a subagent's or a second declaration was taken: {:?}",
                    decision
                );
                prop_assert_eq!(after, before, "a refused declaration changed the state");
            }
        }
        Ok(())
    }

    /// Checks that a busy gate never turns the watchdog off: after a main stop that was to be
    /// watched found another gate running, the next main stop that runs `check` is watched,
    /// whatever `stop_hook_active` and the count say.
    fn check_owed_watch(
        watch: &mut Watch,
        event: &Event,
        decision: &Decision,
        (before, after): (&State, &State),
    ) -> Result<(), TestCaseError> {
        let busy = Event::Checked {
            agent: Agent::Main,
            result: CheckResult::GateBusy,
        };
        if *event == busy && before.main.round == (MainRound::Checking { watched: true }) {
            watch.owed = true;
        }
        let checks = matches!(
            event,
            Event::Stop {
                agent: Agent::Main,
                ..
            }
        ) && *decision == Decision::RunCheck;
        if checks && std::mem::take(&mut watch.owed) {
            prop_assert_eq!(
                &after.main.round,
                &MainRound::Checking { watched: true },
                "a busy gate turned the watchdog off for the next stop"
            );
        }
        Ok(())
    }

    /// Checks the invariants on one `event` that `rule` decided as `decision`, from `before` to
    /// `after`.
    fn check_step(
        watch: &mut Watch,
        event: &Event,
        decision: &Decision,
        (before, after): (&State, &State),
    ) -> Result<(), TestCaseError> {
        if watch.stopping.is_some_and(|index| index != 0)
            && matches!(event, Event::Stop { .. } | Event::Checked { .. })
        {
            prop_assert_eq!(
                &after.main,
                &before.main,
                "a subagent's event changed the main agent's state: {:?}",
                event
            );
        }
        if *event == Event::TaskSet {
            prop_assert_eq!(after.main.blocked, 0, "a new task resets the count");
            for (id, before_sub) in &before.subagents {
                prop_assert!(
                    !before_sub.checking
                        || after.subagents.get(id).is_some_and(|kept| kept.checking),
                    "a new task wiped the check of {:?}",
                    id
                );
            }
            if matches!(
                before.main.round,
                MainRound::Checking { .. } | MainRound::Watching
            ) {
                prop_assert_eq!(
                    &after.main.round,
                    &before.main.round,
                    "a new task wiped the main agent's check"
                );
            }
            watch.held.clear();
        }
        check_declaration(event, decision, (before, after))?;
        check_owed_watch(watch, event, decision, (before, after))?;
        if let Event::Checked {
            result: CheckResult::Failed | CheckResult::GateBusy,
            ..
        } = event
        {
            prop_assert!(
                *decision != Decision::Allow,
                "a failed check, or one another gate kept from starting, let the stop through"
            );
        }
        if let Event::Checked {
            agent: Agent::Main,
            result: CheckResult::Passed,
        } = event
            && *decision != Decision::Ignored
        {
            prop_assert_eq!(after.main.blocked, 0, "a pass resets the count");
        }
        if let Decision::Honour {
            declaration,
            unjudged_after,
        } = decision
        {
            prop_assert!(
                watch.honoured.insert(declaration.clone()),
                "{} was honoured twice",
                declaration
            );
            match unjudged_after {
                None => prop_assert_eq!(
                    event,
                    &Event::Judged(Ruling::Outside),
                    "honoured without the judge"
                ),
                Some(count) => prop_assert!(
                    *count >= MAX_BLOCKED_STOPS,
                    "honoured unjudged before the cap"
                ),
            }
            prop_assert!(
                watch.stopping == Some(0),
                "a declaration honoured for a subagent's stop"
            );
        }
        if let Some(who) = watch.stopping {
            let count = watch.held.entry(who).or_default();
            if lets_through(decision) {
                *count = 0;
            } else if matches!(decision, Decision::Block(_)) {
                *count = count.saturating_add(1);
                prop_assert!(
                    *count <= MAX_BLOCKED_STOPS,
                    "agent {} was held {} times in a row",
                    who,
                    count
                );
            }
        }
        Ok(())
    }

    /// Runs `steps` through `rule` and checks the invariants after every one.
    fn invariants_hold(rule: Rule, steps: &[Step]) -> Result<(), TestCaseError> {
        let mut state = State::default();
        let mut watch = Watch::default();
        for step in steps {
            let (event, index) = event_of(step, &mut watch.declared);
            if index.is_some() {
                watch.stopping = index;
            }
            let before = state.clone();
            let (decision, after) = rule(state, event.clone());
            check_step(&mut watch, &event, &decision, (&before, &after))?;
            state = after;
        }
        Ok(())
    }

    fn config() -> Config {
        Config {
            cases: 512,
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::SourceParallel(
                    "proptest-regressions",
                ),
            )),
            ..Config::default()
        }
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn every_event_sequence_keeps_the_invariants(steps in steps()) {
            invariants_hold(decide, &steps)?;
        }
    }

    /// The rule before 802ce11: a subagent's failing stop was held with no cap.
    fn uncapped_subagents(state: State, event: Event) -> (Decision, State) {
        let subagent = matches!(
            event,
            Event::Checked {
                agent: Agent::Sub(_),
                result: CheckResult::Failed
            }
        );
        let before = state.clone();
        let (decision, mut after) = decide(state, event.clone());
        match (decision, event) {
            (
                Decision::Backstop { .. },
                Event::Checked {
                    agent: Agent::Sub(id),
                    ..
                },
            ) if subagent => {
                let mut held = before.subagents.get(&id).cloned().unwrap_or_default();
                held.checking = false;
                held.blocked = held.blocked.saturating_add(1);
                after.subagents.insert(id, held);
                (Decision::Block(Block::CheckFailed), after)
            }
            (decision, _) => (decision, after),
        }
    }

    /// The rule before f4e9492: a subagent's stop used the main agent's state, its declaration
    /// and its count.
    fn shared_state(state: State, mut event: Event) -> (Decision, State) {
        if let Event::Stop { agent, .. } | Event::Checked { agent, .. } = &mut event {
            *agent = Agent::Main;
        }
        decide(state, event)
    }

    /// The rule before this fix: a new task wiped the checks under way, so their results were
    /// ignored and the stops let through.
    fn wiping_task_set(state: State, event: Event) -> (Decision, State) {
        if event == Event::TaskSet {
            return (Decision::Recorded, State::default());
        }
        decide(state, event)
    }

    /// The rule before this fix: every declaration replaced the one waiting, whoever made it.
    fn replacing_declarations(state: State, event: Event) -> (Decision, State) {
        match event {
            Event::Declared { text, .. } => {
                let mut state = state;
                state.main.declaration = Some(text);
                (Decision::Recorded, state)
            }
            event @ (Event::TaskSet
            | Event::Stop { .. }
            | Event::Checked { .. }
            | Event::Judged(_)
            | Event::Watched(_)) => decide(state, event),
        }
    }

    /// The minimal sequence on which `rule` breaks an invariant, found from a fixed seed.
    fn counterexample(rule: Rule) -> Option<(Vec<Step>, String)> {
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: 512,
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &[7; 32]),
        );
        match runner.run(&steps(), |steps| invariants_hold(rule, &steps)) {
            Ok(()) => None,
            Err(proptest::test_runner::TestError::Fail(reason, steps)) => {
                Some((steps, reason.to_string()))
            }
            Err(proptest::test_runner::TestError::Abort(reason)) => {
                Some((Vec::new(), format!("aborted: {reason}")))
            }
        }
    }

    #[test]
    fn the_invariants_catch_a_subagent_held_for_ever() {
        let (steps, reason) =
            counterexample(uncapped_subagents).expect("the old rule must break an invariant");
        assert!(reason.contains("times in a row"), "{reason}: {steps:?}");
        assert!(
            steps
                .iter()
                .all(|step| !matches!(step, Step::TaskSet | Step::Declare { .. })),
            "the minimal case is a subagent's stops alone: {steps:?}"
        );
    }

    #[test]
    fn the_invariants_catch_state_shared_with_subagents() {
        let (steps, reason) =
            counterexample(shared_state).expect("the old rule must break an invariant");
        assert!(
            reason.contains("changed the main agent's state"),
            "{reason}: {steps:?}"
        );
    }

    #[test]
    fn the_invariants_catch_a_new_task_that_wipes_a_check() {
        let (steps, reason) =
            counterexample(wiping_task_set).expect("the old rule must break an invariant");
        assert!(reason.contains("wiped"), "{reason}: {steps:?}");
    }

    #[test]
    fn the_invariants_catch_declarations_that_replace_each_other() {
        let (steps, reason) =
            counterexample(replacing_declarations).expect("the old rule must break an invariant");
        assert!(reason.contains("declaration"), "{reason}: {steps:?}");
    }

    #[test]
    fn a_subagent_cannot_declare_and_a_second_declaration_is_refused() {
        let (decisions, state) = run(vec![
            Event::Declared {
                agent: sub("a"),
                text: "not mine".to_owned(),
            },
            declare("first".to_owned()),
            declare("second".to_owned()),
            stop(Agent::Main),
            declare("during the judging".to_owned()),
        ]);
        assert!(
            matches!(&decisions[0], Decision::Refused(why) if why.contains("only the main agent")),
            "{decisions:?}"
        );
        assert_eq!(decisions[1], Decision::Recorded);
        assert!(
            matches!(&decisions[2], Decision::Refused(why) if why.contains("already waits")),
            "{decisions:?}"
        );
        assert_eq!(decisions[3], Decision::Judge("first".to_owned()));
        assert!(
            matches!(&decisions[4], Decision::Refused(_)),
            "{decisions:?}"
        );
        assert_eq!(
            state.main.round,
            MainRound::Judging {
                declaration: "first".to_owned()
            },
            "the one being judged stays"
        );
        assert_eq!(state.main.declaration, None, "and nothing replaced it");
    }

    #[test]
    fn a_new_task_keeps_the_checks_under_way() {
        let (decisions, state) = run(vec![
            stop(sub("a")),
            stop(sub("b")),
            Event::Checked {
                agent: sub("b"),
                result: CheckResult::Failed,
            },
            stop(Agent::Main),
            Event::TaskSet,
            Event::Checked {
                agent: sub("a"),
                result: CheckResult::Failed,
            },
            Event::Checked {
                agent: Agent::Main,
                result: CheckResult::Failed,
            },
        ]);
        assert_eq!(
            decisions[5],
            Decision::Block(Block::CheckFailed),
            "the subagent's failed check still holds its stop"
        );
        assert_eq!(
            decisions[6],
            Decision::Block(Block::CheckFailed),
            "and so does the main agent's"
        );
        assert_eq!(
            state.subagents.len(),
            1,
            "b's count was forgotten, a's kept"
        );
        assert_eq!(state.subagents[&AgentId::new("a".to_owned())].blocked, 1);
    }

    #[test]
    fn the_state_file_names_its_fields() {
        let state = State {
            main: Main {
                blocked: 2,
                declaration: Some("stuck".to_owned()),
                unwatched: true,
                round: MainRound::Checking { watched: false },
            },
            subagents: BTreeMap::from([(
                AgentId::new("a1".to_owned()),
                super::Sub {
                    blocked: 1,
                    checking: true,
                },
            )]),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "main": {
                    "blocked": 2,
                    "declaration": "stuck",
                    "unwatched": true,
                    "round": { "phase": "checking", "watched": false }
                },
                "subagents": { "a1": { "blocked": 1, "checking": true } }
            }),
            "one record per agent"
        );
        assert_eq!(
            serde_json::from_value::<State>(json).unwrap(),
            state,
            "and reads back"
        );
    }
}
