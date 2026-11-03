@AGENTS.md

The strict in-development rules in AGENTS.md (S1 to S7) bind every agent working in this repo,
Claude included.

## Claude Code notes

- **Hooks** live in `.claude/settings.json` and act in the checkout you are working in, worktrees
  included. Stop and SubagentStop run `xtask/hooks/stop.sh`, and through it `cargo xtask hook
  stop`: `cargo xtask check --changed`, then, for the main agent's first Stop, the watchdog. While
  either fails you keep working, but a stop can end honestly (D39): PASS, FAIL or BLOCKED. If the
  task cannot be finished, run `cargo xtask task blocked "<what is missing, what was tried>"`. Your
  next Stop logs the declaration in `.abc/blocked-log.md` and asks the watchdog one question: is
  the blocker outside your control? A failing check or flaky tests never is. If it says no, the
  stop is sent back with its reason, and every declaration is judged the same way; otherwise the
  stop is allowed and the handoff is shown to the user. A `check` that cannot run counts as a
  failed one, a refused declaration counts as a blocked stop, and after 5 blocked stops in a row
  the hook lets the next stop through (a declaration with its handoff, unjudged; otherwise noted in
  `.abc/stop-backstop.md`). The count lives in `.abc/` and resets when `check` passes, a
  declaration is accepted or a new task is set. Only the main agent's Stop takes the declaration
  and uses the main count; each subagent's blocked stops are counted apart, by its `agent_id`, and
  its stop is let through after 5 as well. When xtask cannot run, `stop.sh` holds and counts the
  stop itself and never honours a declaration unjudged. Read-only subagents
  (Explore, Plan and the like) and Claude Code's internal agents are exempt. A PreToolUse hook on
  Bash and Monitor runs `xtask/hooks/pre-tool-use.sh`, and through it `cargo xtask hook
  pre-tool-use`, which catches accidental force pushes, git or gh commands it cannot read, and gh
  changes made as the user; GitHub's ruleset and the agent App's permissions are the enforcement.
  If the guard itself cannot run, git and gh commands are blocked and everything else still runs.
  In an agent VM both hooks run the image's installed `abc-xtask` with `--standalone`, so a
  checkout whose workspace does not build cannot block the guard or trap a stop. Outside this
  repository, or in a checkout without these hooks, the hooks check nothing.
- **Watchdog.** Run `cargo xtask task set "<the task>"` when you start a task. When you stop, the
  watchdog runs your own `claude -p` with Haiku, no tools, hooks off and `ABC_WATCHDOG=1` so it
  cannot recurse, on the task, the rule IDs and the diff. Off track, it keeps you working with its
  reasons, once: a stop that a stop hook already sent back (`stop_hook_active`) is never blocked
  again, and subagents' stops are not judged. With no task recorded it only notes that; if claude
  fails or times out, it steps aside.
- **Agent VMs.** Sessions run in a microVM per worktree (`cargo xtask vm`, D37). You see only
  this worktree and a git repository holding its branch and `origin/main`; the network reaches
  only `xtask/vm/egress-allow.txt`; Postgres 18 runs in the VM on 127.0.0.1 as `abc_test`
  (`ABC_TEST_DATABASE_URL` is set). git pushes and `cargo xtask gh -- <args>` act as the
  getabc-dev-agent App, with a token per use from the host's broker (`ABC_AGENT_BROKER`), scoped
  to this repository and the permissions each use needs. Push only `agent/<topic>` branches;
  GitHub refuses the App any other branch, any tag and any workflow change. Claude Code here runs on the
  maintainer's subscription token only; the VM refuses any API key or other route, before every
  tool call too.
- Run `cargo xtask setup` once in a new clone or worktree so the hooks find their tools.
- pstack skills are typed as `/pstack:<skill>`.
