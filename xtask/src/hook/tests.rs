use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use super::{
    Answer, Context, Event, HookInput, LAUNCHER, Location, Steps, XTASK_HOOK, git_in,
    guard_unavailable, locate, run,
};
use crate::stop::{self, CheckResult, Ruling};
use crate::testing::{git, repository, script};

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
    fs::write(worktree.join(LAUNCHER), "").unwrap();
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
    }
}

/// A script at `dir/cargo` that drains its input, records its arguments in `dir/args`, prints
/// `printed` and exits with `code`.
fn fake_cargo(dir: &Path, printed: &str, code: u8) -> PathBuf {
    let path = dir.join("cargo");
    let quoted = printed.replace('\'', r"'\''");
    script(
        &path,
        &format!(
            "cat >/dev/null\nprintf '%s\\n' \"$*\" > \"$(dirname \"$0\")/args\"\nprintf '%s' '{quoted}'\nexit {code}\n"
        ),
    );
    path
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

/// The input of the main agent's stop in `cwd`.
fn stop_in(cwd: &Path) -> String {
    serde_json::json!({ "hook_event_name": "Stop", "cwd": cwd }).to_string()
}

fn guard_at(context: &Context, cwd: Option<&Path>, command: &str) -> (Answer, String) {
    let mut log = Vec::new();
    let answer = run(Event::PreToolUse, &input(cwd, command), context, &mut log);
    (answer, String::from_utf8(log).unwrap())
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
    assert_eq!(
        locate(Some(&worktree), Some(&main), &main).unwrap(),
        Location::Worktree(worktree),
        "the agent's cwd wins"
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
fn answers_are_claude_codes_hook_json() {
    let cases = [
        (Answer::Allow, Event::Stop, None),
        (Answer::Allow, Event::PreToolUse, None),
        (
            Answer::Notify("BLOCKED: \"no\" network".to_owned()),
            Event::Stop,
            Some(serde_json::json!({ "systemMessage": "BLOCKED: \"no\" network" })),
        ),
        (
            Answer::Notify("warning".to_owned()),
            Event::PreToolUse,
            Some(serde_json::json!({ "systemMessage": "warning" })),
        ),
        (
            Answer::Block("keep working".to_owned()),
            Event::Stop,
            Some(serde_json::json!({ "decision": "block", "reason": "keep working" })),
        ),
        (
            Answer::Block("no force pushes".to_owned()),
            Event::PreToolUse,
            Some(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "no force pushes"
                }
            })),
        ),
    ];
    for (answer, event, expected) in cases {
        let printed = answer.json(event);
        assert_eq!(
            printed
                .as_deref()
                .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap()),
            expected,
            "{answer:?} for {event:?}"
        );
        assert!(
            printed.as_deref().is_none_or(|text| !text.contains('\n')),
            "one line"
        );
        assert_eq!(
            Answer::parse(event, printed.as_deref().unwrap_or_default()).unwrap(),
            answer,
            "reads back"
        );
    }
    for (event, printed) in [
        (
            Event::Stop,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"x"}}"#,
        ),
        (Event::PreToolUse, r#"{"decision":"block","reason":"x"}"#),
        (Event::Stop, r#"{"decision":"block"}"#),
        (
            Event::Stop,
            r#"{"systemMessage":"x","decision":"block","reason":"y"}"#,
        ),
        (Event::Stop, r#"{"continue":false}"#),
        (Event::Stop, "exit 2"),
    ] {
        assert!(
            Answer::parse(event, printed).is_err(),
            "{printed} is no answer for {event:?}"
        );
    }
}

#[test]
fn the_guard_reads_the_project_directory_when_cwd_is_absent() {
    let (_temp, main, _worktree) = repository_with_nested_worktree();
    let (answer, _) = guard_at(&context(&main, Some(&main)), None, "git push --force");
    assert!(
        matches!(&answer, Answer::Block(reason) if reason.contains("force-pushes")),
        "a force push in the project directory is denied, with the reason: {answer:?}"
    );
    let (allowed, _) = guard_at(&context(&main, Some(&main)), None, "git status");
    assert_eq!(allowed, Answer::Allow, "a safe command says nothing");
}

#[test]
fn outside_any_repository_hooks_allow_without_checks() {
    let (_temp, main, _worktree) = repository_with_nested_worktree();
    let outside = tempfile::tempdir().unwrap();
    let context = context(&main, Some(&main));
    let (answer, log) = guard_at(&context, Some(outside.path()), "git push --force");
    assert_eq!(
        answer,
        Answer::Allow,
        "nothing is checked outside the repository"
    );
    assert!(log.is_empty(), "and nothing is said");
    let mut stop_log = Vec::new();
    let stopped = run(
        Event::Stop,
        &stop_in(outside.path()),
        &context,
        &mut stop_log,
    );
    let said = String::from_utf8(stop_log).unwrap();
    assert_eq!(stopped, Answer::Allow, "the agent may stop");
    assert_eq!(said.lines().count(), 1, "one line: {said:?}");
    assert!(
        said.contains("is not in a git repository; no checks run"),
        "{said:?}"
    );
}

#[test]
fn a_missing_cwd_blocks_git_and_holds_the_stop() {
    let (temp, main, _worktree) = repository_with_nested_worktree();
    let gone = temp.path().join("removed-worktree");
    let context = context(&main, Some(&main));
    let (push, _) = guard_at(&context, Some(&gone), "git push origin main");
    assert!(
        matches!(&push, Answer::Block(reason) if reason.contains("does not exist")),
        "git is denied when the guard cannot locate the agent, with the reason: {push:?}"
    );
    let (listing, _) = guard_at(&context, Some(&gone), "ls -la");
    assert!(
        matches!(&listing, Answer::Notify(warning) if warning.starts_with("warning: ")),
        "other commands still run, with a warning: {listing:?}"
    );
    let stopped = run(Event::Stop, &stop_in(&gone), &context, &mut Vec::new());
    assert!(
        matches!(&stopped, Answer::Block(reason) if reason.contains("does not exist")),
        "the stop is held and says why: {stopped:?}"
    );
}

#[test]
fn a_worktree_without_the_launcher_is_noted_not_checked() {
    let (_temp, main, worktree) = repository_with_nested_worktree();
    fs::remove_file(worktree.join(LAUNCHER)).unwrap();
    let context = context(&main, Some(&main));
    let (answer, log) = guard_at(&context, Some(&worktree), "git push -f");
    assert_eq!(answer, Answer::Allow, "an older checkout is not locked out");
    assert_eq!(log.lines().count(), 1, "one line: {log}");
    assert!(log.contains("nothing is checked there"), "{log}");
    let mut stop_log = Vec::new();
    let stopped = run(Event::Stop, &stop_in(&worktree), &context, &mut stop_log);
    assert_eq!(stopped, Answer::Allow, "the agent may stop");
    assert!(
        String::from_utf8(stop_log)
            .unwrap()
            .contains("nothing is checked there"),
        "with a note"
    );
}

#[test]
fn a_worktree_answers_through_its_own_xtask() {
    let (temp, main, worktree) = repository_with_nested_worktree();
    let mut handing = context(&main, Some(&main));
    let deny = Answer::Block("no 'force' pushes".to_owned())
        .json(Event::PreToolUse)
        .unwrap();
    handing.cargo = fake_cargo(temp.path(), &deny, 0);
    let (answer, _) = guard_at(&handing, Some(&worktree), "git push -f");
    assert_eq!(
        answer,
        Answer::Block("no 'force' pushes".to_owned()),
        "the worktree's own answer"
    );
    let mut expected: Vec<&str> = XTASK_HOOK.to_vec();
    expected.push("pre-tool-use");
    assert_eq!(
        fs::read_to_string(temp.path().join("args")).unwrap(),
        format!("{}\n", expected.join(" ")),
        "its xtask, from its own workspace, built apart from its cargo lock"
    );
    let held = Answer::Block("held".to_owned()).json(Event::Stop).unwrap();
    handing.cargo = fake_cargo(temp.path(), &held, 0);
    assert_eq!(
        run(Event::Stop, &stop_in(&worktree), &handing, &mut Vec::new()),
        Answer::Block("held".to_owned()),
        "and its stop answer"
    );
}

#[test]
fn a_worktree_whose_xtask_cannot_start_blocks_git_and_holds_the_stop() {
    let (temp, main, worktree) = repository_with_nested_worktree();
    let mut broken = context(&main, Some(&main));
    for (printed, code) in [("", 101), ("", 2), ("not json", 0)] {
        broken.cargo = fake_cargo(temp.path(), printed, code);
        let (merge, _) = guard_at(&broken, Some(&worktree), "gh pr merge 1");
        assert!(
            matches!(&merge, Answer::Block(reason) if reason.contains("could not run")),
            "{printed:?} {code}: {merge:?}"
        );
        let (build, _) = guard_at(&broken, Some(&worktree), "cargo build");
        assert!(matches!(build, Answer::Notify(_)), "{build:?}");
    }
    broken.cargo = fake_cargo(temp.path(), "", 2);
    let stopped = run(Event::Stop, &stop_in(&worktree), &broken, &mut Vec::new());
    assert!(
        matches!(&stopped, Answer::Block(reason) if reason.contains("exited 2")),
        "an exit code is never an answer: {stopped:?}"
    );
    assert!(
        worktree.join(stop::STATE).is_file(),
        "the hold is counted in the worktree"
    );
    assert!(
        !main.join(stop::STATE).exists(),
        "and not in the checkout the hook started in"
    );
}

#[test]
fn a_handed_over_hook_is_not_handed_on() {
    let (_temp, main, worktree) = repository_with_nested_worktree();
    let mut handed_over = context(&main, Some(&main));
    handed_over.delegated = true;
    let stopped = run(
        Event::Stop,
        &stop_in(&worktree),
        &handed_over,
        &mut Vec::new(),
    );
    assert!(
        matches!(&stopped, Answer::Block(reason) if reason.contains("another checkout")),
        "{stopped:?}"
    );
    assert!(worktree.join(stop::STATE).is_file(), "held in the worktree");
    let (answer, _) = guard_at(&handed_over, Some(&worktree), "git status");
    assert!(
        matches!(answer, Answer::Block(_)),
        "the guard fails closed for git"
    );
}

#[test]
fn unparseable_input_blocks_the_guard_and_holds_the_stop() {
    let (_temp, main, _worktree) = repository_with_nested_worktree();
    assert!(
        matches!(guard_unavailable("the build failed", "not json"), Answer::Block(reason) if reason.starts_with("Blocked: ")),
        "no command can be read, so it may be git"
    );
    let context = context(&main, Some(&main));
    assert!(
        matches!(
            run(Event::PreToolUse, "not json", &context, &mut Vec::new()),
            Answer::Block(_)
        ),
        "the guard blocks input it cannot read"
    );
    let stopped = run(Event::Stop, "not json", &context, &mut Vec::new());
    assert!(
        matches!(&stopped, Answer::Block(reason) if reason.contains("could not read the Stop hook input")),
        "{stopped:?}"
    );
    assert!(main.join(stop::STATE).is_file(), "and counts it");
}

#[test]
fn the_watchdogs_own_session_stops_without_checks() {
    let (_temp, main, _worktree) = repository_with_nested_worktree();
    let mut inside = context(&main, Some(&main));
    inside.inside_watchdog = true;
    assert_eq!(
        run(Event::Stop, "not json", &inside, &mut Vec::new()),
        Answer::Allow
    );
    assert!(!main.join(".abc").exists(), "nothing recorded");
}

#[test]
fn a_stop_is_the_main_agents_unless_it_is_a_subagents() {
    let agent = |json: &str| serde_json::from_str::<HookInput>(json).unwrap().agent();
    assert_eq!(agent(r#"{"hook_event_name": "Stop"}"#), stop::Agent::Main);
    assert_eq!(agent("{}"), stop::Agent::Main, "an unnamed event");
    assert_eq!(
        agent(r#"{"hook_event_name": "SubagentStop", "agent_id": "a1"}"#),
        stop::Agent::Sub(stop::AgentId::new("a1".to_owned()))
    );
}

#[test]
fn a_handed_over_hook_names_its_event_as_the_command_line_does() {
    use clap::ValueEnum as _;
    for event in Event::value_variants() {
        let name = event.to_possible_value().unwrap();
        assert_eq!(event.name(), name.get_name(), "{event:?}");
    }
}

/// Steps that answer as told and count how often the judge and the watchdog ran.
struct Fake {
    check: Result<CheckResult, String>,
    sent_back: Option<String>,
    drift: Option<String>,
    checked: u32,
    judged: u32,
    watched: u32,
}

impl Fake {
    const fn new(check: Result<CheckResult, String>) -> Self {
        Self {
            check,
            sent_back: None,
            drift: None,
            checked: 0,
            judged: 0,
            watched: 0,
        }
    }
}

impl Steps for Fake {
    fn check(&mut self, out: &mut Vec<u8>) -> anyhow::Result<CheckResult> {
        self.checked = self.checked.saturating_add(1);
        out.extend_from_slice(b"the check ran\n");
        self.check.clone().map_err(|error| anyhow::anyhow!(error))
    }

    fn judge(&mut self, declaration: &str, _: &mut Vec<u8>) -> anyhow::Result<Ruling> {
        anyhow::ensure!(!declaration.is_empty(), "a declaration to judge");
        self.judged = self.judged.saturating_add(1);
        Ok(self
            .sent_back
            .clone()
            .map_or(Ruling::Outside, Ruling::SentBack))
    }

    fn watch(&mut self, _: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
        self.watched = self.watched.saturating_add(1);
        Ok(self.drift.clone())
    }
}

/// The hook input of the main agent's first stop.
const MAIN: &str = r#"{"hook_event_name": "Stop", "stop_hook_active": false}"#;
/// The hook input of the main agent's stop after a stop hook sent it back.
const MAIN_AGAIN: &str = r#"{"hook_event_name": "Stop", "stop_hook_active": true}"#;
/// The hook input of a subagent's stop.
const SUBAGENT: &str = r#"{"hook_event_name": "SubagentStop", "agent_id": "a1"}"#;

fn drive(root: &Path, input: &str, steps: &mut Fake) -> Answer {
    let fields = serde_json::from_str::<HookInput>(input).unwrap();
    super::drive(root, &fields, steps, &mut Vec::new()).unwrap()
}

#[test]
fn a_passing_first_stop_goes_to_the_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = Fake::new(Ok(CheckResult::Passed));
    assert_eq!(drive(dir.path(), MAIN, &mut steps), Answer::Allow);
    assert_eq!(steps.watched, 1, "the watchdog judged it");
    steps.drift = Some("off the task".to_owned());
    assert_eq!(
        drive(dir.path(), MAIN, &mut steps),
        Answer::Block("the check ran\noff the task".to_owned()),
        "a drift keeps the agent working, with what the steps said"
    );
    assert_eq!(drive(dir.path(), MAIN_AGAIN, &mut steps), Answer::Allow);
    assert_eq!(
        steps.watched, 2,
        "a stop the hook sent back is never judged again"
    );
    assert_eq!(drive(dir.path(), SUBAGENT, &mut steps), Answer::Allow);
    assert_eq!(steps.watched, 2, "nor a subagent's");
}

#[test]
fn a_failing_check_holds_the_stop_and_tells_the_agent_how_to_declare() {
    let dir = tempfile::tempdir().unwrap();
    let held = drive(dir.path(), MAIN, &mut Fake::new(Ok(CheckResult::Failed)));
    assert!(
        matches!(&held, Answer::Block(reason) if reason.starts_with("the check ran\n") && reason.contains("cargo xtask task blocked")),
        "{held:?}"
    );
    let cannot = drive(
        dir.path(),
        MAIN,
        &mut Fake::new(Err("no nextest".to_owned())),
    );
    assert!(
        matches!(&cannot, Answer::Block(reason) if reason.contains("the check could not run: no nextest")),
        "a check that cannot run fails: {cannot:?}"
    );
    let sub = drive(
        dir.path(),
        SUBAGENT,
        &mut Fake::new(Ok(CheckResult::Failed)),
    );
    assert!(
        matches!(&sub, Answer::Block(reason) if !reason.contains("task blocked")),
        "a subagent has no declaration to make: {sub:?}"
    );
}

/// PR 5a: the stop hook's `check` started beside the agent's `coverage`, and the out-of-memory
/// killer took the linker. A stop that finds another gate running is sent back to wait for it.
#[test]
fn a_stop_while_another_gate_runs_is_sent_back_to_wait_for_it() {
    let dir = tempfile::tempdir().unwrap();
    for input in [MAIN, SUBAGENT] {
        let mut busy = Fake::new(Ok(CheckResult::GateBusy));
        let held = drive(dir.path(), input, &mut busy);
        assert!(
            matches!(&held, Answer::Block(reason) if reason.contains("wait for that gate to finish") && !reason.contains("task blocked")),
            "{input}: {held:?}"
        );
        assert_eq!(busy.watched, 0, "{input}: nothing was checked to watch");
    }
    let broken = tempfile::tempdir().unwrap();
    break_state(broken.path(), "no .abc");
    let kept = super::stop_with(
        broken.path(),
        &serde_json::from_str::<HookInput>(MAIN).unwrap(),
        &mut Fake::new(Ok(CheckResult::GateBusy)),
        &mut Vec::new(),
    );
    assert!(
        matches!(&kept, Answer::Held(reason) if reason.contains("check: NOT RUN") && reason.contains("wait for that gate")),
        "with no state to count it in, the launcher holds it: {kept:?}"
    );
}

#[test]
fn the_backstop_lets_the_sixth_blocked_stop_through_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..stop::MAX_BLOCKED_STOPS {
        assert!(matches!(
            drive(
                dir.path(),
                MAIN_AGAIN,
                &mut Fake::new(Ok(CheckResult::Failed))
            ),
            Answer::Block(_)
        ));
    }
    let through = drive(
        dir.path(),
        MAIN_AGAIN,
        &mut Fake::new(Ok(CheckResult::Failed)),
    );
    assert!(
        matches!(&through, Answer::Notify(message) if message.contains("5 stops in a row")),
        "{through:?}"
    );
    let record = fs::read_to_string(dir.path().join(stop::BACKSTOP)).unwrap();
    assert!(record.contains("after 5 blocked stops"), "{record}");
    for _ in 0..stop::MAX_BLOCKED_STOPS {
        drive(
            dir.path(),
            SUBAGENT,
            &mut Fake::new(Ok(CheckResult::Failed)),
        );
    }
    let sub = drive(
        dir.path(),
        SUBAGENT,
        &mut Fake::new(Ok(CheckResult::Failed)),
    );
    assert!(
        matches!(&sub, Answer::Notify(message) if message.starts_with("A subagent's check failed")),
        "{sub:?}"
    );
}

#[test]
fn a_genuine_declaration_ends_the_stop_with_a_handoff_and_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    crate::task::block(dir.path(), "no network; tried twice", &mut std::io::sink()).unwrap();
    let mut steps = Fake::new(Ok(CheckResult::Failed));
    assert_eq!(
        drive(dir.path(), MAIN, &mut steps),
        Answer::Notify("BLOCKED, stop allowed. Handoff: no network; tried twice".to_owned())
    );
    assert_eq!(steps.judged, 1, "judged");
    let log = fs::read_to_string(dir.path().join(stop::BLOCKED_LOG)).unwrap();
    assert!(
        log.contains(": stop allowed\n\nno network; tried twice\n"),
        "{log}"
    );
}

#[test]
fn a_declaration_at_the_cap_goes_through_unjudged_and_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..stop::MAX_BLOCKED_STOPS {
        drive(
            dir.path(),
            MAIN_AGAIN,
            &mut Fake::new(Ok(CheckResult::Failed)),
        );
    }
    crate::task::block(dir.path(), "stuck", &mut std::io::sink()).unwrap();
    let mut steps = Fake::new(Ok(CheckResult::Failed));
    assert_eq!(
        drive(dir.path(), MAIN_AGAIN, &mut steps),
        Answer::Notify("BLOCKED, stop allowed. Handoff: stuck".to_owned())
    );
    assert_eq!(steps.judged, 0, "not judged");
    let log = fs::read_to_string(dir.path().join(stop::BLOCKED_LOG)).unwrap();
    assert!(
        log.contains(": stop allowed, not judged: 5 blocked stops in a row\n\nstuck\n"),
        "{log}"
    );
}

