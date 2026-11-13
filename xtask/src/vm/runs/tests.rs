use std::cell::Cell;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use super::outcome::{
    Cause, MAX_ATTEMPTS, MAX_OUTAGE, Next, Outage, Outcome, Verdict, after_attempt, classify, down,
    reset,
};
use super::service::{Manager, Services};
use super::{Clock, Run, Runs, Status, Supervisor};
use crate::testing::script;
use crate::vm::tests::{Fake, FakeServices, World, act_with, host_at, subscribed, world};
use crate::vm::{Action, Backend, Os, QueueAction, Spec};

/// Results recorded from real `claude -p --output-format stream-json` runs this week.
const LIMIT: &str = r#"{"type":"result","subtype":"success","is_error":true,"api_error":"usage_limit_reached","result":"You've hit your 5-hour limit · resets 10:40am (UTC)"}"#;
const DNS: &str = r#"{"type":"result","subtype":"success","is_error":true,"api_error":"connection_failed","result":"API Error: Can't reach the API server — check your internet or DNS (EAI_AGAIN)"}"#;
const HANDOFF: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"**PASS** — all six criteria are met ..."}"#;

/// 2026-11-10 08:00:00 UTC.
const MORNING: i64 = 1_794_297_600;
/// 10:40 UTC that day.
const RESET: i64 = MORNING + 2 * 3600 + 40 * 60;

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn recorded_results_read_as_their_kind_of_stop() {
    assert_eq!(
        classify(Some(&json(LIMIT)), MORNING),
        Outcome::Limit {
            reset: Some(RESET),
            message: "You've hit your 5-hour limit · resets 10:40am (UTC)".to_owned()
        }
    );
    assert_eq!(
        classify(Some(&json(DNS)), MORNING),
        Outcome::Network {
            message: "API Error: Can't reach the API server — check your internet or DNS \
                      (EAI_AGAIN)"
                .to_owned()
        }
    );
    assert_eq!(
        classify(Some(&json(HANDOFF)), MORNING),
        Outcome::Done {
            verdict: Verdict::Pass
        }
    );
    let result = |fields: serde_json::Value| classify(Some(&fields), MORNING);
    assert_eq!(
        result(serde_json::json!({"type": "result", "is_error": false,
            "result": "BLOCKED: on the maintainer; the check PASSes"})),
        Outcome::Done {
            verdict: Verdict::Blocked
        },
        "the first word, whole"
    );
    assert_eq!(
        result(serde_json::json!({"type": "result", "is_error": false, "result": "FAIL: two"})),
        Outcome::Done {
            verdict: Verdict::Fail
        }
    );
    assert_eq!(
        result(serde_json::json!({"type": "result", "is_error": true,
            "result": "getaddrinfo ENOTFOUND api.anthropic.com"})),
        Outcome::Network {
            message: "getaddrinfo ENOTFOUND api.anthropic.com".to_owned()
        },
        "a DNS failure without api_error"
    );
    assert_eq!(
        result(serde_json::json!({"type": "result", "is_error": false,
            "result": "Should I go on?"})),
        Outcome::Unfinished {
            message: "Should I go on?".to_owned()
        }
    );
    assert_eq!(
        result(
            serde_json::json!({"type": "result", "subtype": "error_max_turns",
            "is_error": true})
        ),
        Outcome::Error {
            message: "error_max_turns:".to_owned()
        }
    );
    assert_eq!(
        result(serde_json::json!({"type": "result", "is_error": true,
            "api_error": "invalid_request", "result": "x".repeat(600)})),
        Outcome::Error {
            message: format!("invalid_request: {}", "x".repeat(483))
        },
        "cut to 500 characters"
    );
    assert_eq!(
        classify(None, MORNING),
        Outcome::Error {
            message: "claude ended without a result line".to_owned()
        }
    );
}

#[test]
fn a_limits_reset_is_the_next_such_time() {
    let at = |text: &str, now: i64| reset(&format!("limit · resets {text} (UTC)"), now);
    assert_eq!(at("10:40am", MORNING), Some(RESET));
    assert_eq!(
        at("10:40am", RESET + 2 * 3600),
        Some(RESET + 24 * 3600),
        "a time well past is tomorrow's"
    );
    assert_eq!(at("3pm", MORNING), Some(MORNING + 7 * 3600));
    assert_eq!(at("12am", MORNING), Some(MORNING + 16 * 3600));
    assert_eq!(at("12:30pm", MORNING), Some(MORNING + 4 * 3600 + 30 * 60));
    assert_eq!(
        at("Nov 12, 3pm", MORNING),
        Some(MORNING + 2 * 24 * 3600 + 7 * 3600)
    );
    assert_eq!(
        at("Jan 3, 3pm", MORNING),
        Some(MORNING + 54 * 24 * 3600 + 7 * 3600),
        "a date long past is next year's"
    );
    for unread in [
        "13pm",
        "0am",
        "10:61am",
        "ten",
        "Foo 3, 3pm",
        "Nov 31, 3pm",
        "10:40",
    ] {
        assert_eq!(at(unread, MORNING), None, "{unread}");
    }
    assert_eq!(
        reset("resets 10:40am (Europe/Berlin)", MORNING),
        None,
        "only UTC, the VM's zone"
    );
    assert_eq!(reset("resets 10:40am", MORNING), None, "no zone");
    assert_eq!(reset("no reset named", MORNING), None);
}

#[test]
fn each_kind_of_stop_has_its_restart_rule() {
    let limit = Outcome::Limit {
        reset: Some(RESET),
        message: String::new(),
    };
    assert_eq!(
        after_attempt(&[], &limit, MORNING, None, MORNING),
        Next::Wait {
            until: RESET + 60,
            cause: Cause::Limit,
            outage: None
        }
    );
    let unknown = Outcome::Limit {
        reset: None,
        message: String::new(),
    };
    assert_eq!(
        after_attempt(&[], &unknown, MORNING, None, MORNING),
        Next::Wait {
            until: MORNING + 1800,
            cause: Cause::Limit,
            outage: None
        }
    );
    let network = Outcome::Network {
        message: String::new(),
    };
    let first = Outage {
        since: MORNING,
        tries: 1,
    };
    assert_eq!(
        after_attempt(&[], &network, MORNING, None, MORNING + 5),
        Next::Wait {
            until: MORNING + 35,
            cause: Cause::Network,
            outage: Some(first)
        }
    );
    assert_eq!(
        down(first, MORNING + 35, Cause::Network),
        Next::Wait {
            until: MORNING + 95,
            cause: Cause::Network,
            outage: Some(Outage {
                since: MORNING,
                tries: 2
            })
        },
        "each failed probe doubles the wait"
    );
    assert_eq!(
        down(
            Outage {
                since: MORNING,
                tries: 30
            },
            MORNING + 100,
            Cause::Network
        ),
        Next::Wait {
            until: MORNING + 1000,
            cause: Cause::Network,
            outage: Some(Outage {
                since: MORNING,
                tries: 31
            })
        },
        "up to 15 minutes"
    );
    assert!(
        matches!(down(first, MORNING + MAX_OUTAGE, Cause::Network), Next::GiveUp(reason)
            if reason.contains("unreachable since 2026-11-10 08:00 UTC, 24 hours")),
        "a day without the API ends the run"
    );
}

#[test]
fn failures_are_retried_a_few_times_and_a_handoff_always_ends_the_run() {
    let limit = Outcome::Limit {
        reset: Some(RESET),
        message: String::new(),
    };
    let first = Outage {
        since: MORNING,
        tries: 1,
    };
    let error = Outcome::Error {
        message: "boom".to_owned(),
    };
    let unfinished = Outcome::Unfinished {
        message: "?".to_owned(),
    };
    assert_eq!(
        after_attempt(&[], &error, MORNING, Some(first), MORNING),
        Next::Wait {
            until: MORNING + 60,
            cause: Cause::Retry,
            outage: None
        },
        "a failure ends the outage"
    );
    assert_eq!(
        after_attempt(
            &[limit.clone(), unfinished.clone()],
            &error,
            0,
            None,
            MORNING
        ),
        Next::Wait {
            until: MORNING + 120,
            cause: Cause::Retry,
            outage: None
        }
    );
    assert!(
        matches!(
            after_attempt(&[error.clone(), unfinished], &error, 0, None, MORNING),
            Next::GiveUp(reason) if reason == "3 attempts in a row failed; the last ended with an error: boom"
        ),
        "three failures in a row"
    );
    assert_eq!(
        after_attempt(
            std::slice::from_ref(&error),
            &Outcome::Interrupted,
            0,
            None,
            MORNING
        ),
        Next::Wait {
            until: MORNING,
            cause: Cause::Start,
            outage: None
        },
        "a restarted supervisor carries on at once"
    );
    let done = Outcome::Done {
        verdict: Verdict::Fail,
    };
    let many = vec![limit; MAX_ATTEMPTS];
    assert_eq!(
        after_attempt(&many, &done, 0, None, MORNING),
        Next::Done(Verdict::Fail),
        "a handoff always ends the run"
    );
    assert!(matches!(
        after_attempt(&many[1..], &Outcome::Interrupted, 0, None, MORNING),
        Next::GiveUp(reason) if reason.starts_with("40 attempts, the most a run makes")
    ));
}

