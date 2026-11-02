#!/bin/sh
# Claude Code's Stop and SubagentStop hook. It runs `cargo xtask hook stop` in the jail
# (jail.sh), which checks the changed crates and, for the main agent's stop, writes the
# watchdog's prompt. claude needs the user's own login, which the jail keeps out, so this script
# then runs claude outside the jail: Claude Code's cheapest model, no tools, no MCP servers, every
# hook off, and ABC_WATCHDOG set so a stop hook in its session returns at once. Its answer goes
# back into the jail for `cargo xtask watchdog` to judge.
#
# Exit 2 keeps the agent working, with the reason on stderr. The watchdog is advice: when claude
# fails or takes longer than ABC_WATCHDOG_SECONDS (240), it steps aside.
#
# This script is trusted like jail.sh: Claude Code's sandbox cannot write .claude/hooks/, and
# every edit under .claude/ asks the user.

here=$(cd "$(dirname "$0")" && pwd -P)
root=${CLAUDE_PROJECT_DIR:-}
seconds=${ABC_WATCHDOG_SECONDS:-240}

if [ "$(uname -s)" != Darwin ]; then
  echo "xtask stop hook: hooks run repository code only on macOS for now; no checks run" >&2
  exit 0
fi
cd "$root" 2>/dev/null || {
  echo "xtask stop hook: CLAUDE_PROJECT_DIR is not a directory: $root" >&2
  exit 0
}
work=$(mktemp -d "${TMPDIR:-/tmp}/abc-stop.XXXXXX") || {
  echo "xtask stop hook: could not make a temporary directory; keep working" >&2
  exit 2
}
trap 'rm -rf "$work"' EXIT

if ! sh "$here/jail.sh" --share "$work" cargo xtask hook stop --watchdog-prompt "$work/prompt.md" 1>&2; then
  echo 'xtask stop hook: keep working; see stderr'
  exit 2
fi
[ -s "$work/prompt.md" ] || exit 0

ABC_WATCHDOG=1 claude -p --model haiku --output-format json \
  --settings '{"disableAllHooks": true}' --tools "" --strict-mcp-config \
  <"$work/prompt.md" >"$work/answer.json" 2>/dev/null &
claude=$!
(
  sleep "$seconds" &
  timer=$!
  trap 'kill "$timer" 2>/dev/null; exit 0' TERM
  wait "$timer" && kill "$claude" 2>/dev/null
) &
deadline=$!
wait "$claude"
answered=$?
kill "$deadline" 2>/dev/null
if [ "$answered" -ne 0 ]; then
  echo "xtask watchdog: claude failed or took longer than $seconds seconds (exit $answered); skipped" >&2
  exit 0
fi

sh "$here/jail.sh" cargo xtask watchdog <"$work/answer.json" 1>&2
case $? in
  0) exit 0 ;;
  2) exit 2 ;;
  *)
    echo "xtask watchdog: could not judge claude's answer; skipped" >&2
    exit 0
    ;;
esac
