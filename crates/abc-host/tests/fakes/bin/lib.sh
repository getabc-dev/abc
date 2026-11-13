# shellcheck shell=sh
# Shared by the fakes in tests/fakes: where the test's scratch tree is, a log of every call, and
# the services the fake service managers run.
#
# A test copies bin/ and programs/ into a scratch directory beside state/ and services/; each
# fake sets ROOT to that directory before sourcing this file.

STATE="$ROOT/state"

# record WORD...: appends one line, the words joined by spaces, to state/calls. A program a
# service manager started (start, below) also says it has started, once its call is in the log.
record() {
	printf '%s\n' "$*" >>"$STATE/calls"
	if [ -n "${FAKE_STARTED:-}" ]; then
		: >"$FAKE_STARTED"
	fi
}

# running NAME: whether the service NAME runs.
running() {
	[ -f "$STATE/$1.pid" ] && kill -0 "$(cat "$STATE/$1.pid")" 2>/dev/null
}

# start NAME: runs state/NAME.args (one word a line) in the background with only state/NAME.env
# (NAME=VALUE lines), state/extra.env (the test's own additions, such as trust in a test CA) and a
# PATH in its environment, as a service manager does, from state/
# rather than the caller's directory (so nothing it writes there lands in the repository), its
# output in state/NAME.log. When the program is one of the fakes in programs/ (a script that sources this file), it returns once the
# program has recorded its call (the program's `record`, through FAKE_STARTED), or has ended, or
# ten seconds have passed, so a service that `running` reports has really started: a manager's
# status never answers for a process still on its way to its program. A real program (the
# tests that run abc itself) records nothing and is not waited for.
start() {
	if running "$1"; then
		return 0
	fi
	service=$1
	rm -f "$STATE/$service.started"
	(
		set --
		while IFS= read -r pair; do
			set -- "$@" "$pair"
		done <"$STATE/$service.env"
		if [ -f "$STATE/extra.env" ]; then
			while IFS= read -r pair; do
				set -- "$@" "$pair"
			done <"$STATE/extra.env"
		fi
		while IFS= read -r word; do
			set -- "$@" "$word"
		done <"$STATE/$service.args"
		cd "$STATE" || exit 1
		exec env -i PATH=/usr/bin:/bin "FAKE_STARTED=$STATE/$service.started" "$@"
	) </dev/null >>"$STATE/$service.log" 2>&1 &
	echo "$!" >"$STATE/$1.pid"
	waited=0
	if ! grep -qs 'bin/lib.sh' "$(head -n 1 "$STATE/$service.args")"; then
		waited=200
	fi
	while [ ! -f "$STATE/$service.started" ] && running "$service" && [ "$waited" -lt 200 ]; do
		sleep 0.05
		waited=$((waited + 1))
	done
}

# stop NAME [SIGNAL]: signals the service (TERM unless named) and waits up to ten seconds for it
# to end, then kills it.
stop() {
	if running "$1"; then
		pid=$(cat "$STATE/$1.pid")
		kill -s "${2:-TERM}" "$pid"
		waited=0
		while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 100 ]; do
			sleep 0.1
			waited=$((waited + 1))
		done
		kill -s KILL "$pid" 2>/dev/null || true
	fi
	rm -f "$STATE/$1.pid"
}