/// A clock that jumps to whatever time the supervisor sleeps until, and remembers each.
struct FakeClock {
    now: Cell<i64>,
    slept: std::cell::RefCell<Vec<i64>>,
}

impl FakeClock {
    fn at(now: i64) -> Self {
        Self {
            now: Cell::new(now),
            slept: std::cell::RefCell::default(),
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        let now = self.now.get();
        // Every look at the clock takes a second, so attempts have a length.
        self.now.set(now.saturating_add(1));
        now
    }

    fn sleep_until(&self, at: i64) {
        self.slept.borrow_mut().push(at);
        self.now.set(self.now.get().max(at));
    }
}

/// The fake backend, but each `stream` runs its command's program from `bin` on this host, as
/// `agent-exec` runs it in the VM.
struct Local {
    fake: Fake,
    bin: PathBuf,
}

impl Backend for Local {
    fn is_running(&self, name: &str) -> anyhow::Result<bool> {
        self.fake.is_running(name)
    }

    fn start(&self, spec: &Spec) -> anyhow::Result<()> {
        self.fake.start(spec)
    }

    fn exec(&self, name: &str, command: &[OsString], stdin: &[u8]) -> anyhow::Result<Output> {
        self.fake.exec(name, command, stdin)
    }

    fn attach(&self, name: &str, command: &[OsString], tty: bool) -> anyhow::Result<ExitStatus> {
        self.fake.attach(name, command, tty)
    }

    fn stream(
        &self,
        name: &str,
        command: &[OsString],
        log: &mut dyn std::io::Write,
    ) -> anyhow::Result<ExitStatus> {
        anyhow::ensure!(command[0] == "agent-exec", "as the agent");
        self.fake
            .calls
            .borrow_mut()
            .push(format!("stream {name} {}", command[1].to_string_lossy()));
        let mut child = std::process::Command::new(self.bin.join(&command[1]))
            .args(&command[2..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        // In small pieces, as a stream arrives; stderr after stdout, as one pipe may order them.
        let mut stdout = child.stdout.take().unwrap();
        let mut buffer = [0; 64];
        loop {
            let read = stdout.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            log.write_all(&buffer[..read])?;
        }
        std::io::copy(&mut child.stderr.take().unwrap(), log)?;
        Ok(child.wait()?)
    }

    fn remove(&self, name: &str) -> anyhow::Result<()> {
        self.fake.remove(name)
    }

    fn remove_volumes(&self, names: &[String]) -> anyhow::Result<()> {
        self.fake.remove_volumes(names)
    }

    fn volumes(&self) -> anyhow::Result<Vec<(String, u64)>> {
        self.fake.volumes()
    }

    fn list(&self, running_only: bool) -> anyhow::Result<Vec<super::super::Sandbox>> {
        self.fake.list(running_only)
    }

    fn load_image(&self, archive: &mut dyn Read, tag: &str) -> anyhow::Result<()> {
        self.fake.load_image(archive, tag)
    }
}

/// A fake `claude` in `bin` that answers its n-th call with the n-th of `answers` (the last one
/// for every later call), each a stream-json result, after an init line naming session `s<n>`;
/// it records each call's arguments in `bin/calls`, one per line. An answer `hang` writes the
/// init line and its pid to `bin/pid`, then sleeps.
///
/// Models: `claude -p --output-format stream-json`. Environment: none read. Streams: stream-json
/// lines on stdout, whose results are those recorded from real runs ([`LIMIT`], [`DNS`],
/// [`HANDOFF`]), and a line on stderr. Exit codes: always 0, where the real claude exits non-zero
/// for some errors, so the supervisor reads the result line and never the exit code, which
/// `classify`'s tests pin.
fn fake_claude(bin: &Path, answers: &[&str]) {
    std::fs::create_dir_all(bin).unwrap();
    let mut cases = String::new();
    for (n, answer) in answers.iter().enumerate() {
        let body = if *answer == "hang" {
            "echo $$ > \"$bin/pid\"; exec sleep 60".to_owned()
        } else {
            format!("cat <<'JSON'\n{answer}\nJSON")
        };
        let pattern = if n.saturating_add(1) == answers.len() {
            "*".to_owned()
        } else {
            n.saturating_add(1).to_string()
        };
        std::fmt::Write::write_fmt(&mut cases, format_args!("{pattern})\n{body}\n;;\n")).unwrap();
    }
    script(
        &bin.join("claude"),
        &format!(
            "bin=$(dirname \"$0\")\nn=$(( $(cat \"$bin/count\" 2>/dev/null || echo 0) + 1 ))\n\
             echo $n > \"$bin/count\"\nprintf '%s\\n' \"$*\" >> \"$bin/calls\"\n\
             echo 'some stderr' >&2\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s'$n'\"}}'\n\
             case $n in\n{cases}esac\n"
        ),
    );
}

fn calls(bin: &Path) -> Vec<String> {
    std::fs::read_to_string(bin.join("calls"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A subscribed backend whose VM runs.
fn running() -> Fake {
    let mut fake = subscribed();
    fake.running = true;
    fake
}

/// A world whose worktree holds a brief, and a running VM.
fn briefed() -> (World, Local) {
    let world = world("t");
    std::fs::create_dir_all(world.worktree.join(".abc")).unwrap();
    std::fs::write(world.worktree.join(".abc/brief.md"), "# Brief\n").unwrap();
    let local = Local {
        fake: running(),
        bin: world.root.join("bin"),
    };
    (world, local)
}

fn runs(world: &World) -> Runs {
    Runs {
        dir: world.host.runs.clone(),
    }
}

fn session(world: &World) -> crate::vm::session::Session {
    crate::vm::session::Session::open(&world.worktree, &world.host.repo, &world.host.state).unwrap()
}

/// Starts a run in `world` the way `vm run` does, with `services`, at `MORNING`.
fn started(world: &World, services: &FakeServices) -> Run {
    started_at(world, services, MORNING)
}

fn started_at(world: &World, services: &FakeServices, now: i64) -> Run {
    super::start(
        &runs(world),
        services,
        &session(world),
        ".abc/brief.md",
        now,
    )
    .unwrap()
}

fn supervise(
    world: &World,
    local: &Local,
    services: &FakeServices,
    clock: &FakeClock,
    id: &str,
) -> Status {
    Supervisor {
        runs: &runs(world),
        host: &world.host,
        backend: local,
        services,
        clock,
    }
    .supervise(id)
    .unwrap()
}

/// Class 2 of the /correct report: three runs stopped on a usage limit, and nobody restarted them
/// until the maintainer noticed. The fake `claude` stops on a limit, then hands off after the
/// reset; the supervisor sleeps until the reset and starts a fresh session on the checkpoint.
#[test]
fn a_run_waits_out_a_limit_and_carries_on_fresh_from_the_checkpoint() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[LIMIT, HANDOFF]);
    let services = FakeServices::default();
    let run = started(&world, &services);
    assert_eq!(run.id, "abc-w-20261110T080000");
    assert_eq!(*services.calls.borrow(), [format!("install {}", run.id)]);
    let clock = FakeClock::at(MORNING);

    let status = supervise(&world, &local, &services, &clock, &run.id);

    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    assert_eq!(
        *clock.slept.borrow(),
        [MORNING, RESET + 60],
        "the first attempt at once, the second a minute after the reset"
    );
    let calls = calls(&local.bin);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with("-p Read .abc/brief.md and do the brief to its handoff."),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].starts_with(
            "-p This is a fresh session of a supervised run (attempt 2); the last session ended \
             with a usage limit, reset at 2026-11-10 10:40 UTC. Read .abc/brief.md, then \
             .abc/progress.md, the checkpoint of the work so far, and carry on from its next step"
        ),
        "{}",
        calls[1]
    );
    for call in &calls {
        assert!(
            call.ends_with(
                "--output-format stream-json --verbose --permission-mode bypassPermissions"
            ),
            "{call}"
        );
        assert!(
            !call.contains("--resume"),
            "never a resumed transcript: {call}"
        );
    }
    let state = runs(&world).load(&run.id).unwrap();
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(state.attempts[0].sessions, ["s1"]);
    assert_eq!(state.attempts[1].sessions, ["s2"]);
    assert!(matches!(
        state.attempts[0].outcome,
        Some(Outcome::Limit {
            reset: Some(RESET),
            ..
        })
    ));
    assert_eq!(
        state.attempts[1].outcome,
        Some(Outcome::Done {
            verdict: Verdict::Pass
        })
    );
    assert_eq!(
        *services.calls.borrow(),
        [format!("install {}", run.id), format!("retire {}", run.id)],
        "a run that ended retires its service"
    );
    let log = std::fs::read_to_string(runs(&world).log(&run.id)).unwrap();
    for expected in [
        "--- abc run abc-w-20261110T080000: attempt 1 at 2026-11-10 08:00 UTC\n",
        "some stderr\n",
        LIMIT,
        "attempt 1 ended with a usage limit, reset at 2026-11-10 10:40 UTC\n",
        "attempt 2 at 2026-11-10 10:41 UTC\n",
        HANDOFF,
        "--- abc run abc-w-20261110T080000: done: PASS\n",
    ] {
        assert!(log.contains(expected), "{expected:?} in:\n{log}");
    }
    let stops: Vec<String> = local
        .fake
        .calls
        .borrow()
        .iter()
        .filter(|call| call.starts_with("exec abc-w pkill"))
        .cloned()
        .collect();
    assert_eq!(
        stops, ["exec abc-w pkill -KILL -u agent -f @anthropic-ai/claude-code"; 2],
        "no claude is left running before each attempt"
    );
}

/// Class 2: a night-long DNS outage stopped a run. The supervisor backs off, probes the API from
/// inside the VM, and restarts once it answers.
#[test]
fn a_run_waits_out_an_outage_probing_the_api() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[DNS, HANDOFF]);
    local.fake.offline.set(2);
    let services = FakeServices::default();
    let run = started(&world, &services);
    let clock = FakeClock::at(MORNING);

