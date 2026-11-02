#!/bin/sh
# Runs a command from this repository under the macOS Seatbelt profile in jail.sb, for Claude Code
# hooks: jail.sh [--share DIR] COMMAND [ARGUMENT...]
#
# Hooks run outside Claude Code's sandbox, with the user's full access, and xtask and the tests are
# code agents can edit. So every hook runs repository code through this script, which confines it
# the way the sandbox confines agents: no credentials, writes only to the checkout
# ($CLAUDE_PROJECT_DIR) and a fresh temporary directory, no network. --share names one more
# directory the command may write, for the hook to read back.
#
# This script and jail.sb are trusted: Claude Code's sandbox cannot write .claude/hooks/, and
# .claude/settings.json makes every edit under .claude/ ask the user.
#
# It exits 2 without running the command when it cannot confine it: on any system but macOS, or
# inside another sandbox, where Seatbelt cannot apply a second profile. Exit 2 blocks a PreToolUse
# tool call, so agent sessions fail closed there.

here=$(cd "$(dirname "$0")" && pwd -P)
refuse() {
  echo "abc jail: $*; the command did not run" >&2
  exit 2
}

share=
if [ "${1-}" = --share ]; then
  [ $# -ge 3 ] || refuse "usage: jail.sh [--share DIR] COMMAND [ARGUMENT...]"
  share=$(cd "$2" 2>/dev/null && pwd -P) || refuse "--share $2 is not a directory"
  shift 2
fi
[ $# -ge 1 ] || refuse "usage: jail.sh [--share DIR] COMMAND [ARGUMENT...]"
[ "$(uname -s)" = Darwin ] ||
  refuse "hooks run repository code only under macOS Seatbelt, and $(uname -s) has none; abc agent sessions need macOS for now"
[ -x /usr/bin/sandbox-exec ] || refuse "/usr/bin/sandbox-exec is missing"
root=$(cd "${CLAUDE_PROJECT_DIR:-}" 2>/dev/null && pwd -P) && [ -n "${CLAUDE_PROJECT_DIR:-}" ] ||
  refuse "CLAUDE_PROJECT_DIR is not a directory: ${CLAUDE_PROJECT_DIR:-}"
home=$(cd "${HOME:-}" 2>/dev/null && pwd -P) && [ -n "${HOME:-}" ] || refuse "HOME is not a directory"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/abc-jail.XXXXXX") && tmp=$(cd "$tmp" && pwd -P) ||
  refuse "could not make a temporary directory"
trap 'rm -rf "$tmp"' EXIT
[ -n "$share" ] || share=$tmp

jail() {
  TMPDIR="$tmp/" /usr/bin/sandbox-exec -f "$here/jail.sb" \
    -D ROOT="$root" -D HOME="$home" -D TMP="$tmp" -D SHARE="$share" "$@"
}
# Seatbelt cannot apply a profile inside another sandbox; find that out before running anything.
jail /usr/bin/true 2>/dev/null || refuse "Seatbelt could not apply jail.sb (already inside a sandbox?)"
jail "$@"
