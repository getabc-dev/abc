# abc

abc is an open-source, self-hosted software factory for developers. Agents take a change from idea
to shipped pull request on your own machines, and every PR carries one HTML artifact that lets you
trust it without reading the code.

**Status: in development. Nothing is usable yet.** There are no releases yet, and the parts below
describe the design being built, not features you can use today.

## Install

From its first release, Homebrew installs abc on macOS 12 or newer and on Linux with glibc 2.28 or
newer (RHEL 8, Debian 10, Ubuntu 20.04 and later), on arm64 or x86_64, and one command makes the
machine abc's server:

```sh
brew install getabc-dev/tap/abc
abc server setup
```

Homebrew installs the `abc` binary, Postgres 18, and abc's own private `tailscaled` and `tailscale`,
which stay in abc's directory and off your `PATH`, so your own `tailscale` command is never
shadowed. Setup runs a private Postgres and abc's own Tailscale node, asks for your password once
(Tailscale lets only root point Serve at abc), installs the services that keep them running, and
opens the claim page in your browser with a setup code filled in. It never touches the Tailscale
you already run. [Running abc's server](docs/server.md) covers what it sets up,
`abc server status`, upgrades, backups and uninstalling.

## How a change moves

```text
Idea → Plan → Tasks → Build → Review → Ship
         ▲                             ▲
     you approve                   you approve
     the plan                      the PR
```

Agents do the work at each station. You decide at two gates: you approve the plan before any code
is written, and you approve the PR before it merges.

## How trust works

- **Gates.** Every change passes the same checks before merging: types, lint, tests and security
  scans. Tests are never weakened to get a change through.
- **The PR artifact.** Each PR carries one self-contained HTML file on its branch. It shows what
  changed, why, and what each reviewer found. Every entry records who wrote it, and the agent that
  raised a finding never closes it.
- **Your machines, your login.** The server and workers run on your own machines over your own
  Tailscale network. Agents run your own `claude` on your own login; abc never reads or stores your
  Claude credentials. Production secrets never reach agents.

## Stack

One Rust binary (`abc server`, `abc worker`), a React and strict TypeScript web app, Postgres on
the server, and workers on your machines connected over Tailscale. Agents run as `claude -p`,
Codex, or any agent that speaks ACP. [AGENTS.md](AGENTS.md) has the full stack and the rules the
code follows.

## Links

- [Running abc's server](docs/server.md)
- [Contributing](CONTRIBUTING.md), including the AI policy
- [Security policy](SECURITY.md) and the [threat model](docs/threat-model.md)
- [Code of conduct](CODE_OF_CONDUCT.md)
- [Licence: MIT](LICENSE)