    let status = supervise(&world, &local, &services, &clock, &run.id);

    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    let slept = clock.slept.borrow().clone();
    assert_eq!(slept.len(), 4, "start, then three probes: {slept:?}");
    assert_eq!(
        slept[1] - slept[0],
        33,
        "30 s after the attempt ended: {slept:?}"
    );
    assert_eq!(slept[2] - slept[1], 60, "the wait doubles: {slept:?}");
    assert_eq!(slept[3] - slept[2], 120, "{slept:?}");
    let probes = local
        .fake
        .calls
        .borrow()
        .iter()
        .filter(|call| call.as_str() == "exec abc-w agent-exec curl --silent --output /dev/null --max-time 20 https://api.anthropic.com/")
        .count();
    assert_eq!(probes, 3, "as the agent, with no token");
    let state = runs(&world).load(&run.id).unwrap();
    assert_eq!(state.outage, None, "over");
    assert_eq!(calls(&local.bin).len(), 2);
    let log = std::fs::read_to_string(runs(&world).log(&run.id)).unwrap();
    assert_eq!(
        log.matches("the API is still unreachable").count(),
        2,
        "{log}"
    );
}

/// A run whose sessions keep failing gives up after three in a row, with its reason, and the
/// queue moves on.
#[test]
fn a_run_that_keeps_failing_gives_up_and_the_next_starts() {
    let (world, local) = briefed();
    fake_claude(
        &local.bin,
        &[r#"{"type":"result","is_error":false,"result":"Shall I?"}"#],
    );
    let services = FakeServices::default();
    let run = started(&world, &services);
    let queued = super::enqueue(
        &runs(&world),
        &services,
        &session(&world),
        ".abc/brief.md",
        MORNING + 5,
    )
    .unwrap();
    assert_eq!(queued.status, Status::Queued, "one run at a time");
    let clock = FakeClock::at(MORNING);

    let status = supervise(&world, &local, &services, &clock, &run.id);

    assert!(
        matches!(&status, Status::GaveUp { reason } if reason.starts_with("3 attempts in a row failed; the last ended with the session ended without a handoff: Shall I?")),
        "{status:?}"
    );
    let next = runs(&world).load(&queued.id).unwrap();
    assert!(
        matches!(
            next.status,
            Status::Waiting {
                cause: Cause::Start,
                ..
            }
        ),
        "the next queued run starts: {:?}",
        next.status
    );
    assert_eq!(
        *services.calls.borrow(),
        [
            format!("install {}", run.id),
            format!("retire {}", run.id),
            format!("install {}", queued.id)
        ]
    );
}

/// The VM is gone after a reboot: the supervisor starts it, with the subscription token from the
/// keychain as `vm start` hands it over, before the attempt. A session that cannot start is a
/// failed attempt.
#[test]
fn an_attempt_starts_the_vm_when_it_is_not_running() {
    let (world, mut local) = briefed();
    local.fake.running = false;
    fake_claude(&local.bin, &[HANDOFF]);
    let services = FakeServices::default();
    let run = started(&world, &services);
    let status = supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    assert_eq!(local.fake.started.borrow().len(), 1, "the VM started");
    let env = &local.fake.started.borrow()[0].env;
    assert!(
        env.contains(&("TZ".to_owned(), "UTC".to_owned())),
        "limits reset in UTC: {env:?}"
    );
}

#[test]
fn a_session_that_cannot_start_is_retried_with_a_growing_wait() {
    let services = FakeServices::default();
    let (world, mut local) = briefed();
    local.fake.running = false;
    // After a reboot the keychain may still be locked: no token comes out of it.
    std::fs::write(world.root.join("security"), "#!/bin/sh\nexit 44\n").unwrap();
    fake_claude(&local.bin, &[HANDOFF]);
    let run = started(&world, &services);
    let clock = FakeClock::at(MORNING);
    let status = supervise(&world, &local, &services, &clock, &run.id);
    assert!(
        matches!(&status, Status::GaveUp { reason } if reason == "the session has not started since 2026-11-10 08:00 UTC, 24 hours"),
        "{status:?}"
    );
    let slept = clock.slept.borrow().clone();
    // Each attempt takes the fake clock two seconds.
    assert_eq!(slept[2] - slept[1], 62, "the wait doubles: {slept:?}");
    assert_eq!(slept[3] - slept[2], 122, "{slept:?}");
    assert_eq!(slept[39] - slept[38], 902, "up to 15 minutes: {slept:?}");
    assert!(
        slept.len() > 90,
        "a day of tries, beyond the cap on sessions: {}",
        slept.len()
    );
    assert!(!local.bin.join("calls").exists(), "claude never ran");
    let state = runs(&world).load(&run.id).unwrap();
    assert!(matches!(
        &state.attempts[0].outcome,
        Some(Outcome::Unstarted { message }) if message.starts_with("no Claude Code subscription token")
    ));
    assert_eq!(
        down(first_outage(), MORNING + MAX_OUTAGE, Cause::Unstarted),
        Next::GiveUp("the session has not started since 2026-11-10 08:00 UTC, 24 hours".to_owned())
    );
}

fn first_outage() -> Outage {
    Outage {
        since: MORNING,
        tries: 1,
    }
}

/// A run whose worktree is gone cannot carry on, whatever restarts its supervisor: it gives up
/// at once rather than fail and be restarted by its service for ever.
#[test]
fn a_run_whose_worktree_is_gone_gives_up() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let run = started(&world, &services);
    std::fs::remove_dir_all(&world.worktree).unwrap();
    let status = supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    assert!(
        matches!(&status, Status::GaveUp { reason } if reason.starts_with("its session cannot be opened: ")),
        "{status:?}"
    );
    assert_eq!(
        services.calls.borrow().last().unwrap(),
        &format!("retire {}", run.id)
    );
    assert_eq!(runs(&world).load(&run.id).unwrap().status, status);
    let listed = runs(&world).load(&run.id).unwrap().status.describe();
    assert!(
        listed.starts_with("gave up: its session cannot be opened"),
        "{listed}"
    );
}

/// The supervisor of a test run in a child process: runs only when a parent test sets
/// `XTASK_TEST_SUPERVISOR` to the world's root and home, which it then kills.
#[test]
fn supervisor_in_a_child_process() {
    let Some(paths) = std::env::var_os("XTASK_TEST_SUPERVISOR") else {
        return;
    };
    // However its parent test ends, this process is gone within a minute.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(60));
        std::process::abort();
    });
    let paths = PathBuf::from(paths);
    let root = paths.join("root");
    let home = paths.join("home");
    let host = host_at(
        &std::fs::read_link(&root).unwrap(),
        &std::fs::read_link(&home).unwrap(),
    );
    let id = std::fs::read_to_string(paths.join("run")).unwrap();
    let local = Local {
        fake: running(),
        bin: host.repo.parent().unwrap().join("bin"),
    };
    Supervisor {
        runs: &Runs {
            dir: host.runs.clone(),
        },
        host: &host,
        backend: &local,
        services: &FakeServices::default(),
        clock: &super::SystemClock,
    }
    .supervise(&id)
    .unwrap();
    panic!("the supervisor returned, but its claude hangs until it is killed");
}

/// A child process killed when dropped, so a failing test leaves none running.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _waited = self.0.wait();
    }
}

