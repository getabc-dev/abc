#!/bin/sh
# The launcher of every Claude Code hook in .claude/settings.json: `sh xtask/hooks/hook.sh EVENT`,
# with EVENT pre-tool-use (PreToolUse on Bash and Monitor), post-tool-use (PostToolUse on Bash,
# which keeps the checkpoint .abc/progress.md) or stop (Stop and SubagentStop).
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
# recording that in .abc/stop-backstop.md; for post-tool-use it says so on stderr and lets the
# action stand, since a checkpoint never decides anything. The holds are counted per agent, in .abc/hook-holds for
# the main agent and .abc/hook-holds.<agent_id> for each subagent, and cleared only by a stop that
# xtask let through. Exit status 3 is xtask's one other word: it ran the check, which failed, but
# could not keep its state to count the hold, so its output is the reason and this script counts
# the hold. That is all the logic it has.
#
# In an agent VM, before xtask is built, the launcher checks what nothing may run without (D37),
# with the image's programs, code the agent cannot change; never the worktree's copies of them,
# which a branch can edit or break. pre-tool-use runs `abc-xtask vm billing`, and denies every
# tool call while it finds a credential other than the subscription token. Both events run
# `abc-own-disks` (built from xtask/vm/own-disks): while target/ or .tools/ is not the session's
# own disk, every tool call is denied and every stop held as when xtask cannot start, with the fix
# as the reason, so no build lands on the worktree mount the VM shares with the host. An image
# without `abc-own-disks` is treated the same way, naming its rebuild. On the host neither check
# applies.

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

# Holds the stop with the reason $2 at most 5 times in a row, counted in $holds; the sixth goes
# through with $1, the cause, recorded in .abc/stop-backstop.md.
hold() {
  count=$(cat "$holds" 2>/dev/null)
  case $count in
    '' | *[!0-9]*) count=0 ;;
  esac
  # A hold that cannot be counted is never made, so the cap always holds.
  if [ "$count" -lt 5 ] && mkdir -p "$CLAUDE_PROJECT_DIR/.abc" 2>/dev/null &&
    printf '%s\n' $((count + 1)) >"$holds.$$" 2>/dev/null && mv "$holds.$$" "$holds"; then
    answer "{\"decision\":\"block\",\"reason\":\"$(json_text "$2")\"}"
  fi
  rm -f "$holds" "$holds.$$"
  if [ "$count" -ge 5 ]; then
    {
      printf '\n## Unix time %s: launcher backstop\n\n' "$(date +%s)"
      printf 'The stop hook let a stop of %s through after %s blocked stops in a row: %s\n' \
        "$who" "$count" "$1"
    } >>"$CLAUDE_PROJECT_DIR/.abc/stop-backstop.md" 2>/dev/null
  fi
  answer "{\"systemMessage\":\"$(json_text "$1"); $count stops in a row were held, so this stop is allowed with no check.\"}"
}

if [ "${ABC_VM:-}" = 1 ]; then
  if [ "$event" = pre-tool-use ] && ! refused=$(abc-xtask vm billing 2>&1); then
    deny "$refused"
  fi
  if ! command -v abc-own-disks >/dev/null 2>&1; then
    broken="This agent VM's image has no abc-own-disks, so the hook cannot check that target/ and \
.tools/ are the session's own disks, and every command fails until it can. On the host, from the \
main clone, run: cargo xtask vm image, then cargo xtask vm stop and cargo xtask vm start for this \
worktree."
    case $event in
      pre-tool-use) deny "$broken" ;;
      stop) hold "$broken" "$broken After 5 stops held like this one, the next is let through." ;;
      post-tool-use) exit 0 ;;
    esac
  fi
  if ! broken=$(abc-own-disks "${ABC_WORKTREE:-$CLAUDE_PROJECT_DIR}" 2>&1); then
    case $event in
      pre-tool-use) deny "$broken" ;;
      stop) hold "$broken" "$broken After 5 stops held like this one, the next is let through." ;;
      post-tool-use) exit 0 ;;
    esac
  fi
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
    if [ "$status" -eq 3 ]; then
      hold "$why" "$why Fix what the check reports. After 5 stops held like this one, the next is let through."
    fi
    hold "$why" "$why, so the check cannot pass. Make xtask build again (cargo build --manifest-path xtask/Cargo.toml). A BLOCKED declaration is judged once it does; after 5 stops held like this one, the next is let through."
    ;;
  post-tool-use)
    echo "xtask hook: $why; the checkpoint .abc/progress.md was not updated" >&2
    exit 0
    ;;
esac
echo "xtask hook: unknown event '$event'; use pre-tool-use, post-tool-use or stop" >&2
exit 1
