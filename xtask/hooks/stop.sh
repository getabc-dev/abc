#!/bin/sh
# Claude Code Stop and SubagentStop hook. It runs `cargo xtask hook stop`: `cargo xtask check
# --changed`, the BLOCKED declaration (`cargo xtask task blocked`), the backstop and, for the main
# agent, the watchdog (xtask/src/hook.rs).
#
# In an agent VM it runs the image's installed xtask (`abc-xtask hook stop --standalone`) on the
# project directory, as the git guard does, so a workspace that does not build cannot keep xtask
# from answering. Only when that xtask cannot run (exit 126 or 127, or the usage error of an image
# that predates the flag) does it try the checkout's own, as outside a VM. xtask exits 0 to let
# the agent stop and 2 to keep it working, with the reason on stderr, which Claude Code gives the
# agent; it turns every error of its own into the latter, counted. A stop let through with a
# handoff prints {"systemMessage": ...} on stdout, which Claude Code shows the user; this script
# passes that line on and sends the rest of xtask's stdout to stderr, so nothing else reaches
# Claude Code's JSON.
#
# When no xtask can run (or one dies with any other status), this script keeps the backstop
# itself: it holds the stop and counts it, the main agent's in .abc/stop-blocks and each
# subagent's, by agent_id, in .abc/subagent-stops/, and lets the sixth in a row through. The main
# agent's is noted in .abc/stop-backstop.md. A BLOCKED declaration is never honoured here, since
# nothing can judge it: it stays in .abc/blocked.md, is held and counted like any stop, and goes
# out in the handoff of the stop the backstop lets through, logged in .abc/blocked-log.md. The
# files are in the checkout the hook input's cwd is in, as xtask finds it. xtask/src/hook.rs's
# tests run this script.

input=$(cat)

# A string field of the hook input's top level. A string value holds its quotes escaped, so text
# the agent wrote cannot pose as a field.
field() {
  printf '%s' "$input" | tr -d '\r\n' |
    sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\\([^\"\\\\]*\\)\".*/\\1/p"
}

if errors=$(mktemp 2>/dev/null); then
  trap 'rm -f "$errors"' EXIT
else
  errors=/dev/null
fi

status=127
if [ "${ABC_VM:-}" = 1 ]; then
  output=$(printf '%s' "$input" | abc-xtask hook stop --standalone 2>"$errors")
  status=$?
  # An image older than the flag answers with clap's usage error, which exits 1 (or 2, like a
  # block); only its message tells it from xtask's own failures.
  if { [ "$status" -eq 1 ] || [ "$status" -eq 2 ]; } && grep -q "unexpected argument '--standalone' found" "$errors"; then
    status=127
  else
    cat "$errors" >&2
  fi
fi
case $status in
  # xtask could not run: outside a VM, or in one whose image cannot run it. The checkout's own.
  126 | 127)
    if cd "$CLAUDE_PROJECT_DIR" 2>/dev/null; then
      output=$(printf '%s' "$input" | cargo xtask hook stop)
      status=$?
    else
      echo "CLAUDE_PROJECT_DIR is not a directory: $CLAUDE_PROJECT_DIR" >&2
      output=
      status=1
    fi
    ;;
esac
if [ -n "$output" ]; then
  printf '%s\n' "$output" | grep -v '^{"systemMessage":' >&2
  printf '%s\n' "$output" | grep '^{"systemMessage":' | tail -n 1
fi
case $status in
  0 | 2) exit "$status" ;;
esac

why="the stop hook's xtask could not run (it exited $status)"

# The checkout the agent is in, found as xtask's locate does: the root of the input's cwd, or of
# the project directory without one.
toplevel() { (unset GIT_DIR GIT_WORK_TREE; git -C "$1" rev-parse --show-toplevel 2>/dev/null); }
commondir() {
  (unset GIT_DIR GIT_WORK_TREE; git -C "$1" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
}
start=$(field cwd)
[ -n "$start" ] && [ -d "$start" ] || start=$CLAUDE_PROJECT_DIR
root=$(toplevel "$start")
if [ -z "$root" ]; then
  echo "xtask stop hook: $start is not in a git repository; no checks run" >&2
  exit 0
fi
if [ "$(commondir "$root")" != "$(commondir "$CLAUDE_PROJECT_DIR")" ]; then
  echo "xtask stop hook: $root is not a checkout of this repository; no checks run" >&2
  exit 0
fi

abc="$root/.abc"
main=
if [ "$(field hook_event_name)" = Stop ]; then
  main=1
  mkdir -p "$abc" || exit 2
  counter=$abc/stop-blocks
else
  # The same file name as xtask's task::subagent_file.
  key=$(printf '%s' "$(field agent_id)" | tr -c 'A-Za-z0-9_-' '_')
  mkdir -p "$abc/subagent-stops" || exit 2
  counter=$abc/subagent-stops/${key:-subagent}
fi
declared=
if [ -n "$main" ] && [ -f "$abc/blocked.md" ] && grep -q '[^[:space:]]' "$abc/blocked.md"; then
  declared=1
fi

count=$(cat "$counter" 2>/dev/null)
case $count in
  '' | *[!0-9]*) count=0 ;;
esac
if [ "$count" -ge 5 ]; then
  rm -f "$counter"
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  if [ -n "$declared" ]; then
    {
      printf '\n## %s: stop allowed, not judged: %s; %s blocked stops in a row\n\n' "$now" "$why" "$count"
      cat "$abc/blocked.md"
    } >>"$abc/blocked-log.md"
    # The declaration as the inside of a JSON string: tabs become spaces, other control
    # characters but newlines go, backslashes and quotes are escaped, and lines are joined with \n.
    handoff=$(tr '\t\r' '  ' <"$abc/blocked.md" | tr -d '\000-\010\013-\037' |
      sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' |
      awk '{ printf "%s%s", (NR > 1 ? "\\n" : ""), $0 }')
    rm -f "$abc/blocked.md"
    echo "xtask stop hook: $why; BLOCKED, stop allowed after $count blocked stops" >&2
    printf '{"systemMessage":"BLOCKED, stop allowed (%s; %s stops in a row were blocked and the declaration could not be judged). Handoff: %s"}\n' "$why" "$count" "$handoff"
  elif [ -n "$main" ]; then
    {
      printf '\n## %s: backstop\n\n' "$now"
      printf 'The stop hook let a stop through after %s blocked stops in a row, with no BLOCKED declaration (%s).\n' "$count" "$why"
    } >>"$abc/stop-backstop.md"
    printf '{"systemMessage":"%s, and %s stops in a row were blocked with no BLOCKED declaration, so this stop is allowed; recorded in .abc/stop-backstop.md."}\n' "$why" "$count"
  else
    printf '{"systemMessage":"A subagent stop was held %s times in a row (%s), so this stop is allowed."}\n' "$count" "$why"
  fi
  exit 0
fi
printf '%s\n' $((count + 1)) >"$counter.$$" && mv "$counter.$$" "$counter"
echo "xtask stop hook: $why, so the check cannot pass; keep working." >&2
if [ -n "$declared" ]; then
  echo "Your BLOCKED declaration in .abc/blocked.md was not judged, since nothing could; it stays" \
    "and counts as a blocked stop. The stop is allowed with it after 5 in a row." >&2
elif [ -n "$main" ]; then
  echo "If what stops you is outside your control, write what is missing and what was tried in" \
    ".abc/blocked.md (cargo xtask task blocked does the same once xtask runs)." >&2
fi
exit 2