/// Class 2: a reboot of the Mac stopped every VM and wiped the logs. A supervisor killed in the
/// middle of an attempt, as a reboot kills it, leaves its run's state on the host; the next
/// supervisor marks that attempt interrupted and carries on from the checkpoint in a fresh session.
#[test]
fn a_killed_supervisor_resumes_from_the_checkpoint() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &["hang", HANDOFF]);
    let services = FakeServices::default();
    // The child's supervisor runs on the system clock.
    let run = started_at(&world, &services, crate::progress::now());
    let paths = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&world.root, paths.path().join("root")).unwrap();
    std::os::unix::fs::symlink(&world.host.home, paths.path().join("home")).unwrap();
    std::fs::write(paths.path().join("run"), &run.id).unwrap();
    let mut child = Killed(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "vm::runs::tests::supervisor_in_a_child_process",
                "--exact",
                "--nocapture",
            ])
            .env("XTASK_TEST_SUPERVISOR", paths.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut running = None;
    for _ in 0..600 {
        let state = runs(&world).load(&run.id).unwrap();
        if state.status == Status::Running
            && state
                .attempts
                .first()
                .is_some_and(|attempt| attempt.sessions == ["s1"])
            && local.bin.join("pid").exists()
        {
            running = Some(state);
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the supervisor is still running"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let running = running.expect("the attempt started and named its session");
    assert_eq!(running.attempts[0].outcome, None, "mid-attempt");
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let claude = std::fs::read_to_string(local.bin.join("pid")).unwrap();
    std::process::Command::new("kill")
        .arg(claude.trim())
        .status()
        .unwrap();

    let clock = FakeClock::at(MORNING + 3600);
    let status = supervise(&world, &local, &services, &clock, &run.id);

    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    let state = runs(&world).load(&run.id).unwrap();
    assert_eq!(state.attempts[0].outcome, Some(Outcome::Interrupted));
    assert_eq!(
        state.attempts[0].sessions,
        ["s1"],
        "the killed session is named"
    );
    let calls = calls(&local.bin);
    assert!(
        calls[1].starts_with(
            "-p This is a fresh session of a supervised run (attempt 2); the last session ended \
             with the supervisor stopped (killed, or the host rebooted). Read .abc/brief.md, then \
             .abc/progress.md"
        ),
        "{}",
        calls[1]
    );
    let log = std::fs::read_to_string(runs(&world).log(&run.id)).unwrap();
    assert!(
        log.contains("the supervisor restarted mid-attempt"),
        "{log}"
    );
}

#[test]
fn one_run_is_active_and_the_queue_starts_the_rest_in_order() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let act = |action| {
        let (result, out) = act_with(&world, &local.fake, &services, action);
        result.map(|_| out).map_err(|error| format!("{error:#}"))
    };
    let add = || {
        act(Action::Queue {
            action: Some(QueueAction::Add {
                worktree: world.worktree.clone(),
                brief: ".abc/brief.md".to_owned(),
            }),
        })
    };
    let first = add().unwrap();
    assert!(first.starts_with("vm: run abc-w-"), "{first}");
    assert!(
        first.contains(": starting at "),
        "no run was active, so it starts: {first}"
    );
    let second = add().unwrap();
    assert!(second.ends_with(": queued\n"), "{second}");
    let refused = act(Action::Run {
        worktree: world.worktree.clone(),
        brief: ".abc/brief.md".to_owned(),
    })
    .unwrap_err();
    assert!(
        refused.contains(" is active in this worktree, and two claudes must never work in one"),
        "{refused}"
    );
    let all = runs(&world).list().unwrap().runs;
    assert_eq!(all.len(), 2);
    let queue = act(Action::Queue { action: None }).unwrap();
    assert!(
        queue.starts_with(&format!("active\t{}\tstarting at ", all[0].id)),
        "{queue}"
    );
    assert!(queue.contains(&format!("\n1\t{}\t", all[1].id)), "{queue}");

    let listed = act(Action::Runs { json: false }).unwrap();
    assert!(listed.contains("\tqueued\t0 attempts\t"), "{listed}");
    assert!(
        listed.contains("/runs/abc-w-"),
        "the log, on the host: {listed}"
    );
    let json: Vec<Run> = serde_json::from_str(&act(Action::Runs { json: true }).unwrap()).unwrap();
    assert_eq!(json, all, "the state files");

    let cancelled = act(Action::Cancel {
        run: all[0].id.clone(),
    })
    .unwrap();
    assert_eq!(
        cancelled,
        format!("vm: cancelled {}; started {}\n", all[0].id, all[1].id)
    );
    assert_eq!(
        runs(&world).load(&all[0].id).unwrap().status,
        Status::Cancelled
    );
    assert!(
        !local
            .fake
            .calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("exec abc-w pkill")),
        "a run waiting to start has no claude to stop"
    );
    let again = act(Action::Cancel {
        run: all[0].id.clone(),
    })
    .unwrap_err();
    assert_eq!(again, format!("run {} has ended: cancelled", all[0].id));
    let unknown = act(Action::Cancel {
        run: "../x".to_owned(),
    })
    .unwrap_err();
    assert!(unknown.contains("../x is not a run"), "{unknown}");
    let left = act(Action::Queue { action: None }).unwrap();
    assert!(
        left.starts_with(&format!("active\t{}\tstarting at ", all[1].id)),
        "{left}"
    );
    assert!(left.ends_with("\nvm queue: nothing queued\n"), "{left}");
    assert_eq!(
        *services.calls.borrow(),
        [
            format!("install {}", all[0].id),
            format!("remove {}", all[0].id),
            format!("install {}", all[1].id),
        ]
    );
}

#[test]
fn a_brief_is_a_file_inside_the_worktree_and_a_failed_install_is_recorded() {
    let (world, _local) = briefed();
    let services = FakeServices::default();
    let start = |brief: &str| {
        super::start(&runs(&world), &services, &session(&world), brief, MORNING)
            .map_err(|error| format!("{error:#}"))
    };
    assert!(
        start("../outside.md")
            .unwrap_err()
            .contains("must be a plain path inside the worktree")
    );
    assert!(
        start(".abc/missing.md")
            .unwrap_err()
            .contains("is not a file in")
    );
    assert!(start("/etc/passwd").unwrap_err().contains("is not in"));
    let absolute = world.worktree.join(".abc/brief.md");
    let kept = start(absolute.to_str().unwrap()).unwrap();
    assert_eq!(kept.brief, ".abc/brief.md", "kept relative");

    let broken = FakeServices {
        broken: true,
        ..FakeServices::default()
    };
    let (other, _other_local) = briefed();
    let error = super::start(
        &runs(&other),
        &broken,
        &session(&other),
        ".abc/brief.md",
        MORNING,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not start: launchctl said no"),
        "{error:#}"
    );
    let all = runs(&other).list().unwrap().runs;
    assert!(
        matches!(&all[0].status, Status::GaveUp { reason } if reason.contains("its service could not be installed")),
        "{:?}",
        all[0].status
    );
    assert!(
        runs(&other)
            .list()
            .unwrap()
            .runs
            .iter()
            .all(|each| !each.status.active()),
        "so it holds no place"
    );
}

/// Runs of the same worktree started in the same second get their own directories.
#[test]
fn runs_started_together_do_not_share_a_directory() {
    let (world, _local) = briefed();
    let services = FakeServices::default();
    let all = runs(&world);
    let first = all
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    let second = all
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    assert_eq!(second.id, format!("{}-2", first.id));
    drop(services);
    std::fs::write(all.dir.join(&first.id).join("state.json"), "{").unwrap();
    assert_eq!(
        all.list().unwrap().unreadable[0].0,
        first.id,
        "a broken state file is reported, not skipped"
    );
}

fn manager(os: Os, dir: &Path) -> Manager {
    let program = dir.join("manager");
    script(
        &program,
        "printf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/managed\"\n",
    );
    let exe = dir.join("xtask");
    std::fs::write(&exe, "the xtask binary").unwrap();
    Manager {
        os,
        units: dir.join("units"),
        program,
        exe,
        bin: dir.join("runs/bin"),
        env: vec![
            ("HOME".to_owned(), "/Users/me & you".to_owned()),
            (
                "PATH".to_owned(),
                "/opt/homebrew/bin:/usr/bin:%x".to_owned(),
            ),
        ],
        runs: dir.join("runs"),
    }
}

fn managed(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("managed")).unwrap_or_default()
}

