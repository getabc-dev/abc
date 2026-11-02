@AGENTS.md

The strict in-development rules in AGENTS.md (S1 to S7) bind every agent working in this repo,
Claude included.

## Claude Code notes

- **Sandbox.** Your Bash commands run in Claude Code's sandbox (`sandbox` in
  `.claude/settings.json`), and you cannot run one outside it. They cannot read `~/.config/gh/`,
  `~/.config/abc/agent-app/`, the keychains or `~/.ssh/`; they write only inside the checkout and
  the sandbox's temporary directory; they reach only GitHub, crates.io, the Rust toolchain's and
  the pinned tools' download hosts, and the agent App's token broker. `~/.cargo` is read-only: if
  a build needs a crate that is not downloaded yet, ask the user to run `cargo fetch`. Never use
  Docker. Sessions need macOS for now: elsewhere every Bash call is blocked.
- **Hooks** live in `.claude/settings.json` and act in the checkout the session started in and the
  worktrees inside it, such as Claude Code's own in `.claude/worktrees/`. Hooks run outside the
  sandbox, so they run repository code only through `.claude/hooks/jail.sh`, which holds it to the
  sandbox's limits with macOS Seatbelt. Stop and SubagentStop run `.claude/hooks/stop.sh`: first
  `cargo xtask check --changed` in the jail, then, for the main agent's Stop, the watchdog. While
  either fails, you keep working. Read-only subagents (Explore, Plan and the like) and Claude
  Code's internal agents are exempt. A PreToolUse hook on Bash and Monitor runs
  `xtask/hooks/pre-tool-use.sh` in the jail, and through it `cargo xtask hook pre-tool-use`, which
  catches accidental force pushes, git or gh commands it cannot read, and gh changes made as the
  user; GitHub's ruleset and the agent App's permissions are the enforcement. If the guard itself
  cannot run, for example while xtask does not build, git and gh commands are blocked and
  everything else still runs. Outside this repository, or in a checkout without these hooks, the
  hooks check nothing. Every edit under `.claude/` asks the user.
- **Watchdog.** Run `cargo xtask task set "<the task>"` when you start a task. When you stop, the
  watchdog runs your own `claude -p` outside the jail, with Haiku, no tools, no MCP servers, hooks
  off and `ABC_WATCHDOG=1` so it cannot recurse, on the task, the rule IDs and the diff. Off
  track, it keeps you working with its reasons, once: a stop that a stop hook already sent back
  (`stop_hook_active`) is never blocked again, and subagents' stops are not judged. With no task
  recorded it only notes that; if claude fails or takes more than four minutes, it steps aside.
- **The agent App.** In your sessions, `.claude/settings.json` sets git's credential helper for
  github.com, through `GIT_CONFIG_*` variables, to `cargo xtask git-credential`, after clearing
  the user's helpers. So git pushes and `cargo xtask gh -- <args>` act as the getabc-dev-agent App,
  with tokens for this repository alone and the permissions each use needs. The tokens come from
  the broker (`cargo xtask broker`), which the user runs outside Claude Code from a build of
  `main`, over `~/.config/abc/agent-broker/broker.sock`; only the broker reads the App's files in
  `~/.config/abc/agent-app/` (`app.json` with `client_id`, `installation_id` and optionally
  `repository`, default `abc`; and `private-key.pem`, which must be mode 600). If the broker is not
  running, ask the user to start it. The user's own terminal keeps the user's credentials. Do not
  use the user's gh login for changes on GitHub.
- **Denied:** `gh auth token`, `gh auth status --show-token`, `security find-*-password`, and
  reading `~/.config/gh/` or `~/.config/abc/agent-app/`. Credentials are not yours to read.
- Run `cargo xtask setup` once in a new clone or worktree so the hooks find their tools.
- pstack skills are typed as `/pstack:<skill>`.