#[test]
fn a_refused_declaration_is_logged_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    crate::task::block(dir.path(), "the check fails", &mut std::io::sink()).unwrap();
    let mut steps = Fake::new(Ok(CheckResult::Failed));
    steps.sent_back = Some("a failing check is the work".to_owned());
    let sent = drive(dir.path(), MAIN, &mut steps);
    assert!(
        matches!(&sent, Answer::Block(reason) if reason.contains("was sent back: a failing check is the work")),
        "{sent:?}"
    );
    let log = fs::read_to_string(dir.path().join(stop::BLOCKED_LOG)).unwrap();
    assert!(
        log.contains(": sent back: a failing check is the work\n\nthe check fails\n"),
        "{log}"
    );
    assert!(matches!(
        drive(dir.path(), MAIN_AGAIN, &mut steps),
        Answer::Block(reason) if reason.contains("the check did not pass")
    ));
    assert_eq!(steps.judged, 1, "the declaration covered one stop");
}

#[test]
fn a_subagents_stops_never_spend_the_main_agents_declaration() {
    let dir = tempfile::tempdir().unwrap();
    crate::task::block(dir.path(), "no network", &mut std::io::sink()).unwrap();
    let mut steps = Fake::new(Ok(CheckResult::Failed));
    for _ in 0..=stop::MAX_BLOCKED_STOPS {
        drive(dir.path(), SUBAGENT, &mut steps);
    }
    assert_eq!(steps.judged, 0, "never judged for a subagent");
    assert_eq!(
        drive(dir.path(), MAIN, &mut steps),
        Answer::Notify("BLOCKED, stop allowed. Handoff: no network".to_owned()),
        "the main agent's declaration is still there"
    );
}

