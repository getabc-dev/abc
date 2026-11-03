@AGENTS.md

The strict in-development rules in AGENTS.md (S1 to S7) bind every agent working in this repo,
Claude included.

## Claude Code notes

- **Hooks** live in `.claude/settings.json` and act in the checkout you are working in, worktrees
  included. Stop and SubagentStop run `cargo xtask hook stop`: first `cargo xtask check --changed`,
  then, for the main agent's Stop, the watchdog. While either fails, you keep working. Read-only subagents (Explore, Plan and
  the like) and Claude Code's internal agents are exempt. A PreToolUse hook on Bash and Monitor
  runs `xtask/hooks/pre-tool-use.sh`, and through it `cargo xtask hook pre-tool-use`, which catches
  accidental force pushes, git or gh commands it cannot read, and gh changes made as the user;
  GitHub's ruleset and the agent App's permissions are the enforcement. If the guard itself cannot
  run, for example while xtask does not build, git and gh commands are blocked and everything else
  still runs. Outside this repository, or in a checkout without these hooks, the hooks check
  nothing.
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
