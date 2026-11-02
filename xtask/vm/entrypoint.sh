#!/bin/sh
# The agent VM's first process, as root (xtask/src/vm/mod.rs). It refuses to go on unless Claude
# Code can use only the subscription token, links the session's git repository where the
# worktree's .git file points, relays the host's token broker from its vsock port to a socket only
# the agent can use, readies the session's volumes, starts the session's Postgres, then sleeps as
# the agent user with no capabilities. Sessions enter through agent-exec.
set -eu
: "${ABC_WORKTREE:?}" "${ABC_BROKER_VSOCK_PORT:?}" "${ABC_GIT_COMMON:?}"
abc-xtask vm billing

as_agent() {
  setpriv --reuid agent --regid agent --init-groups --inh-caps=-all --bounding-set=-all \
    --no-new-privs -- "$@"
}

# The session repository is mounted at /abc/git; the worktree's .git file names the main clone's
# git directory, which the launcher checked lies outside the guest's system directories.
if [ ! -e "$ABC_GIT_COMMON" ]; then
  mkdir -p "$(dirname "$ABC_GIT_COMMON")"
  ln -s /abc/git "$ABC_GIT_COMMON"
fi

# /run/abc is memory. Root keeps the subscription token there (claude-token, which only root can
# read); the agent's broker socket lives in a directory of its own.
chown root:root /run/abc
chmod 711 /run/abc
install -d -o agent -g agent -m 700 /run/abc/agent
as_agent socat "UNIX-LISTEN:/run/abc/agent/broker.sock,fork,mode=600" \
  "VSOCK-CONNECT:2:$ABC_BROKER_VSOCK_PORT" &

# The session's volumes: build output, Linux tools, and the agent's home (Claude Code's state).
chown agent:agent "$ABC_WORKTREE/target" "$ABC_WORKTREE/.tools" /home/agent
if [ ! -e /home/agent/.abc-seeded ]; then
  cp -a /etc/skel/. /home/agent/
  printf '{"hasCompletedOnboarding":true}\n' > /home/agent/.claude.json
  chown -R agent:agent /home/agent
  touch /home/agent/.abc-seeded
fi
if [ ! -e "$ABC_WORKTREE/.tools/.abc-seeded" ]; then
  cp -a /opt/abc-tools/. "$ABC_WORKTREE/.tools/"
  touch "$ABC_WORKTREE/.tools/.abc-seeded"
fi

# Postgres in this VM, data in memory: the superuser only for the postgres OS user (peer
# authentication on the Unix socket), the agent as abc_test, which may only create databases, so
# it cannot run programs as the server (COPY ... TO PROGRAM).
pg=/usr/lib/postgresql/18/bin
as_postgres() { setpriv --reuid postgres --regid postgres --init-groups -- "$@"; }
install -d -o postgres -g postgres -m 700 /var/lib/postgresql /var/lib/postgresql/data
as_postgres "$pg/initdb" -D /var/lib/postgresql/data -U postgres -E UTF8 --no-sync \
  --auth-local=peer --auth-host=reject >/dev/null
cat > /var/lib/postgresql/data/pg_hba.conf <<'HBA'
local all postgres peer
local all abc_test trust
host  all abc_test 127.0.0.1/32 trust
host  all abc_test ::1/128 trust
HBA
echo "CREATE ROLE abc_test LOGIN CREATEDB" \
  | as_postgres "$pg/postgres" --single -D /var/lib/postgresql/data postgres >/dev/null
as_postgres "$pg/pg_ctl" -D /var/lib/postgresql/data -l /var/lib/postgresql/server.log -w -s \
  -o "-c listen_addresses=127.0.0.1 -c unix_socket_directories=/run/postgresql" start

touch /run/abc/ready
exec /usr/local/bin/agent-exec sleep infinity