/// Makes the stop state of the repository at `root` impossible to keep, in the way `case` names.
fn break_state(root: &Path, case: &str) {
    match case {
        // A read-only or missing `.abc` cannot be created either.
        "no .abc" => fs::write(root.join(".abc"), "not a directory").unwrap(),
        // A full disk fails the write of the state; so does a path the state cannot replace.
        "state path is a directory" => fs::create_dir_all(root.join(stop::STATE)).unwrap(),
        "lock path is a directory" => {
            fs::create_dir_all(root.join(".abc/stop-state.lock")).unwrap();
        }
        other => panic!("unknown case {other}"),
    }
}

const BROKEN_STATES: [&str; 3] = [
    "no .abc",
    "state path is a directory",
    "lock path is a directory",
];

#[test]
fn a_state_that_cannot_be_kept_still_runs_the_check_once_and_says_pass_or_fail() {
    for case in BROKEN_STATES {
        let temp = tempfile::tempdir().unwrap();
        let root = repository(&temp.path().join("abc"));
        break_state(&root, case);
        let fields = serde_json::from_str::<HookInput>(MAIN).unwrap();
        assert!(
            super::drive(
                &root,
                &fields,
                &mut Fake::new(Ok(CheckResult::Passed)),
                &mut Vec::new()
            )
            .is_err(),
            "{case}: no state to count in"
        );
        let mut passing = Fake::new(Ok(CheckResult::Passed));
        let passed = super::stop_with(&root, &fields, &mut passing, &mut Vec::new());
        assert!(
            matches!(&passed, Answer::Notify(message) if message.contains("check: PASS") && message.contains("could not keep its state")),
            "{case}: {passed:?}"
        );
        assert_eq!(passing.checked, 1, "{case}: the check ran once");
        assert_eq!(passing.watched, 0, "{case}");
        let mut failing = Fake::new(Ok(CheckResult::Failed));
        let failed = super::stop_with(&root, &fields, &mut failing, &mut Vec::new());
        assert!(
            matches!(&failed, Answer::Held(message) if message.contains("the check ran") && message.contains("check: FAIL") && message.contains("could not keep its state")),
            "{case}: a failure is held for the launcher to count: {failed:?}"
        );
        assert_eq!(
            failing.checked, 1,
            "{case}: once, though the state failed after it"
        );
        let mut broken_check = Fake::new(Err("no tools".to_owned()));
        let errored = super::stop_with(&root, &fields, &mut broken_check, &mut Vec::new());
        assert!(
            matches!(&errored, Answer::Held(message) if message.contains("the check could not run: no tools")),
            "{case}: {errored:?}"
        );
    }
}