#[test]
fn a_launchd_agent_runs_the_supervisor_at_login_until_its_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let launchd = manager(Os::MacOs, dir.path());
    launchd.install("abc-w-1").unwrap();
    let copies: Vec<_> = std::fs::read_dir(dir.path().join("runs/bin"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy = &copies[0];
    assert_eq!(std::fs::read_to_string(copy).unwrap(), "the xtask binary");
    let name = copy.file_name().unwrap().to_str().unwrap();
    assert_eq!(name.len(), "xtask-".len() + 32, "{name}");
    let plist =
        std::fs::read_to_string(dir.path().join("units/dev.getabc.run.abc-w-1.plist")).unwrap();
    assert_eq!(plist, launchd.render("abc-w-1", copy));
    for expected in [
        "<key>Label</key><string>dev.getabc.run.abc-w-1</string>",
        &format!(
            "<key>ProgramArguments</key><array><string>{}</string><string>vm</string><string>supervise</string><string>abc-w-1</string></array>",
            copy.display()
        ),
        "<key>EnvironmentVariables</key><dict><key>HOME</key><string>/Users/me &amp; you</string><key>PATH</key><string>/opt/homebrew/bin:/usr/bin:%x</string></dict>",
        "<key>RunAtLoad</key><true/>",
        "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>",
        "/runs/abc-w-1/supervisor.log</string>",
    ] {
        assert!(plist.contains(expected), "{expected} in:\n{plist}");
    }
    let uid = std::process::Command::new("id").arg("-u").output().unwrap();
    let uid = String::from_utf8(uid.stdout).unwrap();
    assert_eq!(
        managed(dir.path()),
        format!(
            "bootstrap gui/{} {}\n",
            uid.trim(),
            dir.path()
                .join("units/dev.getabc.run.abc-w-1.plist")
                .display()
        )
    );
    launchd.install("abc-w-2").unwrap();
    assert_eq!(
        std::fs::read_dir(dir.path().join("runs/bin"))
            .unwrap()
            .count(),
        1,
        "one copy per build"
    );
    launchd.retire("abc-w-1").unwrap();
    assert!(
        !dir.path()
            .join("units/dev.getabc.run.abc-w-1.plist")
            .exists()
    );
    launchd.remove("abc-w-2").unwrap();
    assert!(
        managed(dir.path()).ends_with(&format!(
            "bootout gui/{}/dev.getabc.run.abc-w-2\n",
            uid.trim()
        )),
        "{}",
        managed(dir.path())
    );
    let before = managed(dir.path());
    launchd.remove("abc-w-2").unwrap();
    launchd.retire("abc-w-2").unwrap();
    assert_eq!(
        managed(dir.path()),
        before,
        "a service that is gone is fine"
    );
}

#[test]
fn a_systemd_user_unit_runs_the_supervisor_until_its_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let systemd = manager(Os::Linux, dir.path());
    systemd.install("abc-w-1").unwrap();
    let unit = std::fs::read_to_string(dir.path().join("units/abc-run-abc-w-1.service")).unwrap();
    for expected in [
        "[Unit]\nDescription=abc agent run abc-w-1\n",
        "/runs/bin/xtask-",
        "\" \"vm\" \"supervise\" \"abc-w-1\"\n",
        "Environment=\"HOME=/Users/me & you\"\nEnvironment=\"PATH=/opt/homebrew/bin:/usr/bin:%%x\"\n",
        "KillMode=process\nRestart=on-failure\nRestartSec=60\n\n[Install]\nWantedBy=default.target\n",
        "/runs/abc-w-1/supervisor.log\n",
    ] {
        assert!(unit.contains(expected), "{expected} in:\n{unit}");
    }
    systemd.retire("abc-w-1").unwrap();
    systemd.install("abc-w-2").unwrap();
    systemd.remove("abc-w-2").unwrap();
    assert_eq!(
        managed(dir.path()),
        "--user daemon-reload\n--user enable --now abc-run-abc-w-1.service\n\
         --user disable abc-run-abc-w-1.service\n--user daemon-reload\n\
         --user daemon-reload\n--user enable --now abc-run-abc-w-2.service\n\
         --user stop abc-run-abc-w-2.service\n--user disable abc-run-abc-w-2.service\n\
         --user daemon-reload\n"
    );
    assert!(!dir.path().join("units/abc-run-abc-w-2.service").exists());
    script(&dir.path().join("manager"), "echo nope >&2\nexit 1\n");
    let failed = format!("{:#}", systemd.install("abc-w-3").unwrap_err());
    assert!(
        failed.ends_with("--user daemon-reload failed: nope"),
        "{failed}"
    );
}

#[test]
fn the_service_manager_is_the_hosts() {
    let home = Path::new("/home/me");
    let env = |name: &str| match name {
        "PATH" => Some(OsString::from("/usr/bin")),
        "XDG_CONFIG_HOME" => Some(OsString::from("/cfg")),
        _ => None,
    };
    let linux = Manager::of(Os::Linux, home, Path::new("/state/runs"), &env).unwrap();
    assert_eq!(linux.units, Path::new("/cfg/systemd/user"));
    assert_eq!(linux.program, Path::new("systemctl"));
    assert_eq!(linux.bin, Path::new("/state/runs/bin"));
    assert_eq!(
        linux.env,
        [
            ("HOME".to_owned(), "/home/me".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("XDG_CONFIG_HOME".to_owned(), "/cfg".to_owned())
        ]
    );
    let mac = Manager::of(Os::MacOs, home, Path::new("/state/runs"), &env).unwrap();
    assert_eq!(mac.units, Path::new("/home/me/Library/LaunchAgents"));
    assert_eq!(mac.program, Path::new("launchctl"));
}

/// The host rebooted while the run waited out an outage: the VM is gone, and a probe in it could
/// never answer. The attempt starts the VM instead.
#[test]
fn an_outage_wait_starts_a_vm_that_is_gone_instead_of_probing_it() {
    let (world, mut local) = briefed();
    local.fake.running = false;
    fake_claude(&local.bin, &[DNS, HANDOFF]);
    let services = FakeServices::default();
    let run = started(&world, &services);
    let status = supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    assert!(
        !local
            .fake
            .calls
            .borrow()
            .iter()
            .any(|call| call.contains("curl")),
        "{:?}",
        local.fake.calls.borrow()
    );
    assert_eq!(
        local.fake.started.borrow().len(),
        2,
        "started for each attempt"
    );
}

/// The token broker stopped with an earlier supervisor while the VM ran on: an attempt makes
/// sure one listens, and one that cannot be started is a session that could not start.
#[test]
fn an_attempt_in_a_running_vm_needs_its_token_broker() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[HANDOFF]);
    let files = crate::broker::files(&world.host.home, "abc-w");
    std::fs::remove_file(&files.control).unwrap();
    let services = FakeServices::default();
    let run = started(&world, &services);
    let status = supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    assert!(matches!(status, Status::GaveUp { .. }), "{status:?}");
    let state = runs(&world).load(&run.id).unwrap();
    assert!(
        matches!(&state.attempts[0].outcome, Some(Outcome::Unstarted { message }) if message.contains("the token broker is not installed")),
        "{:?}",
        state.attempts[0].outcome
    );
    assert!(
        !local.bin.join("calls").exists(),
        "claude never ran without a broker"
    );
}

/// A log that cannot be flushed.
struct Unflushable;
impl std::io::Write for Unflushable {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("disk gone"))
    }
}

#[test]
fn the_stream_reads_a_last_line_without_a_newline_and_names_each_session_once() {
    let mut log = Vec::new();
    let mut seen = Vec::new();
    let mut record = |id: &str| {
        seen.push(id.to_owned());
        Ok(())
    };
    let mut stream = super::Stream::new(&mut log, &mut record);
    std::io::Write::write_all(
        &mut stream,
        b"{\"type\":\"system\",\"session_id\":\"a\"}\nnot json\n{\"type\":\"result\",\"is_error\":false,\"result\":\"PASS\"}",
    )
    .unwrap();
    std::io::Write::flush(&mut stream).unwrap();
    let result = stream.finish().unwrap().unwrap();
    assert_eq!(result["result"], "PASS");
    assert_eq!(seen, ["a"]);
    assert!(log.ends_with(b"\"PASS\"}"), "passed on whole");

    let mut unflushable = Unflushable;
    let mut failing = |_: &str| Err(anyhow::anyhow!("state not kept"));
    let mut broken = super::Stream::new(&mut unflushable, &mut failing);
    assert!(
        std::io::Write::flush(&mut broken).is_err(),
        "a flush reaches the log"
    );
    std::io::Write::write_all(&mut broken, b"{\"session_id\":\"b\"}\n").unwrap();
    let error = broken.finish().unwrap_err();
    assert_eq!(
        error.to_string(),
        "state not kept",
        "a session id that was not kept fails"
    );
}

/// The session id that a stream names on its init line and again on later lines is recorded
/// once.
#[test]
fn an_attempt_records_each_session_once() {
    let (world, local) = briefed();
    let repeated = format!(
        "{{\"type\":\"assistant\",\"session_id\":\"s1\"}}\n{}",
        HANDOFF.replace(
            "\"result\":\"**PASS**",
            "\"session_id\":\"s1\",\"result\":\"**PASS**"
        )
    );
    fake_claude(&local.bin, &[&repeated]);
    let services = FakeServices::default();
    let run = started(&world, &services);
    supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    let state = runs(&world).load(&run.id).unwrap();
    assert_eq!(state.attempts[0].sessions, ["s1"]);
}

#[test]
fn cancelling_a_run_mid_attempt_ends_the_attempt_as_interrupted() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let mut run = started(&world, &services);
    run.status = Status::Running;
    run.attempts.push(super::Attempt {
        started: MORNING,
        ended: None,
        sessions: vec!["s1".to_owned()],
        outcome: None,
    });
    runs(&world).save(&run).unwrap();
    super::cancel(
        &runs(&world),
        &services,
        &local.fake,
        &world.host,
        &run.id,
        MORNING + 9,
    )
    .unwrap();
    let state = runs(&world).load(&run.id).unwrap();
    assert_eq!(state.status, Status::Cancelled);
    assert_eq!(state.attempts[0].outcome, Some(Outcome::Interrupted));
    assert_eq!(state.attempts[0].ended, Some(MORNING + 9));
    assert!(
        local
            .fake
            .calls
            .borrow()
            .iter()
            .any(|call| call.contains("pkill")),
        "its claude is stopped"
    );

    let (other, other_local) = briefed();
    let queued = runs(&other)
        .create(&session(&other), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    super::cancel(
        &runs(&other),
        &services,
        &other_local.fake,
        &other.host,
        &queued.id,
        MORNING,
    )
    .unwrap();
    let cancelled = runs(&other).load(&queued.id).unwrap();
    assert_eq!(cancelled.status, Status::Cancelled);
    assert!(
        cancelled.attempts.is_empty(),
        "a queued run has no attempt to end"
    );
}

#[test]
fn runs_are_none_before_the_first_and_a_runs_directory_that_cannot_be_read_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Runs {
        dir: dir.path().join("runs"),
    };
    assert_eq!(
        missing.list().unwrap().runs,
        Vec::<Run>::new(),
        "no runs yet"
    );
    std::fs::write(dir.path().join("runs"), "a file").unwrap();
    missing.list().unwrap_err();
    let (mut out, mut data) = (Vec::new(), Vec::new());
    super::list(
        &Runs {
            dir: dir.path().join("none"),
        },
        false,
        &mut data,
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "vm runs: none\n");
    assert_eq!(data, b"", "a message is not data");
}

