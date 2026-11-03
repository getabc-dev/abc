#!/bin/sh
# Claude Code Stop and SubagentStop hook. It runs `cargo xtask hook stop`: `cargo xtask check
# --changed`, the BLOCKED declaration (`cargo xtask task blocked`), the backstop and, for the main
# agent, the watchdog (xtask/src/hook.rs).
#
# In an agent VM it runs the image's installed xtask (`abc-xtask hook stop --standalone`) on the
# project directory, as the git guard does, so a workspace that does not build cannot keep xtask
# from answering; when that xtask cannot run, as in an image that predates the flag, it tries the
# checkout's own, as outside a VM. xtask exits 0 to let the agent stop and 2 to keep it working, with the reason on
# stderr, which Claude Code gives the agent. A stop let through with a handoff prints
# {"systemMessage": ...} on stdout, which Claude Code shows the user; this script passes that line
# on and sends the rest of xtask's stdout to stderr, so nothing else reaches Claude Code's JSON.
#
# When xtask cannot run at all (any other exit), this script keeps D39's promise itself, for the
# main agent's Stop only: it honours a declaration in .abc/blocked.md, unjudged, and logs it in
# .abc/blocked-log.md; otherwise it counts the blocked stop in .abc/stop-blocks and lets the sixth
# in a row through, noted in .abc/stop-backstop.md. A subagent's stop never takes the declaration
# or counts, and stops unchecked with a notice, since holding a stop that never counts would hold
# it with no bound. xtask/src/hook.rs's tests run this script.

input=$(cat)

status=1
if [ "${ABC_VM:-}" = 1 ]; then
  output=$(printf '%s' "$input" | abc-xtask hook stop --standalone)
  status=$?
fi
case $status in
  0 | 2) ;;
  # Outside a VM, or when the image's xtask cannot run (an image older than `hook stop
  # --standalone`), the checkout's own xtask.
  *)
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
# The event, from the hook input's top-level "hook_event_name". A string value holds its quotes
# escaped, so text the agent wrote cannot pose as the field.
event=$(printf '%s' "$input" | tr -d ' \t\r\n' | sed -n 's/.*"hook_event_name":"\([A-Za-z]*\)".*/\1/p')
if [ "$event" != Stop ]; then
  # A subagent's stop never counts, so holding it here would hold it with no bound; the main
  # agent's stop is still held to the check and the count.
  printf '{"systemMessage":"A subagent stopped unchecked: %s."}\n' "$why"
  exit 0
fi

abc="$CLAUDE_PROJECT_DIR/.abc"
mkdir -p "$abc" || exit 2
if [ -f "$abc/blocked.md" ] && grep -q '[^[:space:]]' "$abc/blocked.md"; then
  {
    printf '\n## %s: stop allowed, not judged: %s\n\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$why"
    cat "$abc/blocked.md"
  } >>"$abc/blocked-log.md"
  # The declaration as the inside of a JSON string: tabs become spaces, other control characters
  # but newlines go, backslashes and quotes are escaped, and lines are joined with \n.
  handoff=$(tr '\t\r' '  ' <"$abc/blocked.md" | tr -d '\000-\010\013-\037' |
    sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' |
    awk '{ printf "%s%s", (NR > 1 ? "\\n" : ""), $0 }')
  rm -f "$abc/blocked.md" "$abc/stop-blocks" "$abc/blocked-judged"
  echo "xtask stop hook: $why; BLOCKED, stop allowed" >&2
  printf '{"systemMessage":"BLOCKED, stop allowed (%s). Handoff: %s"}\n' "$why" "$handoff"
  exit 0
fi

count=$(cat "$abc/stop-blocks" 2>/dev/null)
case $count in
  '' | *[!0-9]*) count=0 ;;
esac
if [ "$count" -ge 5 ]; then
  rm -f "$abc/stop-blocks" "$abc/blocked-judged"
  {
    printf '\n## %s: backstop\n\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'The stop hook let a stop through after %s blocked stops in a row, with no BLOCKED declaration (%s).\n' "$count" "$why"
  } >>"$abc/stop-backstop.md"
  printf '{"systemMessage":"%s, and %s stops in a row were blocked with no BLOCKED declaration, so this stop is allowed; recorded in .abc/stop-backstop.md."}\n' "$why" "$count"
  exit 0
fi
printf '%s\n' $((count + 1)) >"$abc/stop-blocks.$$" && mv "$abc/stop-blocks.$$" "$abc/stop-blocks"
echo "xtask stop hook: $why, so the check cannot pass; keep working. If what stops you is" \
  "outside your control, write what is missing and what was tried in .abc/blocked.md" \
  "(cargo xtask task blocked does the same once xtask runs)." >&2
exit 2