#[test]
fn a_stop_with_a_working_state_passes_the_watchdog_through() {
    let temp = tempfile::tempdir().unwrap();
    let root = repository(&temp.path().join("abc"));
    let fields = serde_json::from_str::<HookInput>(MAIN).unwrap();
    let mut drifting = Fake::new(Ok(CheckResult::Passed));
    drifting.drift = Some("off task".to_owned());
    let answer = super::stop_with(&root, &fields, &mut drifting, &mut Vec::new());
    assert!(
        matches!(&answer, Answer::Block(why) if why.contains("off task")),
        "{answer:?}"
    );
}

#[test]
fn a_stop_with_a_working_state_is_never_held_for_the_launcher() {
    let temp = tempfile::tempdir().unwrap();
    let root = repository(&temp.path().join("abc"));
    let fields = serde_json::from_str::<HookInput>(MAIN).unwrap();
    let mut failing = Fake::new(Ok(CheckResult::Failed));
    let blocked = super::stop_with(&root, &fields, &mut failing, &mut Vec::new());
    assert!(matches!(blocked, Answer::Block(_)), "{blocked:?}");
    assert_eq!(failing.checked, 1);
}

#[test]
fn a_held_answer_is_no_json_and_a_hand_over_passes_it_on() {
    assert_eq!(Answer::Held("x".to_owned()).json(Event::Stop), None);
    let (temp, main, worktree) = repository_with_nested_worktree();
    let mut handed = context(&main, Some(&main));
    handed.cargo = fake_cargo(temp.path(), "the check failed", super::UNKEPT_STATUS);
    let held = run(Event::Stop, &stop_in(&worktree), &handed, &mut Vec::new());
    assert_eq!(held, Answer::Held("the check failed".to_owned()));
}

#[test]
fn a_subagent_may_not_declare_a_task_blocked() {
    let context = context(Path::new("/"), None);
    let subagent = |command: &str| {
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "agent_id": "a1",
            "tool_name": "Bash",
            "tool_input": { "command": command },
        })
        .to_string()
    };
    for command in [
        "cargo xtask task blocked \"no network\"",
        "cd x && cargo xtask task   blocked 'y'",
    ] {
        let answer = run(
            Event::PreToolUse,
            &subagent(command),
            &context,
            &mut Vec::new(),
        );
        assert!(
            matches!(&answer, Answer::Block(why) if why.contains("only the main agent")),
            "{command}: {answer:?}"
        );
    }
    let other = run(
        Event::PreToolUse,
        &subagent("cargo xtask task show"),
        &context,
        &mut Vec::new(),
    );
    assert!(!matches!(&other, Answer::Block(why) if why.contains("only the main agent")));
    assert!(super::declares_blocked("cargo xtask task blocked x"));
    assert!(!super::declares_blocked("cargo xtask task set blocked"));
    assert!(!super::declares_blocked("echo task blocked"));
}

#[test]
fn a_check_that_outlives_its_deadline_is_stopped_and_fails_with_a_reason() {
    let mut slow = Command::new("sh");
    // The grandchild is in the check's process group too, and must not outlive it.
    slow.args(["-c", "echo started; sleep 60 & sleep 60"]);
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let status = super::run_bounded(slow, std::time::Duration::from_millis(400), &mut out).unwrap();
    let said = String::from_utf8(out).unwrap();
    assert_eq!(status, None, "{said}");
    assert_eq!(super::check_result(status), CheckResult::Failed);
    assert!(said.contains("started"), "what it printed is kept: {said}");
    assert!(
        said.contains("check did not finish in 0 s"),
        "the reason names the deadline: {said}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "stopped, not awaited"
    );
}