#[test]
fn the_system_clock_sleeps_until_the_time_given() {
    let clock = super::SystemClock;
    let before = std::time::Instant::now();
    clock.sleep_until(clock.now() - 5);
    assert!(
        before.elapsed() < std::time::Duration::from_millis(500),
        "a time past returns at once"
    );
    let target = clock.now() + 2;
    clock.sleep_until(target);
    assert!(clock.now() >= target, "not before");
}

#[test]
fn a_supervisor_shows_its_runs_directory() {
    let (world, local) = briefed();
    let all = runs(&world);
    let shown = format!(
        "{:?}",
        Supervisor {
            runs: &all,
            host: &world.host,
            backend: &local,
            services: &FakeServices::default(),
            clock: &super::SystemClock,
        }
    );
    assert!(
        shown.starts_with("Supervisor { runs: Runs { dir: "),
        "{shown}"
    );
}

#[test]
fn each_build_of_xtask_gets_its_own_copy() {
    let dir = tempfile::tempdir().unwrap();
    let launchd = manager(Os::MacOs, dir.path());
    launchd.install("abc-w-1").unwrap();
    std::fs::write(&launchd.exe, "another build").unwrap();
    launchd.install("abc-w-2").unwrap();
    assert_eq!(
        std::fs::read_dir(dir.path().join("runs/bin"))
            .unwrap()
            .count(),
        2,
        "a new build, a new copy"
    );
}

/// Review finding: the first PASS, FAIL or BLOCKED anywhere in the text was taken as the verdict,
/// so a handoff naming a gate's status first ended the run with the wrong one. The verdict is
/// the handoff's first word, as the prompt asks.
#[test]
fn the_verdict_is_the_handoffs_first_word() {
    let result = |text: &str| {
        classify(
            Some(&serde_json::json!({"type": "result", "is_error": false, "result": text})),
            MORNING,
        )
    };
    assert_eq!(
        result("check --changed: PASS, coverage FAIL. Verdict: BLOCKED on the maintainer"),
        Outcome::Unfinished {
            message: "check --changed: PASS, coverage FAIL. Verdict: BLOCKED on the maintainer"
                .to_owned()
        }
    );
    assert_eq!(
        result("I'll make the check PASS next"),
        Outcome::Unfinished {
            message: "I'll make the check PASS next".to_owned()
        }
    );
    for (text, verdict) in [
        (
            "**BLOCKED** — the maintainer must label it; check PASS",
            Verdict::Blocked,
        ),
        ("## FAIL\n\nthe e2e test", Verdict::Fail),
        ("  PASS: all six criteria", Verdict::Pass),
    ] {
        assert_eq!(result(text), Outcome::Done { verdict }, "{text}");
    }
}

/// Review finding: a reset a few minutes past, as when the stop hook ran after the limit
/// message, was read as tomorrow's, so the run waited a day for a limit already over.
#[test]
fn a_reset_just_past_is_over_not_tomorrows() {
    let at = |text: &str, now: i64| reset(&format!("limit · resets {text} (UTC)"), now);
    assert_eq!(at("10:40am", RESET + 300), Some(RESET), "five minutes past");
    assert_eq!(
        after_attempt(
            &[],
            &Outcome::Limit {
                reset: Some(RESET),
                message: String::new()
            },
            RESET - 120,
            None,
            RESET + 300
        ),
        Next::Wait {
            until: RESET + 300,
            cause: Cause::Limit,
            outage: None
        },
        "the next session starts at once"
    );
    assert_eq!(
        at("8am", RESET),
        Some(MORNING + 24 * 3600),
        "a clock time well past is tomorrow's"
    );
    assert_eq!(
        at("Nov 9, 3pm", MORNING),
        Some(MORNING - 24 * 3600 + 7 * 3600),
        "a date days past is over"
    );
}

/// Review finding: an overloaded or failing API, transient as an outage is, failed the run after
/// three tries in three minutes. It is waited out as an outage is, for up to a day.
#[test]
fn an_unavailable_api_is_waited_out_like_an_outage() {
    for fields in [
        serde_json::json!({"type": "result", "is_error": true, "api_error": "overloaded_error", "result": "API Error: 529 Overloaded"}),
        serde_json::json!({"type": "result", "is_error": true, "api_error": "rate_limit_error", "result": "API Error: 429"}),
        serde_json::json!({"type": "result", "is_error": true, "result": "API Error: 503 Service Unavailable"}),
        serde_json::json!({"type": "result", "is_error": true, "result": "API Error: 500 Internal server error"}),
    ] {
        let outcome = classify(Some(&fields), MORNING);
        assert!(
            matches!(&outcome, Outcome::Unavailable { .. }),
            "{fields}: {outcome:?}"
        );
        let next = after_attempt(
            &[outcome.clone(), outcome.clone()],
            &outcome,
            MORNING,
            None,
            MORNING,
        );
        assert!(
            matches!(
                next,
                Next::Wait {
                    cause: Cause::Unavailable,
                    outage: Some(_),
                    ..
                }
            ),
            "{next:?}"
        );
    }
    assert!(matches!(
        down(first_outage(), MORNING + MAX_OUTAGE, Cause::Unavailable),
        Next::GiveUp(reason) if reason.starts_with("the API has failed every session since ")
    ));
}

/// Review finding: an interrupted attempt between two failures reset the count of failures in a
/// row, so a run failing on every session spent the whole cap of 40.
#[test]
fn an_interruption_does_not_reset_the_failures_in_a_row() {
    let error = Outcome::Error {
        message: "boom".to_owned(),
    };
    assert!(matches!(
        after_attempt(
            &[error.clone(), Outcome::Interrupted, error.clone(), Outcome::Interrupted],
            &error,
            0,
            None,
            MORNING
        ),
        Next::GiveUp(reason) if reason.starts_with("3 attempts in a row failed")
    ));
}

/// Review finding: cancelling a queued run killed every claude in its VM, such as the active
/// run's in the same worktree or the maintainer's own `vm shell` session.
#[test]
fn cancelling_a_run_with_no_session_stops_no_claude() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let queued = runs(&world)
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    super::cancel(
        &runs(&world),
        &services,
        &local.fake,
        &world.host,
        &queued.id,
        MORNING,
    )
    .unwrap();
    assert!(
        !local
            .fake
            .calls
            .borrow()
            .iter()
            .any(|call| call.contains("pkill")),
        "{:?}",
        local.fake.calls.borrow()
    );
}

/// Services whose removal finds the supervisor had just ended the run, as when it finished
/// between `vm cancel`'s look at the run and its lock.
struct FinishingServices {
    runs: PathBuf,
}

