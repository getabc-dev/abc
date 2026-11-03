#!/bin/sh
# Claude Code PreToolUse hook for Bash and Monitor. It runs `cargo xtask hook pre-tool-use`, which
# blocks force pushes and git or gh commands it cannot read.
#
# xtask builds into target/hook, apart from target/, so the guard never waits on the build lock of
# a cargo command the agent is running. .claude/settings.json gives this hook 300 seconds, enough to
# build xtask there the first time. Claude Code cancels a hook at its timeout, and a timed-out
# PreToolUse hook does not block the tool call: the guard fails open only then, and GitHub's rules
# still stand.
#
# When xtask itself cannot run, for example because it does not build, this script blocks commands
# that mention git or gh and lets every other command through with a warning, so a broken build
# never locks the agent out of the shell. xtask/src/hook.rs applies the same rule when the guard
# fails inside xtask, and its tests run this script.

input=$(cat)

# In an agent VM, Claude Code may use only the subscription token: the VM's own xtask refuses API
# keys, auth tokens, key helpers, custom endpoints and cloud providers before every tool call, and
# exit 2 blocks the call with its reason.
if [ "${ABC_VM:-}" = 1 ]; then
  abc-xtask vm billing || exit 2
fi
if cd "$CLAUDE_PROJECT_DIR" 2>/dev/null; then
  printf '%s' "$input" | CARGO_TARGET_DIR="$CLAUDE_PROJECT_DIR/target/hook" cargo xtask hook pre-tool-use
  status=$?
else
  echo "CLAUDE_PROJECT_DIR is not a directory: $CLAUDE_PROJECT_DIR" >&2
  status=1
fi
case $status in
  0 | 2) exit "$status" ;;
esac

# Whether the hook input's tool_input.command names git or gh as a word. The first "command" key
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
  echo "Blocked: the git guard could not run (cargo xtask hook exited $status), so git and gh" \
    "commands are blocked until it builds again. Commands that do not mention git or gh still run." >&2
  exit 2
fi
echo "warning: the git guard could not run (cargo xtask hook exited $status); allowing a command" \
  "that does not mention git or gh" >&2
exit 0
