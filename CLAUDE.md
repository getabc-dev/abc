@AGENTS.md

The strict in-development rules in AGENTS.md (S1 to S7) bind every agent working in this repo,
Claude included.

## Claude Code notes

- **Hooks** live in `.claude/settings.json` and act in the checkout you are working in, worktrees
  included. Stop and SubagentStop run `cargo xtask hook stop`, which is
  `cargo xtask check --changed`; while it fails, you keep working until it passes. Read-only
  subagents (Explore, Plan and the like) and Claude Code's internal agents are exempt. A
  PreToolUse hook on Bash and Monitor runs `xtask/hooks/pre-tool-use.sh`, and through it
  `cargo xtask hook pre-tool-use`, which catches accidental force pushes and git or gh commands it
  cannot read; GitHub's rulesets and the abc-agent App's permissions are the enforcement. If the
  guard itself cannot run, for example while xtask does not build, git and gh commands are blocked
  and everything else still runs. Outside this repository, or in a checkout without these hooks,
  the hooks check nothing.
- Run `cargo xtask setup` once in a new clone or worktree so the hooks find their tools.
- pstack skills are typed as `/pstack:<skill>`.