#[test]
fn a_check_inside_its_deadline_passes_or_fails_by_its_exit_status() {
    for (script, expected) in [
        ("sleep 0.3; echo fine", CheckResult::Passed),
        ("echo broken >&2; exit 1", CheckResult::Failed),
        (
            "echo 'check: not started' >&2; exit 75",
            CheckResult::GateBusy,
        ),
    ] {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let mut out = Vec::new();
        let status =
            super::run_bounded(command, std::time::Duration::from_secs(60), &mut out).unwrap();
        assert_eq!(super::check_result(status), expected, "{script}");
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("fine") || said.contains("broken") || said.contains("not started"),
            "stdout and stderr: {said}"
        );
        assert!(!said.contains("did not finish"), "{said}");
    }
    assert!(
        super::run_bounded(
            Command::new("/no/such/program"),
            std::time::Duration::from_secs(1),
            &mut Vec::new()
        )
        .is_err(),
        "a check that cannot start is an error"
    );
}

/// The shells hooks must work under: the system `sh` and dash, Debian's and Ubuntu's `sh`.
const SHELLS: [&str; 2] = ["sh", "/bin/dash"];

/// What one run of the launcher did.
#[derive(Debug, PartialEq, Eq)]
struct Launched {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Launched {
    /// The answer the launcher printed, as Claude Code reads it.
    fn answer(&self, event: Event) -> Answer {
        assert_eq!(self.code, Some(0), "a hook always exits 0: {self:?}");
        Answer::parse(event, &self.stdout).unwrap()
    }
}

/// Runs the launcher for `event` under `shell` on `input`, in the project directory `project`,
/// with `bin` first on `PATH`, and when `vm` as in an agent VM: `ABC_VM` set and `project` the
/// session's worktree.
fn launch(shell: &str, event: &str, project: &Path, bin: &Path, input: &str, vm: bool) -> Launched {
    let launcher = crate::testing::checkout().join(LAUNCHER);
    let mut command = Command::new(shell);
    command
        .arg(&launcher)
        .arg(event)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("CLAUDE_PROJECT_DIR", project)
        // These tests run in an agent VM too, which sets this.
        .env_remove("ABC_VM")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if vm {
        command.env("ABC_VM", "1").env("ABC_WORKTREE", project);
        // The image's disk check, as xtask/vm/Containerfile installs it, unless the test put
        // another in its place.
        let image = bin.join("abc-own-disks");
        if !image.exists() {
            fs::copy(
                crate::testing::checkout().join("xtask/vm/own-disks"),
                &image,
            )
            .unwrap();
        }
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{shell} must be installed: {error}"));
    crate::tools::feed(&mut child, &mut input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    Launched {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// Runs the launcher under every shell with a cargo that prints `printed` and exits `code`,
/// checks that the shells agree, and returns the first one's run.
fn launch_with(event: &str, printed: &str, code: u8, input: &str) -> Launched {
    let runs: Vec<Launched> = SHELLS
        .iter()
        .map(|shell| {
            let temp = tempfile::tempdir().unwrap();
            fake_cargo(temp.path(), printed, code);
            launch(shell, event, temp.path(), temp.path(), input, false)
        })
        .collect();
    assert!(
        runs.windows(2)
            .all(|pair| pair[0].code == pair[1].code && pair[0].stdout == pair[1].stdout),
        "the shells disagree: {runs:?}"
    );
    runs.into_iter().next().unwrap()
}

#[test]
fn the_launcher_passes_xtasks_answer_on() {
    for (event, answer) in [
        (
            Event::PreToolUse,
            Answer::Block("no force pushes".to_owned()),
        ),
        (Event::Stop, Answer::Block("keep working".to_owned())),
        (
            Event::Stop,
            Answer::Notify("BLOCKED, stop allowed".to_owned()),
        ),
        (Event::PreToolUse, Answer::Allow),
    ] {
        let printed = answer.json(event).unwrap_or_default();
        let launched = launch_with(event.name(), &printed, 0, "{}");
        assert_eq!(launched.answer(event), answer, "{launched:?}");
    }
}

#[test]
fn the_launcher_runs_the_checkouts_xtask_built_apart() {
    let temp = tempfile::tempdir().unwrap();
    fake_cargo(temp.path(), "", 0);
    for event in [Event::PreToolUse, Event::Stop] {
        launch("sh", event.name(), temp.path(), temp.path(), "{}", false);
        let mut expected: Vec<&str> = XTASK_HOOK.to_vec();
        expected.push(event.name());
        assert_eq!(
            fs::read_to_string(temp.path().join("args")).unwrap(),
            format!("{}\n", expected.join(" ")),
            "the launcher and a hand-over run the same xtask"
        );
    }
}

#[test]
fn an_exit_code_or_a_usage_error_never_reads_as_a_block() {
    let usage = "error: unexpected argument 'x' found";
    for code in [1, 2, 101] {
        let listing = launch_with(
            "pre-tool-use",
            usage,
            code,
            r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        );
        assert!(
            matches!(listing.answer(Event::PreToolUse), Answer::Notify(warning) if warning.contains(&format!("exited {code}"))),
            "xtask exiting {code} is the launcher's to answer, never a block: {listing:?}"
        );
        let stop = launch_with("stop", usage, code, "{}");
        assert!(
            matches!(stop.answer(Event::Stop), Answer::Block(reason) if reason.contains("xtask could not start")),
            "the launcher holds the stop itself: {stop:?}"
        );
    }
}

#[test]
fn when_xtask_cannot_start_the_launcher_denies_only_git_and_gh() {
    let git_push = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"git push origin main"}}"#;
    let quoted =
        r#"{"tool_name":"Bash","tool_input":{"command":"echo \"x\" && gh api repos/o/r"}}"#;
    for input in [git_push, quoted] {
        let denied = launch_with("pre-tool-use", "", 101, input).answer(Event::PreToolUse);
        assert!(
            matches!(&denied, Answer::Block(reason) if reason.contains("could not run")),
            "{input}: {denied:?}"
        );
    }
    let listing = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"ls .github .gitignore"}}"#;
    let socket = r#"{"tool_name":"Monitor","tool_input":{"ws":{"url":"wss://example.com"}}}"#;
    for input in [listing, socket] {
        let allowed = launch_with("pre-tool-use", "", 101, input).answer(Event::PreToolUse);
        assert!(
            matches!(&allowed, Answer::Notify(warning) if warning.starts_with("warning: ")),
            "a git path in cwd, or no command, is not a git command: {allowed:?}"
        );
    }
}

#[test]
fn when_xtask_cannot_start_the_launcher_holds_a_stop_five_times() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), "", 101);
        let stop = || launch(shell, "stop", temp.path(), temp.path(), "{}", false);
        for held in 1..=5 {
            let answer = stop().answer(Event::Stop);
            assert!(
                matches!(answer, Answer::Block(_)),
                "{shell}: hold {held}: {answer:?}"
            );
            assert_eq!(
                fs::read_to_string(temp.path().join(".abc/hook-holds")).unwrap(),
                format!("{held}\n"),
                "{shell}"
            );
        }
        let through = stop().answer(Event::Stop);
        assert!(
            matches!(&through, Answer::Notify(message) if message.contains("5 stops in a row were held")),
            "{shell}: the sixth goes through: {through:?}"
        );
        assert!(
            !temp.path().join(".abc/hook-holds").exists(),
            "{shell}: reset"
        );
        stop();
        fake_cargo(temp.path(), "", 0);
        stop();
        assert!(
            !temp.path().join(".abc/hook-holds").exists(),
            "{shell}: an xtask that starts again resets the count"
        );
    }
}