impl Services for FinishingServices {
    fn install(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove(&self, run: &str) -> anyhow::Result<()> {
        let all = Runs {
            dir: self.runs.clone(),
        };
        let mut ended = all.load(run)?;
        ended.status = Status::Done {
            verdict: Verdict::Pass,
        };
        all.save(&ended)
    }

    fn retire(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Review finding: `vm cancel` checked the run before its lock and overwrote whatever the
/// supervisor had saved meanwhile, losing a handoff.
#[test]
fn a_run_that_ended_while_being_cancelled_keeps_its_end() {
    let (world, local) = briefed();
    let run = started(&world, &FakeServices::default());
    let services = FinishingServices {
        runs: world.host.runs.clone(),
    };
    let said = super::cancel(
        &runs(&world),
        &services,
        &local.fake,
        &world.host,
        &run.id,
        MORNING,
    )
    .unwrap();
    assert_eq!(
        said,
        format!("{} ended before it was cancelled: done: PASS", run.id)
    );
    assert_eq!(
        runs(&world).load(&run.id).unwrap().status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
}

/// Review finding: a cancelled run whose worktree was gone, the usual reason to cancel one,
/// failed before the next queued run started, so the queue stalled.
#[test]
fn cancelling_a_run_whose_worktree_is_gone_still_starts_the_next() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let mut run = started(&world, &services);
    run.status = Status::Running;
    runs(&world).save(&run).unwrap();
    let queued = runs(&world)
        .create(
            &session(&world),
            ".abc/brief.md",
            MORNING + 1,
            Status::Queued,
        )
        .unwrap();
    std::fs::remove_dir_all(&world.worktree).unwrap();
    let error = super::cancel(
        &runs(&world),
        &services,
        &local.fake,
        &world.host,
        &run.id,
        MORNING,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("could not stop its claude"),
        "{error:#}"
    );
    assert_eq!(
        runs(&world).load(&run.id).unwrap().status,
        Status::Cancelled
    );
    assert!(
        services
            .calls
            .borrow()
            .contains(&format!("install {}", queued.id)),
        "the next run started: {:?}",
        services.calls.borrow()
    );
}

/// Review finding: launchd kills a job's process group when the job exits, so a supervisor that
/// ended or restarted took the session's token broker with it.
#[test]
fn launchd_leaves_the_supervisors_broker_running() {
    let dir = tempfile::tempdir().unwrap();
    let plist = manager(Os::MacOs, dir.path()).render("abc-w-1", Path::new("/x"));
    assert!(
        plist.contains("<key>AbandonProcessGroup</key><true/>"),
        "{plist}"
    );
}

/// Review finding: a unit file whose loading failed was left behind, for the service manager to
/// start at the next login although its run had given up.
#[test]
fn a_service_that_does_not_load_leaves_no_unit_file() {
    for os in [Os::MacOs, Os::Linux] {
        let dir = tempfile::tempdir().unwrap();
        let failing = manager(os, dir.path());
        script(&failing.program, "echo 'no GUI session' >&2\nexit 5\n");
        failing.install("abc-w-1").unwrap_err();
        let left: Vec<_> = std::fs::read_dir(dir.path().join("units"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{os:?}: {left:?}");
    }
}

/// Review finding: each build that started a run left a copy of xtask in runs/bin for ever.
/// Installing a service removes the copies no unit file runs.
#[test]
fn copies_of_xtask_no_service_runs_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let launchd = manager(Os::MacOs, dir.path());
    launchd.install("abc-w-1").unwrap();
    launchd.retire("abc-w-1").unwrap();
    std::fs::write(&launchd.exe, "another build").unwrap();
    launchd.install("abc-w-2").unwrap();
    std::fs::write(&launchd.exe, "a third build").unwrap();
    launchd.install("abc-w-3").unwrap();
    let mut copies: Vec<String> = std::fs::read_dir(dir.path().join("runs/bin"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    copies.sort();
    assert_eq!(
        copies,
        ["a third build", "another build"],
        "the first build's copy runs no service any more"
    );
}

/// Review finding: one state file that a run's xtask could not read, as one written by another
/// build, made every listing fail, and with it the queue.
#[test]
fn a_state_that_cannot_be_read_is_shown_and_holds_up_no_queue() {
    let (world, _local) = briefed();
    let services = FakeServices::default();
    let all = runs(&world);
    let broken = all
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    std::fs::write(
        all.dir.join(&broken.id).join("state.json"),
        "{\"status\": \"from-a-newer-build\"}",
    )
    .unwrap();
    let queued = all
        .create(
            &session(&world),
            ".abc/brief.md",
            MORNING + 1,
            Status::Queued,
        )
        .unwrap();
    let started = super::start_next(&all, &services, MORNING).unwrap();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].id, queued.id);
    let (mut out, mut data) = (Vec::new(), Vec::new());
    super::list(&all, false, &mut data, &mut out).unwrap();
    let listed = String::from_utf8(out).unwrap();
    assert!(
        listed.contains(&format!("vm runs: {}: unreadable: ", broken.id)),
        "{listed}"
    );
    assert!(
        !String::from_utf8(data).unwrap().contains(&broken.id),
        "an unreadable run is a message, not a row"
    );
    // The same with --json: the readable runs alone, as a JSON array, on stdout.
    let (mut said, mut json) = (Vec::new(), Vec::new());
    super::list(&all, true, &mut json, &mut said).unwrap();
    let shown: Vec<Run> = serde_json::from_slice(&json).unwrap();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        String::from_utf8(said)
            .unwrap()
            .contains(&format!("vm runs: {}: unreadable: ", broken.id)),
        "said beside the JSON, not in it"
    );
}

/// `vm runs` is for scripts: its listing, as a table or as `--json`, is on stdout alone, and a
/// message is never in it.
#[test]
fn the_runs_listing_is_on_stdout_alone() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let streams = |json| {
        super::super::tests::act_streams(&world, &local.fake, &services, Action::Runs { json })
    };
    let empty_json = streams(true);
    empty_json.result.unwrap();
    assert_eq!(empty_json.stdout, "[]\n", "no run is an empty array");
    assert_eq!(empty_json.stderr, "");
    let empty_table = streams(false);
    empty_table.result.unwrap();
    assert_eq!(empty_table.stdout, "", "no row");
    assert_eq!(empty_table.stderr, "vm runs: none\n", "a message");

    let made = runs(&world)
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    let json = streams(true);
    json.result.unwrap();
    assert_eq!(json.stderr, "", "nothing but the JSON is said");
    let shown: Vec<Run> = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(shown, std::slice::from_ref(&made));
    let table = streams(false);
    table.result.unwrap();
    assert!(table.stdout.starts_with(&made.id), "{}", table.stdout);
    assert_eq!(table.stderr, "");
}

/// Review finding: ended runs' directories, with every session's log, were never removed.
/// `vm gc` lists them and purges them with --yes.
#[test]
fn gc_purges_ended_runs_only_when_told() {
    let (world, _local) = briefed();
    let all = runs(&world);
    let ended = all
        .create(
            &session(&world),
            ".abc/brief.md",
            MORNING,
            Status::Cancelled,
        )
        .unwrap();
    std::fs::write(all.log(&ended.id), "x".repeat(2_000)).unwrap();
    let queued = all
        .create(&session(&world), ".abc/brief.md", MORNING, Status::Queued)
        .unwrap();
    let mut out = Vec::new();
    super::gc(&all, false, &mut out).unwrap();
    let listed = String::from_utf8(out).unwrap();
    assert!(
        listed.contains(&format!("{}\tcancelled\t", ended.id)),
        "{listed}"
    );
    assert!(
        listed.contains("`cargo xtask vm gc --yes` purges them"),
        "{listed}"
    );
    assert!(all.dir.join(&ended.id).exists(), "nothing purged yet");
    let mut purged = Vec::new();
    super::gc(&all, true, &mut purged).unwrap();
    assert!(!all.dir.join(&ended.id).exists());
    assert!(all.dir.join(&queued.id).exists(), "only ended runs go");
}

/// Mutants: what reads an error as the API being unavailable, a date's window for being over,
/// the size gc reports, and a listing that holds only unreadable runs.
#[test]
fn unavailable_errors_dates_and_sizes_are_read_exactly() {
    let error = |fields: serde_json::Value| classify(Some(&fields), MORNING);
    for fields in [
        serde_json::json!({"type": "result", "is_error": true, "api_error": "rate_limit_error", "result": "Rate limited"}),
        serde_json::json!({"type": "result", "is_error": true, "api_error": "overloaded_error", "result": "busy"}),
        serde_json::json!({"type": "result", "is_error": true, "result": "API Error: the server said 503 Service Unavailable"}),
    ] {
        assert!(
            matches!(error(fields.clone()), Outcome::Unavailable { .. }),
            "{fields}"
        );
    }
    assert!(matches!(
        error(
            serde_json::json!({"type": "result", "is_error": true, "result": "API Error: 400 bad request"})
        ),
        Outcome::Error { .. }
    ));
    assert_eq!(
        reset("limit · resets Oct 31, 3pm (UTC)", MORNING),
        Some(MORNING - 10 * 24 * 3600 + 7 * 3600),
        "a date ten days past is over"
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "x".repeat(1_000)).unwrap();
    std::fs::write(dir.path().join("b"), "x".repeat(234)).unwrap();
    assert_eq!(super::size_of(dir.path()), 1_234);
    assert_eq!(super::size_of(&dir.path().join("none")), 0);

    let runs = Runs {
        dir: dir.path().join("runs"),
    };
    std::fs::create_dir_all(runs.dir.join("broken")).unwrap();
    std::fs::write(runs.dir.join("broken/state.json"), "{").unwrap();
    let (mut out, mut data) = (Vec::new(), Vec::new());
    super::list(&runs, false, &mut data, &mut out).unwrap();
    let listed = String::from_utf8(out).unwrap();
    assert!(
        listed.starts_with("vm runs: broken: unreadable: "),
        "{listed}"
    );
    assert!(!listed.contains("vm runs: none"), "{listed}");
}

/// A second linked worktree `v` of `world`'s main clone, holding a brief.
fn second_worktree(world: &World) -> PathBuf {
    let worktree = world.root.join("v");
    let made = std::process::Command::new("git")
        .current_dir(&world.host.repo)
        .args(["worktree", "add", "--quiet", "-b", "second"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    std::fs::create_dir_all(worktree.join(".abc")).unwrap();
    std::fs::write(worktree.join(".abc/brief.md"), "# Brief\n").unwrap();
    worktree
}

fn session_of(world: &World, worktree: &Path) -> crate::vm::session::Session {
    crate::vm::session::Session::open(worktree, &world.host.repo, &world.host.state).unwrap()
}

/// Criterion 1: the number of active runs is a setting, one by default.
#[test]
fn the_parallel_setting_defaults_to_one_and_rejects_nonsense() {
    let (world, local) = briefed();
    let services = FakeServices::default();
    let act = |action| {
        let (result, out) = act_with(&world, &local.fake, &services, action);
        result.map(|_| out).map_err(|error| format!("{error:#}"))
    };
    assert_eq!(
        act(Action::Parallel { count: None }).unwrap(),
        "vm: 1 runs may be active at once\n"
    );
    assert_eq!(
        act(Action::Parallel { count: Some(3) }).unwrap(),
        "vm: 3 runs may be active at once\n"
    );
    assert_eq!(runs(&world).settings().unwrap().parallel, 3, "kept");
    assert_eq!(
        act(Action::Parallel { count: None }).unwrap(),
        "vm: 3 runs may be active at once\n"
    );
    let zero = act(Action::Parallel { count: Some(0) }).unwrap_err();
    assert!(zero.contains("at least one run must be allowed"), "{zero}");

    std::fs::write(world.host.runs.join("settings.json"), "{\"parallel\":0}").unwrap();
    let error = runs(&world).settings().unwrap_err();
    assert!(
        format!("{error:#}").contains("says 0 parallel runs"),
        "{error:#}"
    );
    std::fs::write(
        world.host.runs.join("settings.json"),
        "{\"parallel\":2,\"x\":1}",
    )
    .unwrap();
    runs(&world)
        .settings()
        .expect_err("unknown fields are refused");
    std::fs::remove_file(world.host.runs.join("settings.json")).unwrap();
    std::fs::create_dir(world.host.runs.join("settings.json")).unwrap();
    let unreadable = runs(&world).settings().unwrap_err();
    assert!(
        format!("{unreadable:#}").contains("could not read"),
        "{unreadable:#}"
    );
}

/// Criterion 1: with two slots, two runs are active in two worktrees; a third waits, and so does a
/// second in the same worktree; `vm runs` and `vm queue` show every active run; and raising the
/// number starts what waits.
#[test]
fn runs_start_up_to_the_setting_each_in_its_own_worktree() {
    let (world, local) = briefed();
    let other = second_worktree(&world);
    let services = FakeServices::default();
    let all = runs(&world);
    all.set_parallel(2).unwrap();
    let first = super::start(&all, &services, &session(&world), ".abc/brief.md", MORNING).unwrap();
    let second = super::start(
        &all,
        &services,
        &session_of(&world, &other),
        ".abc/brief.md",
        MORNING,
    )
    .unwrap();
    assert_ne!(first.id, second.id);
    let again =
        super::start(&all, &services, &session(&world), ".abc/brief.md", MORNING).unwrap_err();
    assert!(
        format!("{again:#}").contains("is active in this worktree"),
        "{again:#}"
    );
    all.set_parallel(1).unwrap();
    let full = super::start(
        &all,
        &services,
        &session_of(&world, &other),
        ".abc/brief.md",
        MORNING,
    )
    .unwrap_err();
    // The worktree check comes first; a third worktree meets the number.
    assert!(format!("{full:#}").contains("is active in this worktree"));
    all.set_parallel(2).unwrap();

    let queued =
        super::enqueue(&all, &services, &session(&world), ".abc/brief.md", MORNING).unwrap();
    assert_eq!(queued.status, Status::Queued, "no slot is free");
    all.set_parallel(1).unwrap();
    let third = world.root.join("u");
    let made = std::process::Command::new("git")
        .current_dir(&world.host.repo)
        .args(["worktree", "add", "--quiet", "-b", "third"])
        .arg(&third)
        .output()
        .unwrap();
    assert!(made.status.success());
    std::fs::create_dir_all(third.join(".abc")).unwrap();
    std::fs::write(third.join(".abc/brief.md"), "# Brief\n").unwrap();
    let refused = super::start(
        &all,
        &services,
        &session_of(&world, &third),
        ".abc/brief.md",
        MORNING,
    )
    .unwrap_err();
    assert!(
        format!("{refused:#}")
            .contains("2 runs are active, the most `cargo xtask vm parallel` allows (1)"),
        "{refused:#}"
    );

    let act = |action| {
        let (result, out) = act_with(&world, &local.fake, &services, action);
        result.map(|_| out).map_err(|error| format!("{error:#}"))
    };
    let queue = act(Action::Queue { action: None }).unwrap();
    assert!(
        queue.contains(&format!("active\t{}\t", first.id)),
        "{queue}"
    );
    assert!(
        queue.contains(&format!("active\t{}\t", second.id)),
        "{queue}"
    );
    let listed = act(Action::Runs { json: false }).unwrap();
    for id in [&first.id, &second.id, &queued.id] {
        assert!(listed.contains(id.as_str()), "{listed}");
    }
    let json: Vec<Run> = serde_json::from_str(&act(Action::Runs { json: true }).unwrap()).unwrap();
    assert_eq!(json.len(), 3);

    // A slot frees when a run in the queued one's worktree ends, and raising the number starts
    // runs that wait for a slot.
    let cancelled = act(Action::Cancel {
        run: first.id.clone(),
    })
    .unwrap();
    assert_eq!(
        cancelled,
        format!("vm: cancelled {}\n", first.id),
        "one slot, two active"
    );
    let raised = act(Action::Parallel { count: Some(2) }).unwrap();
    assert_eq!(
        raised,
        format!(
            "vm: 2 runs may be active at once\nvm: started run {}\n",
            queued.id
        )
    );
}

/// Criterion 1: runs share one usage window. A run that meets a limit records its reset, and a
/// run that would start an attempt before it waits for the same reset instead of burning an
/// attempt on the limit.
#[test]
fn a_limit_one_run_met_holds_the_others_until_its_reset() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[HANDOFF]);
    let services = FakeServices::default();
    let all = runs(&world);
    all.note_limit(RESET + 60, MORNING).unwrap();
    all.note_limit(RESET - 600, MORNING).unwrap();
    assert_eq!(
        all.limit(MORNING).unwrap(),
        Some(RESET + 60),
        "an earlier reset never shortens a later one"
    );
    assert_eq!(
        all.limit(RESET + 60).unwrap(),
        None,
        "a reset that has passed is over"
    );
    let run = started(&world, &services);
    let clock = FakeClock::at(MORNING);

    let status = supervise(&world, &local, &services, &clock, &run.id);

    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    assert_eq!(
        *clock.slept.borrow(),
        [MORNING, RESET + 60],
        "it started its first attempt at the shared reset"
    );
    assert_eq!(
        calls(&local.bin).len(),
        1,
        "no attempt was burnt on the limit"
    );
    let log = std::fs::read_to_string(all.log(&run.id)).unwrap();
    assert!(
        log.contains("waiting for the usage limit another run met, until 2026-11-10 10:41 UTC"),
        "{log}"
    );
}

/// Criterion 1: the run that meets the limit publishes its reset for the others.
#[test]
fn a_run_that_meets_a_limit_records_its_reset_for_the_others() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[LIMIT, HANDOFF]);
    let services = FakeServices::default();
    let run = started(&world, &services);
    let clock = FakeClock::at(MORNING);
    supervise(&world, &local, &services, &clock, &run.id);
    let all = runs(&world);
    // The reset has passed by the end; before it, it held.
    assert_eq!(all.limit(MORNING).unwrap(), Some(RESET + 60));
}

