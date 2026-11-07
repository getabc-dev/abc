# Running abc's server

`abc server setup` turns a Mac or a Linux machine with abc installed into abc's server: a private
Postgres 18, abc's own Tailscale node and abc server itself, run as services that survive a
reboot. Nothing else is needed: no Docker, no Postgres or Tailscale of your own, and no change to
the Tailscale you already run.

## Set up

```sh
brew install getabc-dev/tap/abc
abc server setup
```

Setup does this, saying each step as it goes:

1. **Postgres.** It creates a Postgres 18 cluster in abc's home and starts it with no TCP
   listener: only its Unix socket, in a directory only you can enter.
2. **Tailscale.** It starts abc's own `tailscaled`, unprivileged and in userspace, with its own
   state, and joins your tailnet as a new device named `abc`. Your browser opens Tailscale's login
   page for it; log in as yourself. (On a machine without a browser, pass
   `--auth-key-file <file>` with an auth key from the admin console.)
3. **Your password, once.** Tailscale lets only root point Serve at a socket, so setup runs one
   `sudo` command, and prints it and why first. On Linux the same command turns on lingering, so
   your services keep running while you are logged out. That is all root does, and it is never
   asked again.
4. **Services.** It installs and starts the services (below) and waits until abc server is ready.
5. **Claim.** Your browser opens `https://abc.<tailnet>.ts.net` with a setup code filled in. Enter
   your name and a workspace name, and the server is yours.

Run `abc server setup` again at any time: it repairs whatever is missing or changed and duplicates
nothing. If it stops at a step, its message says what to do.

Your tailnet needs MagicDNS and HTTPS certificates turned on (both are on by default for new
tailnets). If your tailnet approves new devices, approve `abc` in the admin console when setup
says so.

## Where everything lives

```text
abc's home                0700  ~/Library/Application Support/abc/server on macOS,
                                ~/.local/share/abc/server on Linux ($XDG_DATA_HOME),
                                or ABC_SERVER_HOME
  config.toml             0600  the public URL, the ops address and the programs' paths
  postgres/               0700  the database
  tailscale/              0700  abc's node: its key and its Serve setting
  run/                    0700  the sockets: Postgres's, tailscaled's and abc's
  logs/                   0700  the services' output (macOS)
  backups/                0700  what abc server backup writes
```

macOS limits socket paths to 104 bytes. If your home directory's path is so long that a socket
would not fit, setup says so: set `ABC_SERVER_HOME` to a shorter directory, such as
`~/.abc-server`, for every `abc server` command.

The programs come from abc's install (`$(brew --prefix)/opt/abc`): abc itself, `libexec/tailscaled`
and `libexec/tailscale` (abc's own, never on your `PATH`) and `libexec/postgresql`, a link to
Homebrew's `postgresql@18`. To use others, set their paths under `[programs]` in `config.toml` and
run `abc server setup` again.

## The services

| | macOS: launchd agents | Linux: systemd user units |
| --- | --- | --- |
| Postgres | `~/Library/LaunchAgents/dev.getabc.postgres.plist` | `~/.config/systemd/user/abc-postgres.service` |
| tailscaled | `dev.getabc.tailscaled.plist` | `abc-tailscaled.service` |
| abc server | `dev.getabc.server.plist` | `abc-server.service`, after the other two |
| Logs | `logs/<name>.log` in abc's home | `journalctl --user -u abc-<name>.service` |

Each starts when you log in (on Linux, at boot, thanks to lingering) and restarts if it fails.
abc server's health and readiness answer on `127.0.0.1:7071` (`/healthz`, `/readyz`); nothing
else listens on TCP.

## Day 2

```sh
abc server status      # each part and, for any that is unhealthy, the command that fixes it
brew upgrade abc && abc server upgrade   # restart on the new version; abc migrates on start
abc server backup [file]                 # a pg_dump of abc's database, private to you
abc server restore [--force] <file>      # load a backup in place of abc's database
abc server setup-code                    # a new setup code, if the claim page's has expired
abc server uninstall [--purge]           # remove the services; --purge deletes the data too
```

`restore` stops abc server, loads the backup into a fresh database, swaps it in and starts abc
server again; a backup that does not load leaves the database as it was. It replaces a server
someone has claimed only with `--force`.

`uninstall` logs abc's node out of your tailnet. Delete the `abc` device in the
[admin console](https://login.tailscale.com/admin/machines) afterwards.

## The backup drill

Practise a restore before you need one, on the same machine or a new one:

1. `abc server backup` and note the file it names. Copy it off the machine.
2. On the machine to restore onto, run `abc server setup` (a new machine) and leave the claim
   page unclaimed.
3. `abc server restore <file>`. On a server already claimed, take a backup of it first, then pass
   `--force`.
4. `abc server status` shows every part healthy; open `https://abc.<tailnet>.ts.net` and check
   that your workspaces and machines are there.

Workers find a restored server at the same URL only if its node keeps the name `abc` on the same
tailnet; on a new machine, delete the old `abc` device first so the new one gets the name.