/// The input of a subagent `id`'s stop, as the launcher reads it.
fn subagent_stop(id: &str) -> String {
    format!(r#"{{"hook_event_name": "SubagentStop", "agent_id": "{id}"}}"#)
}

#[test]
fn the_launcher_counts_holds_per_agent() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), "", 101);
        let stop = |input: &str| launch(shell, "stop", temp.path(), temp.path(), input, false);
        let holds = |name: &str| fs::read_to_string(temp.path().join(".abc").join(name)).ok();
        for _ in 0..5 {
            assert!(matches!(stop("{}").answer(Event::Stop), Answer::Block(_)));
        }
        assert_eq!(
            holds("hook-holds").as_deref(),
            Some("5\n"),
            "{shell}: the main agent's"
        );
        let sub = stop(&subagent_stop("a-1")).answer(Event::Stop);
        assert!(matches!(sub, Answer::Block(_)), "{shell}: {sub:?}");
        assert_eq!(
            holds("hook-holds.a-1").as_deref(),
            Some("1\n"),
            "{shell}: a subagent's own"
        );
        assert_eq!(
            holds("hook-holds").as_deref(),
            Some("5\n"),
            "{shell}: untouched"
        );
        // A key's text inside a value is not an agent: it is the main agent's sixth stop.
        let text = r#"{"hook_event_name":"Stop","last_assistant_message":"\"agent_id\": \"x\""}"#;
        let through = stop(text).answer(Event::Stop);
        assert!(
            matches!(&through, Answer::Notify(message) if message.contains("5 stops in a row")),
            "{shell}: {through:?}"
        );
        assert!(holds("hook-holds.x").is_none(), "{shell}");
        // Unsafe characters in an id cannot leave .abc.
        stop(&subagent_stop("../../evil"));
        assert!(!temp.path().join("evil").exists(), "{shell}");
    }
}

#[test]
fn only_a_stop_xtask_let_through_clears_the_holds() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        let holds = temp.path().join(".abc/hook-holds");
        let run = |event: &str, printed: &str, code: u8| {
            fake_cargo(temp.path(), printed, code);
            launch(shell, event, temp.path(), temp.path(), "{}", false)
        };
        run("stop", "", 101);
        run("stop", "", 101);
        assert_eq!(fs::read_to_string(&holds).unwrap(), "2\n", "{shell}");
        run("pre-tool-use", "", 0);
        assert!(holds.exists(), "{shell}: a tool call is no stop");
        let blocked = Answer::Block("keep working".to_owned())
            .json(Event::Stop)
            .unwrap();
        run("stop", &blocked, 0);
        assert!(
            holds.exists(),
            "{shell}: a stop xtask blocked is no stop let through"
        );
        run("stop", "", super::UNKEPT_STATUS);
        assert_eq!(
            fs::read_to_string(&holds).unwrap(),
            "3\n",
            "{shell}: counted"
        );
        run("stop", "", 0);
        assert!(!holds.exists(), "{shell}: a stop let through clears them");
    }
}

#[test]
fn a_check_that_failed_with_no_state_is_held_by_the_launcher_with_xtasks_reason() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(
            temp.path(),
            "check: FAIL. could not keep its state",
            super::UNKEPT_STATUS,
        );
        for held in 1..=5 {
            let answer =
                launch(shell, "stop", temp.path(), temp.path(), "{}", false).answer(Event::Stop);
            assert!(
                matches!(&answer, Answer::Block(reason) if reason.contains("check: FAIL")),
                "{shell}: hold {held}: {answer:?}"
            );
        }
        let through =
            launch(shell, "stop", temp.path(), temp.path(), "{}", false).answer(Event::Stop);
        assert!(
            matches!(&through, Answer::Notify(message) if message.contains("check: FAIL") && message.contains("5 stops in a row")),
            "{shell}: {through:?}"
        );
    }
}

#[test]
fn a_stop_let_through_at_the_cap_is_recorded() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), "", 101);
        for (input, who) in [
            ("{}", "the main agent"),
            (&*subagent_stop("a1"), "subagent a1"),
        ] {
            for _ in 0..6 {
                launch(shell, "stop", temp.path(), temp.path(), input, false);
            }
            let record = fs::read_to_string(temp.path().join(stop::BACKSTOP)).unwrap();
            assert!(
                record.contains(&format!(
                    "a stop of {who} through after 5 blocked stops in a row"
                )),
                "{shell}: {record}"
            );
            assert!(record.contains("launcher backstop"), "{shell}: {record}");
        }
    }
}

#[test]
fn a_stop_the_launcher_cannot_count_goes_through() {
    let temp = tempfile::tempdir().unwrap();
    fake_cargo(temp.path(), "", 101);
    fs::write(temp.path().join(".abc"), "not a directory").unwrap();
    let through = launch("sh", "stop", temp.path(), temp.path(), "{}", false).answer(Event::Stop);
    assert!(matches!(through, Answer::Notify(_)), "{through:?}");
}

/// Gives the worktree `project` a target/ and a .tools/ on a filesystem of their own, as an agent
/// VM's disks are: links to /dev, which is never on the same filesystem as a temporary directory.
fn own_disks(project: &Path) {
    for dir in ["target", ".tools"] {
        std::os::unix::fs::symlink("/dev", project.join(dir)).unwrap();
    }
}

#[test]
fn in_an_agent_vm_the_launcher_checks_the_billing_first() {
    let temp = tempfile::tempdir().unwrap();
    fake_cargo(temp.path(), "", 0);
    own_disks(temp.path());
    let checked = temp.path().join("checked");
    script(
        &temp.path().join("abc-xtask"),
        &format!(
            "echo \"$*\" > {}\necho 'found: \"ANTHROPIC_API_KEY\" in the environment' >&2\nexit 1\n",
            checked.display()
        ),
    );
    let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let vm = launch("sh", "pre-tool-use", temp.path(), temp.path(), input, true);
    assert_eq!(
        vm.answer(Event::PreToolUse),
        Answer::Block("found: \"ANTHROPIC_API_KEY\" in the environment".to_owned()),
        "a finding denies the tool call, with the finding as the reason"
    );
    assert_eq!(fs::read_to_string(&checked).unwrap(), "vm billing\n");
    fs::remove_file(&checked).unwrap();
    let outside = launch("sh", "pre-tool-use", temp.path(), temp.path(), input, false);
    assert_eq!(
        outside.answer(Event::PreToolUse),
        Answer::Allow,
        "nothing to check outside a VM"
    );
    assert!(!checked.exists());
    let stop = launch("sh", "stop", temp.path(), temp.path(), "{}", true);
    assert_eq!(
        stop.answer(Event::Stop),
        Answer::Allow,
        "billing is the guard's"
    );
    assert!(!checked.exists());
    script(&temp.path().join("abc-xtask"), "exit 0\n");
    let clean = launch("sh", "pre-tool-use", temp.path(), temp.path(), input, true);
    assert_eq!(
        clean.answer(Event::PreToolUse),
        Answer::Allow,
        "a clean VM goes on to xtask"
    );
}