/// No file is no limit, not an error.
#[test]
fn no_limit_file_is_no_limit() {
    let (world, _local) = briefed();
    let all = runs(&world);
    assert_eq!(all.limit(MORNING).unwrap(), None);
}

#[test]
fn a_limit_file_that_cannot_be_read_is_an_error() {
    let (world, _local) = briefed();
    let all = runs(&world);
    std::fs::create_dir_all(&world.host.runs).unwrap();
    std::fs::write(world.host.runs.join("limit.json"), "nonsense").unwrap();
    all.limit(MORNING).unwrap_err();
    std::fs::remove_file(world.host.runs.join("limit.json")).unwrap();
    std::fs::create_dir(world.host.runs.join("limit.json")).unwrap();
    all.limit(MORNING).unwrap_err();
}

/// A limit met while the shared file is corrupt replaces it, so sharing works again.
#[test]
fn a_limit_met_over_a_corrupt_file_rewrites_it() {
    let (world, _local) = briefed();
    let all = runs(&world);
    std::fs::create_dir_all(&world.host.runs).unwrap();
    std::fs::write(world.host.runs.join("limit.json"), "nonsense").unwrap();
    all.note_limit(MORNING + 500, MORNING).unwrap();
    assert_eq!(all.limit(MORNING).unwrap(), Some(MORNING + 500));
}

/// Review findings: a corrupt shared limit file stops no supervisor, and a run held by another
/// run's limit that was waiting out an outage still probes the API afterwards.
#[test]
fn a_corrupt_limit_file_stops_no_run_and_an_outage_wait_keeps_its_probe() {
    let (world, local) = briefed();
    fake_claude(&local.bin, &[HANDOFF]);
    let services = FakeServices::default();
    std::fs::create_dir_all(&world.host.runs).unwrap();
    std::fs::write(world.host.runs.join("limit.json"), "nonsense").unwrap();
    let run = started(&world, &services);
    let status = supervise(&world, &local, &services, &FakeClock::at(MORNING), &run.id);
    assert_eq!(
        status,
        Status::Done {
            verdict: Verdict::Pass
        }
    );
    let log = std::fs::read_to_string(runs(&world).log(&run.id)).unwrap();
    assert!(
        log.contains("ignoring the shared usage limit: could not read"),
        "{log}"
    );

    let held = runs(&world);
    std::fs::remove_file(world.host.runs.join("limit.json")).unwrap();
    held.note_limit(RESET, MORNING).unwrap();
    let mut waiting = started(&world, &services);
    waiting.status = Status::Waiting {
        until: MORNING,
        cause: Cause::Network,
    };
    held.save(&waiting).unwrap();
    supervise(
        &world,
        &local,
        &services,
        &FakeClock::at(MORNING),
        &waiting.id,
    );
    let probes = local
        .fake
        .calls
        .borrow()
        .iter()
        .filter(|call| call.contains("curl"))
        .count();
    assert!(
        probes >= 1,
        "the network wait is probed after the limit: {probes}"
    );
}
