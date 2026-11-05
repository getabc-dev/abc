#!/bin/sh
# The launcher of every Claude Code hook in .claude/settings.json: `sh xtask/hooks/hook.sh EVENT`,
# with EVENT pre-tool-use (PreToolUse on Bash and Monitor) or stop (Stop and SubagentStop).
#
# It builds and runs this checkout's own xtask, `cargo xtask hook EVENT` (xtask/src/hook.rs).
# xtask is a Cargo workspace of its own, so no crate of the main workspace can keep it from
# building, and it builds into target/hook, so a hook never waits on the build lock of a cargo
# command the agent runs. xtask answers in Claude Code's hook JSON on stdout and exits 0; this
# script passes the answer on. .claude/settings.json gives a hook 300 seconds (PreToolUse) or 900
# (Stop), enough to build xtask the first time; Claude Code cancels a hook at its timeout, and a
# timed-out hook decides nothing.
#
# Only when xtask cannot start, because it does not build or exits with any status but 0, does
# this script answer itself, also in JSON: for pre-tool-use it denies a command that names git or
# gh and lets any other run; for stop it holds the stop and lets the sixth in a row through,
# recording that in .abc/stop-backstop.md. The holds are counted per agent, in .abc/hook-holds for
# the main agent and .abc/hook-holds.<agent_id> for each subagent, and cleared only by a stop that
# xtask let through. Exit status 3 is xtask's one other word: it ran the check, which failed, but
# could not keep its state to count the hold, so its output is the reason and this script counts
# the hold. That is all the logic it has. In an agent VM, pre-tool-use first runs
# the image's `abc-xtask vm billing`, code the agent cannot change, and denies every tool call
# while it finds a credential other than the subscription token (D37).

event=$1
input=$(cat)

# Prints the answer and ends the hook.
answer() {
  printf '%s\n' "$1"
  exit 0
}

# $1 as the inside of a JSON string: line breaks and tabs become spaces, other control characters
# go, and backslashes and quotes are escaped.
json_text() {
  printf '%s' "$1" | tr '\n\r\t' '   ' | tr -d '\000-\037' | sed -e 's/[\\"]/\\&/g'
}

deny() {
  answer "{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"$(json_text "$1")\"}}"
}

if [ "$event" = pre-tool-use ] && [ "${ABC_VM:-}" = 1 ]; then
  if ! refused=$(abc-xtask vm billing 2>&1); then
    deny "$refused"
  fi
fi

# The agent that stops: the input's agent_id, none for the main agent. A key is the only place an
# unescaped "agent_id" followed by a colon can be, so the match cannot be a value's text.
agent=$(printf '%s' "$input" | tr '\n\r' '  ' |
  sed -n 's/.*"agent_id"[[:space:]]*:[[:space:]]*"\([^"\\]*\)".*/\1/p' |
  tr -c 'A-Za-z0-9_\n-' '_' | cut -c1-100)
holds=$CLAUDE_PROJECT_DIR/.abc/hook-holds
who="the main agent"
if [ -n "$agent" ]; then
  holds=$holds.$agent
  who="subagent $agent"
fi
if cd "$CLAUDE_PROJECT_DIR" 2>/dev/null; then
  output=$(printf '%s' "$input" |
    cargo run --quiet --locked --manifest-path xtask/Cargo.toml --target-dir target/hook -- hook "$event")
  status=$?
else
  echo "xtask hook: CLAUDE_PROJECT_DIR is not a directory: $CLAUDE_PROJECT_DIR" >&2
  status=1
fi
if [ "$status" -eq 0 ]; then
  # A stop that xtask blocked is not a stop let through, so it does not clear the holds.
  if [ "$event" = stop ]; then
    case $output in
      *'"decision":"block"'*) ;;
      *) rm -f "$holds" ;;
    esac
  fi
  if [ -n "$output" ]; then
    printf '%s\n' "$output"
  fi
  exit 0
fi

why="xtask could not start (cargo xtask hook $event exited $status)"
if [ "$event" = stop ] && [ "$status" -eq 3 ]; then
  why=$output
fi
case $event in
  pre-tool-use)
    # Whether the input's tool_input.command names git or gh as a word. The first "command" key
    # is that field: a key can only be an unescaped string, and the other fields hold no such key.
    mentions=$(printf '%s' "$input" | awk '
      { json = json $0 "\n" }
      END {
        start = index(json, "\"command\"")
        if (start == 0) { print "no"; exit }
        rest = substr(json, start + 9)
        if (!match(rest, /^[ \t\r\n]*:[ \t\r\n]*"/)) { print "no"; exit }
        rest = substr(rest, RLENGTH + 1)
        command = ""
        for (i = 1; i <= length(rest); i++) {
          c = substr(rest, i, 1)
          if (c == "\\") {
            i++
            c = substr(rest, i, 1)
            if (c == "n" || c == "t" || c == "r") c = " "
          } else if (c == "\"") {
            break
          }
          command = command c
        }
        n = split(command, words, /[^A-Za-z0-9_.-]+/)
        for (i = 1; i <= n; i++) {
          if (words[i] == "git" || words[i] == "gh") { print "yes"; exit }
        }
        print "no"
      }')
    if [ "$mentions" = yes ]; then
      deny "Blocked: the git guard could not run: $why. git and gh commands are blocked until xtask builds again; commands that do not mention git or gh still run."
    fi
    answer "{\"systemMessage\":\"warning: the git guard could not run: $(json_text "$why"); allowing a command that does not mention git or gh\"}"
    ;;
  stop)
    count=$(cat "$holds" 2>/dev/null)
    case $count in
      '' | *[!0-9]*) count=0 ;;
    esac
    # A hold that cannot be counted is never made, so the cap always holds.
    if [ "$count" -lt 5 ] && mkdir -p "$CLAUDE_PROJECT_DIR/.abc" 2>/dev/null &&
      printf '%s\n' $((count + 1)) >"$holds.$$" 2>/dev/null && mv "$holds.$$" "$holds"; then
      if [ "$status" -eq 3 ]; then
        answer "{\"decision\":\"block\",\"reason\":\"$(json_text "$why") Fix what the check reports. After 5 stops held like this one, the next is let through.\"}"
      fi
      answer "{\"decision\":\"block\",\"reason\":\"$(json_text "$why"), so the check cannot pass. Make xtask build again (cargo build --manifest-path xtask/Cargo.toml). A BLOCKED declaration is judged once it does; after 5 stops held like this one, the next is let through.\"}"
    fi
    rm -f "$holds" "$holds.$$"
    if [ "$count" -ge 5 ]; then
      {
        printf '\n## Unix time %s: launcher backstop\n\n' "$(date +%s)"
        printf 'The stop hook let a stop of %s through after %s blocked stops in a row: %s\n' \
          "$who" "$count" "$why"
      } >>"$CLAUDE_PROJECT_DIR/.abc/stop-backstop.md" 2>/dev/null
    fi
    answer "{\"systemMessage\":\"$(json_text "$why"); $count stops in a row were held, so this stop is allowed with no check.\"}"
    ;;
esac
echo "xtask hook: unknown event '$event'; use pre-tool-use or stop" >&2
exit 1