/// S0 PR 5a: the host's target/ was deleted while the VM ran, which unmounted the VM's build disk,
/// and builds went to the worktree mount shared with the host unnoticed. In a VM, the launcher
/// denies every tool call and holds every stop while target/ or .tools/ is not the session's own
/// disk, naming the fix, before it builds xtask.
#[test]
fn in_an_agent_vm_every_tool_call_and_stop_fails_while_a_disk_is_not_its_own() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        fake_cargo(project, "", 0);
        script(&project.join("abc-xtask"), "exit 0\n");
        // What the VM saw in PR 5a: target/ a plain directory of the shared worktree mount.
        fs::create_dir(project.join("target")).unwrap();
        std::os::unix::fs::symlink("/dev", project.join(".tools")).unwrap();
        let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        let denied =
            launch(shell, "pre-tool-use", project, project, input, true).answer(Event::PreToolUse);
        assert!(
            matches!(&denied, Answer::Block(reason) if reason.contains("broken: target/ in ")
                && reason.contains("cargo xtask vm stop")
                && reason.contains("then cargo xtask vm start")),
            "{shell}: {denied:?}"
        );
        assert!(
            !project.join("args").exists(),
            "{shell}: xtask was never built, so nothing landed on the wrong disk"
        );
        for held in 1..=5 {
            let stop = launch(shell, "stop", project, project, "{}", true).answer(Event::Stop);
            assert!(
                matches!(&stop, Answer::Block(reason) if reason.contains("cargo xtask vm stop")),
                "{shell}: hold {held}: {stop:?}"
            );
        }
        let through = launch(shell, "stop", project, project, "{}", true).answer(Event::Stop);
        assert!(
            matches!(&through, Answer::Notify(message) if message.contains("Agent VM storage is broken")
                && message.contains("5 stops in a row were held")),
            "{shell}: the sixth stop goes through, as when xtask cannot start: {through:?}"
        );
        let record = fs::read_to_string(project.join(stop::BACKSTOP)).unwrap();
        assert!(
            record.contains("Agent VM storage is broken"),
            "{shell}: {record}"
        );

        fs::remove_dir(project.join("target")).unwrap();
        fs::remove_file(project.join(".tools")).unwrap();
        let both =
            launch(shell, "pre-tool-use", project, project, input, true).answer(Event::PreToolUse);
        assert!(
            matches!(&both, Answer::Block(reason) if reason.contains("broken: target/ .tools/ in")),
            "{shell}: a missing disk is not its own either: {both:?}"
        );
        assert_eq!(
            launch(shell, "pre-tool-use", project, project, input, false).answer(Event::PreToolUse),
            Answer::Allow,
            "{shell}: outside a VM there are no disks to check"
        );
        own_disks(project);
        assert_eq!(
            launch(shell, "pre-tool-use", project, project, input, true).answer(Event::PreToolUse),
            Answer::Allow,
            "{shell}: with its own disks the VM goes on to xtask"
        );
        assert_eq!(
            launch(shell, "stop", project, project, "{}", true).answer(Event::Stop),
            Answer::Allow,
            "{shell}"
        );
    }
}

/// The launcher ran the worktree's xtask/vm/own-disks, which a branch can edit or break. In a VM
/// it runs the image's `abc-own-disks`, as it runs the image's `abc-xtask vm billing`.
#[test]
fn in_an_agent_vm_the_disk_check_is_the_images() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    fake_cargo(project, "", 0);
    script(&project.join("abc-xtask"), "exit 0\n");
    // Disks the worktree's own copy of the check would pass.
    own_disks(project);
    let asked = project.join("asked");
    script(
        &project.join("abc-own-disks"),
        &format!(
            "echo \"$*\" > {}\necho 'the image says: broken' >&2\nexit 1\n",
            asked.display()
        ),
    );
    let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    assert_eq!(
        launch("sh", "pre-tool-use", project, project, input, true).answer(Event::PreToolUse),
        Answer::Block("the image says: broken".to_owned())
    );
    assert_eq!(
        fs::read_to_string(&asked).unwrap(),
        format!("{}\n", project.display()),
        "the session's worktree"
    );
}

