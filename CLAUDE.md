@AGENTS.md

The strict in-development rules in AGENTS.md (S1 to S7) bind every agent working in this repo,
Claude included.

## Claude Code notes

- **Hooks** live in `.claude/settings.json` and act in the checkout you are working in, worktrees
  included. Every hook runs `xtask/hooks/hook.sh`, which builds this checkout's own xtask into
  `target/hook` and runs `cargo xtask hook <event>`. xtask is a Cargo workspace of its own, so a
  crate of the main workspace that does not build cannot block the guard or trap a stop. It
  answers in Claude Code's hook JSON and exits 0, so no exit code, usage error or crash can read
  as a decision. Stop and SubagentStop run `cargo xtask check --changed`, then, for the main
  agent's first Stop since one was let through, the watchdog. While either fails you keep working,
  but a stop can end honestly (D39): PASS, FAIL or BLOCKED. If the task cannot be finished, run
  `cargo xtask task blocked "<what is missing, what was tried>"`. Your next Stop logs the
  declaration in `.abc/blocked-log.md` and asks the watchdog one question: is the blocker outside
  your control? A failing check or flaky tests never is. If it says no, the stop is sent back with
  its reason, and every declaration is judged the same way; otherwise the stop is allowed and the
  handoff is shown to the user. A `check` that cannot run counts as a failed one; a stop while
  another gate holds the gate lock (`target/gate.lock`) does not start `check` and is sent back to
  wait for that gate, as a blocked stop; a refused declaration counts as a blocked stop, and after
  5 blocked stops in a row the hook lets the next stop through (a declaration with its handoff,
  unjudged; otherwise noted in `.abc/stop-backstop.md`). One pure state machine decides every stop (`xtask/src/stop.rs`), on one
  record per agent in `.abc/stop-state.json`: the main agent's count and declaration, and each
  subagent's count by its `agent_id`, which no subagent's stop can reach past. The main count
  resets when `check` passes, a declaration is accepted or a new task is set. When xtask cannot
  start, the launcher answers itself: it holds a stop at most 5 times in a row
  (`.abc/hook-holds`), never honouring a declaration, and denies git and gh commands while letting
  everything else run. Read-only subagents (Explore, Plan and the like) and Claude Code's internal
  agents are exempt. The PreToolUse hook on Bash and Monitor is the git guard: it catches
  accidental force pushes, git or gh commands it cannot read, gh changes made as the user,
  and pull requests opened or edited with gh rather than `cargo xtask pr`;
  GitHub's ruleset and the agent App's permissions are the enforcement. In an agent VM it first
  runs the image's `abc-xtask vm billing`, the one check made with code the agent cannot change.
  In an agent VM, both hooks also run `xtask/vm/own-disks` first: while `target/` or `.tools/` is
  not the session's own disk, every tool call is denied and every stop held, naming the fix
  (`vm stop` then `vm start` on the host). Never delete or move `target/` or `.tools/`. Outside this repository, or in a checkout without these hooks, the hooks check nothing.
- **Watchdog.** Run `cargo xtask task set "<the task>"` when you start a task. When you stop, the
  watchdog runs your own `claude -p` with Haiku, no tools, hooks off and `ABC_WATCHDOG=1` so it
  cannot recurse, on the task, the rule IDs and the diff. Off track, it keeps you working with its
  reasons, once: a stop that a stop hook already sent back (`stop_hook_active`) is never blocked
  again, and subagents' stops are not judged. With no task recorded it only notes that; if claude
  fails or times out, it steps aside.
- **Agent VMs.** Sessions run in a microVM per worktree (`cargo xtask vm`, D37). You see only
  this worktree and a git repository holding its branch and `origin/main`; the network reaches
  only `xtask/vm/egress-allow.txt`; Postgres 18 runs in the VM on 127.0.0.1 as `abc_test`
  (`ABC_TEST_DATABASE_URL` is set). git pushes, `cargo xtask gh -- <args>` and `cargo xtask pr`
  act as the getabc-dev-agent App, with a token per use from the host's broker
  (`ABC_AGENT_BROKER`), scoped to this repository and the permissions each use needs. Open and
  update pull requests with `cargo xtask pr`, and read a pull request's failures with
  `cargo xtask pr checks <number>`: job logs are out of the VM's reach. Push only `agent/<topic>` branches;
  GitHub refuses the App any other branch, any tag and any workflow change. Claude Code here runs on the
  maintainer's subscription token only; the VM refuses any API key or other route, before every
  tool call too.
- Run `cargo xtask setup` once in a new clone or worktree so the hooks find their tools.
- pstack skills are typed as `/pstack:<skill>`.