/// An image built before `abc-own-disks` existed cannot check the disks: every tool call is
/// denied, naming the rebuild, rather than fall back to a copy the branch can change.
#[test]
fn in_an_agent_vm_an_image_without_the_disk_check_names_its_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    fake_cargo(project, "", 0);
    script(&project.join("abc-xtask"), "exit 0\n");
    own_disks(project);
    let launcher = crate::testing::checkout().join(LAUNCHER);
    let mut child = Command::new("/bin/sh")
        .arg(&launcher)
        .arg("pre-tool-use")
        // No directory that holds an image's abc-own-disks.
        .env("PATH", format!("{}:/usr/bin:/bin", project.display()))
        .env("CLAUDE_PROJECT_DIR", project)
        .env("ABC_VM", "1")
        .env("ABC_WORKTREE", project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    crate::tools::feed(
        &mut child,
        &mut &br#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#[..],
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let denied = Launched {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
    .answer(Event::PreToolUse);
    assert!(
        matches!(&denied, Answer::Block(reason) if reason.contains("image has no abc-own-disks")
            && reason.contains("cargo xtask vm image")),
        "{denied:?}"
    );
}

#[test]
fn the_own_disks_check_names_a_worktree_that_is_not_there() {
    let output = Command::new("sh")
        .arg(crate::testing::checkout().join("xtask/vm/own-disks"))
        .arg("/no/such/worktree")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "own-disks: /no/such/worktree does not exist\n"
    );
}

#[test]
fn the_launcher_refuses_an_unknown_event() {
    let temp = tempfile::tempdir().unwrap();
    fake_cargo(temp.path(), "", 101);
    let launched = launch("sh", "pre-compact", temp.path(), temp.path(), "{}", false);
    assert_eq!(launched.code, Some(1), "{launched:?}");
    assert!(launched.stdout.is_empty(), "no answer: {launched:?}");
    assert!(launched.stderr.contains("unknown event"), "{launched:?}");
}

/// Every hook command in `.claude/settings.json`, with its event.
fn hook_commands() -> Vec<(String, String)> {
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(crate::testing::checkout().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    settings["hooks"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(event, groups)| {
            groups.as_array().unwrap().iter().flat_map(move |group| {
                group["hooks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(move |hook| (event.clone(), hook["command"].as_str().unwrap().to_owned()))
            })
        })
        .collect()
}

#[test]
fn settings_wire_every_hook_to_the_launcher() {
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(crate::testing::checkout().join(".claude/settings.json")).unwrap(),
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
        "if [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" ]; then sh \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" pre-tool-use; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no xtask/hooks/hook.sh; the git guard is off\" >&2; fi",
        "the guard runs the launcher"
    );
    assert_eq!(guard["timeout"], 300, "the guard is time-boxed");
    assert_eq!(
        hooks["PostToolUse"][0]["matcher"], "Bash",
        "a commit is made with a shell command"
    );
    let checkpoint = &hooks["PostToolUse"][0]["hooks"][0];
    assert_eq!(
        checkpoint["command"],
        "if [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" ]; then sh \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" post-tool-use; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no xtask/hooks/hook.sh; no checkpoint is kept\" >&2; fi",
        "the checkpoint runs the launcher"
    );
    assert_eq!(checkpoint["timeout"], 300, "the checkpoint is time-boxed");
    let stop_command = "if [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" ]; then sh \"$CLAUDE_PROJECT_DIR/xtask/hooks/hook.sh\" stop; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no xtask/hooks/hook.sh; no checks run\" >&2; fi";
    for event in ["Stop", "SubagentStop"] {
        let stop = &hooks[event][0]["hooks"][0];
        assert_eq!(stop["command"], stop_command, "{event} runs the launcher");
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
fn hook_commands_note_a_checkout_without_the_launcher_and_let_it_be() {
    let empty = tempfile::tempdir().unwrap();
    let commands = hook_commands();
    assert_eq!(
        commands.len(),
        4,
        "PreToolUse, PostToolUse, Stop and SubagentStop: {commands:?}"
    );
    for (event, command) in commands {
        for shell in SHELLS {
            let output = Command::new(shell)
                .args(["-c", &command])
                .env("CLAUDE_PROJECT_DIR", empty.path())
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                output.status.code(),
                Some(0),
                "{event} under {shell}: {stderr}"
            );
            assert!(output.stdout.is_empty(), "{event} under {shell}: no answer");
            assert_eq!(stderr.lines().count(), 1, "{event} under {shell}: {stderr}");
            assert!(stderr.starts_with("xtask hook: "), "{event}: {stderr}");
        }
    }
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

/// The real watch step asks the watchdog's claude about the task and hands its verdict on: a drift
/// comes back with its reasons, and an agent on track gets none.
#[test]
fn the_real_watch_step_hands_on_the_watchdogs_verdict() {
    let temp = tempfile::tempdir().unwrap();
    let repo = repository(&temp.path().join("repo"));
    fs::write(
        repo.join("AGENTS.md"),
        "# abc\n\n- **S1 · Correctness over everything.** Text.\n",
    )
    .unwrap();
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "-m", "rules"]);
    crate::task::set(&repo, "Add the coverage gate", &mut std::io::sink()).unwrap();

    let answer = |verdict: &str| {
        let claude = temp.path().join("claude");
        let envelope = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false, "result": verdict,
        });
        script(
            &claude,
            &format!("cat >/dev/null\ncat <<'JSON'\n{envelope}\nJSON\n"),
        );
        let mut steps = super::Real {
            root: &repo,
            claude: &claude,
            failure: None,
        };
        steps.watch(&mut Vec::new()).unwrap()
    };

    let drift = answer(r#"{"on_track": false, "reasons": ["Rewrote the README instead (S1)"]}"#)
        .expect("a drift is handed on");
    assert!(drift.contains("Rewrote the README instead (S1)"), "{drift}");
    assert_eq!(answer(r#"{"on_track": true, "reasons": []}"#), None);
}

/// Class 2 of the /correct report: a session that ended without a handoff left nothing to carry
/// on from. Every stop records what the agent last said, and a commit, seen after the Bash call
/// that made it, renders the checkpoint again.
#[test]
fn stops_and_commits_keep_the_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let repo = crate::testing::clone_with_origin(temp.path());
    let context = context(&repo, None);
    let progress = || fs::read_to_string(repo.join(crate::progress::PROGRESS)).unwrap();
    let post = serde_json::json!({
        "hook_event_name": "PostToolUse", "cwd": repo, "tool_name": "Bash",
        "tool_input": { "command": "git commit -m one" },
    })
    .to_string();

    git(
        &repo,
        &["commit", "--quiet", "--allow-empty", "-m", "Made one"],
    );
    let mut log = Vec::new();
    assert_eq!(
        run(Event::PostToolUse, &post, &context, &mut log),
        Answer::Allow
    );
    assert!(log.is_empty(), "{}", String::from_utf8_lossy(&log));
    assert!(progress().contains(" Made one\n"), "{}", progress());

    let mut steps = Fake::new(Ok(CheckResult::Passed));
    let stop = serde_json::json!({
        "hook_event_name": "Stop", "cwd": repo,
        "last_assistant_message": "Next: the queue.",
    });
    let fields: HookInput = serde_json::from_value(stop).unwrap();
    super::stop_with(&repo, &fields, &mut steps, &mut log);
    assert!(
        progress().contains("> Next: the queue.\n"),
        "{}",
        progress()
    );

    let broken = serde_json::json!({ "hook_event_name": "PostToolUse", "cwd": "/nonexistent" });
    let answer = run(Event::PostToolUse, &broken.to_string(), &context, &mut log);
    assert_eq!(answer, Answer::Allow, "a checkpoint never blocks");
    let noted = String::from_utf8(log).unwrap();
    assert!(noted.contains("the checkpoint was not rendered"), "{noted}");
    assert_eq!(
        run(Event::PostToolUse, "not json", &context, &mut Vec::new()),
        Answer::Allow
    );

    fs::remove_dir_all(repo.join(".abc")).unwrap();
    fs::write(repo.join(".abc"), "a file where the directory goes").unwrap();
    let mut log = Vec::new();
    super::stop_with(
        &repo,
        &fields,
        &mut Fake::new(Ok(CheckResult::Passed)),
        &mut log,
    );
    let noted = String::from_utf8(log).unwrap();
    assert!(noted.contains("the checkpoint was not written"), "{noted}");
}

#[test]
fn the_launcher_lets_a_post_tool_use_stand_whatever_happens() {
    for shell in SHELLS {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        fake_cargo(project, "", 101);
        let failed = launch(shell, "post-tool-use", project, project, "{}", false);
        assert_eq!(failed.code, Some(0), "{shell}: {failed:?}");
        assert!(failed.stdout.is_empty(), "{shell}: {failed:?}");
        assert!(
            failed
                .stderr
                .contains("the checkpoint .abc/progress.md was not updated"),
            "{shell}: {failed:?}"
        );
        fake_cargo(project, "", 0);
        let ran = launch(shell, "post-tool-use", project, project, "{}", false);
        assert_eq!(ran.answer(Event::PostToolUse), Answer::Allow, "{shell}");
        assert_eq!(
            fs::read_to_string(project.join("args")).unwrap().trim_end(),
            format!("{} post-tool-use", XTASK_HOOK.join(" ")),
            "{shell}"
        );
        fs::remove_file(project.join("args")).unwrap();
        script(&project.join("abc-xtask"), "exit 0\n");
        fs::create_dir(project.join("target")).unwrap();
        let vm = launch(shell, "post-tool-use", project, project, "{}", true);
        assert_eq!(vm.code, Some(0), "{shell}: {vm:?}");
        assert!(vm.stdout.is_empty(), "{shell}: {vm:?}");
        assert!(
            !project.join("args").exists(),
            "{shell}: with a disk not its own, xtask is never built"
        );
    }
}
