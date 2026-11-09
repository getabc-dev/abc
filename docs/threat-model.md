# Threat model

First draft, November 2026. abc is in development, so most of this describes the design, not
running code. Each mitigation is marked **Built**, **Planned** or **Open** (no mitigation decided
yet). Rule IDs such as I2 refer to [AGENTS.md](../AGENTS.md).

Report vulnerabilities as [SECURITY.md](../SECURITY.md) explains.

## What exists today

- **Built:** the worker-server handshake in `crates/abc-protocol`: message types, JSON encoding and
  protocol version negotiation. There is no transport, no authentication and no server or worker
  binary yet.
- **Built:** the repository's gates and guardrails: `cargo xtask` with tools pinned by sha256; CI
  that runs check, Integrity, Coverage, Mutants, Gemini review and the scans (cargo-deny,
  osv-scanner, gitleaks, zizmor, pinact); GitHub secret scanning with push protection; one ruleset
  on `main`; per-use App tokens for agents; the agent git guard hook and the watchdog.
- **Planned:** the rows marked Planned below describe the product's design.
- **Open:** the rows marked Open, and the open questions at the end, have no decided mitigation.

## System

```text
Browser ──B1──▶ abc's tailscaled ──▶ abc server ──▶ Postgres
                                       ▲  │  ▲
                                    B3 │  │  │ B2: webhooks, the only public path
                                       │  │  └──────────────── GitHub
                                       │  └──B6: mint tokens──▶ GitHub API
                                  abc worker
                                       │ B4
                                       ▼
                                 agent sandbox ──B5: git, gh──▶ GitHub
                                       │
                                       └──B7──▶ model providers
```

B1 and B3 run over the user's tailnet. B2 is the only path open to the internet. B5, B6 and B7
leave the user's machines for GitHub and the model providers.

## Assets

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| User code | Git repositories on GitHub; worktrees on workers | Theft, or a malicious change merged under the user's name. |
| Claude login | The user's machine, managed by `claude` itself | Full use of the user's subscription. abc never reads it (I1). |
| Production secrets | The vault on the server (planned) | Access to the user's production systems (I2). |
| Provider API keys | The vault on the server (planned) | Billable model access (I1). |
| GitHub App private key, product | The server (planned) | Mints installation tokens for every repository the App is installed on. |
| GitHub App private key, abc's own development | `~/.config/abc/agent-app/` on the maintainer's laptop | Mints tokens for this repository; see [Today: abc's own development](#today-abcs-own-development). |
| Installation tokens | Minted per use, handed to agents | Write access to code and PRs until revoked or for up to one hour. |
| Webhook secret | The server (planned) | Lets anyone forge GitHub events to the public path. |
| `GEMINI_API_KEY` | A GitHub Actions secret, given only to the Gemini review job | Billable model access, and the review gate itself. |
| Tailscale identity | Node keys of abc's own node (`tailscale/` in the server's home) and each worker; the user's tailnet login | Reaching the server, and being taken for the user or a worker. |
| Event log and PR artifact | Postgres; the PR branch | The record people trust instead of reading code (I4, I6). |
| The gates | CI, `cargo xtask`, the `main` ruleset | Weakened gates let any other attack merge. |

## Actors

- **The user:** owns the server, the workers and the tailnet. Trusted.
- **Other tailnet members and devices:** shared tailnets, people on other tailnets who accepted a
  share of the server's node, tagged servers, a phone that was lost. Not trusted with abc unless the
  permission check allows them (I4).
- **Builder agents:** run the user's tools on the user's login, but act on untrusted input:
  repository content, issues, PR comments, web pages and dependencies. Treat every agent as possibly
  prompt-injected.
- **Reviewer agents:** read the change, its title and its description, all of which the change's
  author controls.
- **Outsiders:** anyone who can open issues, comment or send PRs on a user's public repository, and
  so put text in front of agents.
- **The internet:** reaches only the public webhook path.
- **Upstream suppliers:** crates, pinned tools, GitHub Actions, Claude Code plugins and skill packs,
  Renovate, Claude Code, Codex, Tailscale, the model providers.

## Today: abc's own development

Status: **Mitigated in part**. Until abc can build itself, the maintainer builds it with Claude
Code sessions on a macOS laptop, each in a microVM of its own (D37). This section covers that
setup, not the product.

- **The VM boundary.** `cargo xtask vm` runs each session in a microsandbox VM (libkrun on
  Hypervisor.framework, KVM on Linux), pinned by sha256 in `xtask/tools.toml`. The VM mounts the
  worktree, read-write at its own path, and the session's own git repository at `/abc/git`;
  nothing else of the host's files. The launcher refuses mount paths in the guest's system
  directories or with the separators of microsandbox's mount syntax, so a mount cannot replace
  the image's own programs. The maintainer's home, keys, keychains, gh login, the App's key,
  other worktrees and the main clone, with its config and hooks, are not in the VM. Inside, the
  agent runs as uid 1000 with no capabilities and no_new_privs, and the image has no setuid
  programs.
- **What the VM writes is hostile to the host.** The worktree's files, its `.git` file and the
  session repository are all writable in the VM. The host finds the worktree through the main
  clone (`git worktree list` and the main clone's own administrative directory), never through
  its `.git` file, and runs git on it with an explicit `GIT_DIR` and `GIT_WORK_TREE`. It runs git
  on the session repository with no system or global config and every setting that runs a
  program pinned off (hooks, fsmonitor, ssh, credential helpers, pagers, gc and maintenance, ext
  protocols), and before each start, with the VM stopped, it rewrites that repository's config,
  empties its hooks, drops its alternates and per-worktree config, and replaces its worktree
  files without following symlinks. While a VM runs, the host does not write its repository.
- **Egress.** microsandbox enforces a deny-by-default policy on the host: TCP 443 to the hosts in
  `xtask/vm/egress-allow.txt`, matched by the TLS server name, and DNS answers for those hosts
  only. The host machine, its loopback and LAN services (the colima Docker API among them), other
  VMs and cloud metadata are unreachable. The list limits what a session can download and what
  can steer it; it does not stop leaks. GitHub, crates.io and the Anthropic API all accept what a
  session sends them, an attacker's own credentials included, and matching by server name does
  not stop domain fronting.
- **GitHub as the App.** The getabc-dev-agent App's key lives in `~/.config/abc/agent-app/` on the
  laptop and only `cargo xtask broker` reads it. `cargo xtask vm start` starts one broker per
  session from a build of `main` installed outside any checkout; the broker's refusal to run from
  a checkout, like `cargo xtask vm`'s refusal to run from a linked worktree, prevents mistakes and
  is not a control. Each broker listens on the session's own sockets in
  `~/.config/abc/agent-broker/` (owner only): one forwarded into that VM over vsock, so the VM has
  no network path to the host, and a control socket the VM cannot reach. It logs each request
  with the session's name and never a token, mints at most 120 tokens an hour, and when
  `cargo xtask vm stop` stops it, revokes every token it minted. Each client has five seconds in
  all to send its request, and a stop hangs up on the clients in flight, so a VM that trickles a
  request cannot hold the broker. A broker that does not answer `stop` gets SIGTERM through its
  pid file, once `ps` shows that pid is still the session's broker; SIGTERM stops it directly,
  without its sockets, revoking its tokens the same way. One still the broker a minute later gets
  SIGKILL, which leaves its tokens to expire within the hour. Its files are removed only once it
  is gone; when it may still run, `vm stop` fails naming its pid. git gets contents write alone; a
  change to a workflow is the maintainer's to push. `cargo xtask gh` gets contents and pull
  requests write and read access to CI results. The `branches` ruleset lets the App create,
  update and delete only `agent/*` branches (every other branch but `main`, which its own
  ruleset guards, is for the maintainers and Renovate), and the `tags` ruleset leaves tags to the
  maintainers. Within those bounds a session can push its `agent/*` branches and open or change
  pull requests.
- **Postgres** runs inside the VM, on 127.0.0.1, with its data in memory. The agent connects as
  `abc_test`, which may create databases but is no superuser, so it cannot run programs as the
  server (`COPY ... TO PROGRAM`); the superuser is reachable only by the postgres OS user.
- **Claude Code on the subscription only.** The maintainer's `claude setup-token` token, valid for
  about a year, stays in their keychain; `cargo xtask vm start` pipes it into the VM's memory
  (`/run/abc`, a tmpfs) in a file only root can read, so it never appears in an argument, the
  VM's configuration or a log. Only a `claude` process started through `agent-exec` gets it; shells and other commands entered
  that way do not. The commands Claude Code runs inherit it (see Open). The launcher passes no API key or other route, and the VM's own xtask refuses
  API keys, auth tokens, key helpers, custom endpoints, cloud providers and `CLAUDE_CONFIG_DIR`,
  in the environment and in every settings file Claude Code reads (managed settings and their
  drop-ins included), before the agent user starts and before each session; `start` fails unless
  `claude auth status` reports the subscription token. Rotate the token by replacing the keychain
  item and restarting sessions, and revoke old ones in the claude.ai settings.
- **Hooks** run inside the VM with the session, so code an agent wrote never runs on the host
  through them.
- **The Gemini review** holds `GEMINI_API_KEY`. On pull requests it runs on
  `pull_request_target`, so its workflow and its base-xtask action come from the base branch, and
  it reads the change only as a diff. In the merge queue it runs the queued change's workflow,
  which a code owner approved after its last push; that approval, and the code ownership of
  `.github/`, are what keep the key from a workflow change.

Open:

- microsandbox writes the output of everything run in a VM, the interactive Claude Code session
  included, to `exec.log` on the host while the VM exists. `stop` deletes the VM and its logs;
  until then, a secret a session prints sits in that file, readable by the maintainer's user.
- microsandbox is beta, and its releases break compatibility. The pin, a state directory per
  version and sessions that hold only caches bound the damage of an upgrade; a flaw in libkrun or
  in microsandbox's network stack would cross the boundary.
- Code from an agent's worktree, run from the maintainer's own terminal, runs with the
  maintainer's full access. Review it first, or run it in a session.
- The commands Claude Code runs (builds, tests, the Bash tool) inherit the subscription token.
  Claude Code strips it from them only under its bubblewrap isolation
  (`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`), which on Linux leaves empty placeholder files such as
  `package.json`, `.npmrc` and `.env` in the worktree, where agents would commit them; it is off
  until that is solved.
- The check before each tool call runs from the worktree's hook script, which the agent can edit,
  so it is a tripwire; so is `claude --settings <file>`, which names a settings file no check can
  see in advance. The checks before the VM and each session start run from the image.

## Boundaries and threats

### B1 · Browser to server, through abc's own tailscaled

The server sits behind abc's own tailscaled, a node of its own named `abc`, which `abc server
setup` runs unprivileged in userspace ([The server's host](#the-servers-host-one-root-call)).
`tailscale serve` adds identity headers
(`Tailscale-User-Login`, `Tailscale-User-Name`, `Tailscale-User-Profile-Pic`) to tailnet requests,
and `Tailscale-App-Capabilities` when it is set to forward app capabilities. It strips copies of
these headers sent by the client. It adds no user identity for tagged devices or for Funnel
traffic, and it does add identity headers for people on other tailnets who accepted a share of the
node.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | A request reaches the server without passing through the sidecar and carries forged identity headers. | The server listens only where the sidecar can reach it, and trusts identity headers only on that listener. The public webhook path never reads them. | I4 | Planned |
| Spoofing | A request from a tagged device arrives with no user identity. | No identity means no access. There is no default user. | I4 | Planned |
| Elevation of privilege | Someone on another tailnet, who accepted a share of the node, arrives with valid identity headers. | A valid identity header is not access: the permission check admits only identities the user has added. | I4 | Planned |
| Spoofing | Cross-site request forgery: a web page the user visits makes the browser send requests to the server's tailnet name, and the sidecar adds the user's identity. | State-changing requests require same-origin Fetch Metadata and Origin checks; no permissive CORS; the server checks the host Tailscale Serve forwards (`X-Forwarded-Host`, which Serve overwrites, behind `Host: localhost`) against DNS rebinding. | I4 | Planned |
| Tampering | Traffic altered on the way. | WireGuard between devices; HTTPS from the sidecar. | — | Planned |
| Repudiation | An action that cannot be traced to a person. | Every action records its actor in the event log. | I4 | Planned |
| Information disclosure | A production secret shown in the web app. | Secrets never leave the vault for display, logs or PR artifacts. | I2 | Planned |
| Denial of service | A tailnet device floods the server. | Request size and rate limits per identity. | P4 | Planned |
| Elevation of privilege | A tailnet member reaches another workspace's data. | One permission check on every request; everything belongs to a workspace. | I4 | Planned |

### The server's host: one root call

`abc server setup` runs the stack as the user who runs it: a private Postgres 18, abc's own
tailscaled and abc server, as launchd agents or systemd user units, all under the server's home
(0700). Root is used once, because Tailscale lets only root point Serve at a Unix socket:
`sudo <abc's tailscale> --socket=<abc's> serve --bg unix:<home>/run/abc.sock`, with
`loginctl enable-linger <uid>` in the same `sudo` on Linux. Setup prints that command and why
before `sudo` asks, and runs it only when Serve or lingering is missing. The Serve setting lives
in abc's node's state, so no later run needs root.

abc trusts the identity headers Serve sets because only abc's own node can reach the socket, and
only this user controls that node: its state and its socket sit in the private home.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Elevation of privilege | Setup's root step does more than it says. | One `sudo` of one printed command, built from fixed words with every path an argument (never spliced into a shell script); nothing else runs as root, and a second run asks nothing. Tests check the exact command line. | I3 | Built |
| Spoofing | Another user of the machine connects to `abc.sock` and sends forged identity headers. | The socket's directory is the home's `run/` (0700); abc server refuses a directory with any permission for its group or others, and makes the socket 0600. | I4 | Built |
| Spoofing | Another user of the machine connects to Postgres, which trusts its socket. | Postgres listens on no TCP address (`listen_addresses=''`, host connections rejected), and its socket is in the same 0700 directory with `unix_socket_permissions=0700`. | I4 | Built |
| Tampering | Another user of the machine changes Serve or logs abc's node out through its tailscaled. | tailscaled's socket and state are in the private home; changing Serve to a socket needs root besides. | I4 | Built |
| Tampering | abc changes the user's own Tailscale. | Every Tailscale call names abc's socket; abc runs the tailscale and tailscaled its install or `config.toml` names, never one on the `PATH`. Tests run a `PATH` tailscale that fails if called. | I3 | Built |
| Information disclosure | The setup code leaks from the claim link. | The code rides in the URL's fragment, which the browser never sends, so no proxy or server log holds it; the app reads it once and takes it out of the address bar and history. The link is printed to the terminal and passed to `open` or `xdg-open`, where another user of the machine could see it in the process list for a moment; using it also needs a tailnet identity, and it expires in an hour. | I2 | Built |
| Information disclosure | A backup is read by someone else. | `abc server backup` writes a new 0600 file and never overwrites one. | I2 | Built |

### B2 · GitHub to server, on the public webhook path

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | Forged webhook deliveries. | Verify the `X-Hub-Signature-256` HMAC with the webhook secret, in constant time, before parsing the body. | P4 | Planned |
| Tampering | Replayed deliveries. | Record delivery IDs and process each once. | — | Planned |
| Elevation of privilege | An outsider's issue or comment on a user's public repository starts agent work. | Open: which events, from whom, may start work without the user's approval (see below). | I3 | Open |
| Elevation of privilege | The public path reaches other routes. | The public listener serves the webhook route alone. | P4 | Planned |
| Denial of service | Oversized or repeated payloads. | Body size limit; work is queued, not done inline. | P4 | Planned |

### B3 · Server to worker, over a WebSocket on the tailnet

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | Any device on the tailnet connects as a worker. | Workers must be enrolled by the user and identified by their Tailscale node identity. How enrolment works is open (see below). | I4 | Planned |
| Spoofing | A worker connects to a fake server. | The worker checks the server's Tailscale identity. | — | Planned |
| Tampering | Messages altered in transit. | WireGuard encrypts and authenticates the tunnel. | — | Planned |
| Tampering | A malicious peer sends malformed messages. | Decoders reject unknown message types and reject codes; an inverted version range fails to decode. Size limits on frames are still needed. | P4 | Built (decoding) |
| Tampering | A peer forces an old protocol version with a known flaw. | `negotiate` picks the highest shared version; the server drops old versions from its supported range when a fix needs a new one. | P4 | Built (negotiation) |
| Repudiation | A worker's events cannot be tied to it. | Every event records its actor; the SQLite outbox replays events with stable IDs. | I4 | Planned |
| Information disclosure | A worker receives data for changes it does not run. | The server sends a worker only what its assigned changes need. | I3 | Planned |
| Elevation of privilege | A compromised worker sends events for other workspaces or changes. | The server checks every message against the worker's assignments. | I4 | Planned |

### B4 · Worker to agent sandbox

Agents run the user's own unmodified `claude` (or Codex, or an ACP agent) inside Seatbelt on macOS
or bubblewrap with seccomp on Linux. The agent needs network access to its model provider.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Elevation of privilege | A prompt-injected agent reads or writes outside its change: other repositories, `~/.ssh`, other worktrees. | The sandbox exposes only the change's worktree. Anything else needs a folder grant: scoped, time-limited or task-limited, revocable and logged. | I3 | Built (abc's development, microVMs); Planned (product) |
| Elevation of privilege | Code the agent wrote runs outside the sandbox, for example when the worker runs builds, tests or repository hooks on it. | Every command on agent-written code runs inside the sandbox. | I3 | Planned |
| Elevation of privilege | The agent reaches local sockets: the ssh agent, Docker, the `tailscaled` local API. | The sandbox denies them unless a grant allows one. | I3 | Planned |
| Information disclosure | Production secrets reach the agent through environment variables or files. | The worker starts agents with a clean environment; production secrets never enter it. | I2 | Planned |
| Information disclosure | abc handles the user's Claude credentials. | abc never reads, stores, proxies or logs them; `claude` manages its own login. | I1 | Planned |
| Information disclosure | The agent sends code or data to an attacker over the network. | Open: the agent needs its provider's API, so egress cannot simply be closed (see below). | I3 | Open |
| Tampering | The agent edits the worker, its config or its outbox. | None of them are inside the sandbox. | I3 | Planned |
| Repudiation | An agent's actions cannot be told apart from the user's. | Every action records the agent as its actor. | I4 | Planned |
| Denial of service | A runaway agent fills the disk, burns CPU or spends the user's quota. | Per-run limits on time, disk and processes. | P4 | Planned |

### B5 · Agent to GitHub, through the App

Agents act on GitHub as a GitHub App. For abc's own development that is the getabc-dev-agent App.
It has read access to actions, checks, metadata and statuses, and write access to contents, pull
requests and workflows, though no token asks for workflows any more. Each token is minted for one
use, for this repository alone, with only the permissions that use needs: git gets contents
write; `cargo xtask gh` gets contents and pull requests write and read access to CI results, and
revokes its token when gh exits. The `branches` ruleset confines the App's pushes to `agent/*`.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | An agent passes off its work as a person's approval. | Plan and PR approval come from the user's own account, never from the App. In this repository only a code owner's approval counts. | A1 | Built (abc's repo); Planned (product) |
| Tampering | An agent force-pushes or deletes `main`. | The `main` ruleset forbids force pushes and deletion. | P3 | Built |
| Tampering | An agent lands a change on `main` without the gates or a person's approval. | The one `main` ruleset requires a pull request, approval by a code owner after the last push, squash merges through the merge queue, and every CI check. Its only bypass is a repository admin, and only through a pull request. | P3, A1 | Built |
| Tampering | An agent edits CI workflows or other gate files to weaken the gates. | Gate files are code-owned and flagged by the Integrity check, which needs an approver's `integrity-approved` label. Integrity, Coverage, Mutants and Gemini review run the base commit's xtask, so a PR cannot change the gates that judge it. | P3 | Built |
| Tampering | An agent forges who wrote an entry in the PR artifact, or closes its own finding. | The artifact is rendered from the event log, where actors are recorded by the server. | I6 | Planned |
| Information disclosure | An installation token leaks into logs or the PR artifact. | Tokens are per use and short-lived; xtask never prints them. The product also redacts tokens from logs and artifacts. | I2, I5 | Built (abc's tooling); Planned (product) |
| Elevation of privilege | Monitors use write tokens. | Monitors and integrations request read-only tokens. | I5 | Planned |

### B6 · Server to the GitHub API, minting tokens

The server signs requests with the App's private key to mint installation tokens. For abc's own
development, `cargo xtask` does this on the maintainer's laptop instead (see
[Today](#today-abcs-own-development)).

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Elevation of privilege | The private key leaks and mints tokens for every installation. | The key stays on the server, in the vault. Workers and agents only ever get short-lived tokens. | I2 | Planned |
| Elevation of privilege | Tokens carry more access than the step needs. | Each token is narrowed to one repository and the permissions of its use. | I5 | Built (abc's tooling); Planned (server) |
| Spoofing | A fake GitHub API endpoint receives the signed requests. | HTTPS to GitHub's API with certificate verification. | — | Planned |

### B7 · Agents and reviewers to model providers

Agents and reviewers send code, diffs and prompts to Anthropic (through the user's `claude`),
OpenAI (Codex), Google (Gemini) and other model APIs. abc's own Gemini review runs in CI.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Information disclosure | A user's private code is used to train models or read by the provider's staff. Google's terms for unpaid Gemini API use say content is used to improve its products and human reviewers may read it. | Unpaid tiers are acceptable for abc's own public repository only. For private repositories the product requires a paid tier or the user's own paid key. | I2 | Planned |
| Information disclosure | A production secret reaches a provider in a prompt. | Production secrets never enter an agent's context. | I2 | Planned |
| Information disclosure | A provider API key leaks. | Keys live only in the vault (product). `GEMINI_API_KEY` is given to the review job alone, which runs nothing from the PR's tree. | I1 | Built (CI); Planned (product) |
| Denial of service | A provider's quota or outage stops a gate. | The Gemini review fails closed on quota, size and missing-key errors; the break-glass bypass covers long outages. | P3 | Built |

### Prompt injection

Prompt injection crosses every boundary above, so it has its own rows.

| Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- |
| A prompt-injected builder lands a backdoor that passes the gates and reads fine in the PR artifact. | A person approves the plan and the PR; artifact entries record who wrote them and the agent that raised a finding never closes it; a checker with fresh context reviews each change. | A1, I6 | Planned |
| An injected PR title, description or diff steers a reviewer agent. abc's Gemini review reads all three, and any PR author writes them. | The Gemini review is one gate among several, not the approval. A code owner's review is the backstop. | A1 | Built (abc's repo) |
| An outsider's issue or comment on a user's public repository starts agent work through a webhook. | Open (see B2 and below). | I3 | Open |

### Supply chain and the gates

| Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- |
| A tampered tool download. | `cargo xtask setup` checks the sha256 of each archive or crate and of the executable; `cargo xtask doctor` re-checks in CI; xtask hashes each tool again before running it. | P3 | Built |
| A compromised or retagged GitHub Action. | Actions are pinned to full commit SHAs, checked by pinact; zizmor audits the workflows. | P3 | Built |
| A vulnerable or unwanted dependency. | cargo-deny (advisories, licences, bans, sources) and osv-scanner in CI. | P3 | Built |
| A secret committed to git. | GitHub secret scanning with push protection blocks known secret types at push; gitleaks scans the whole history in CI. | I2 | Built |
| A Claude Code plugin or skill pack runs code as the user. `.claude/settings.json` enables `pstack@cursor-plugins` and `egm@egm` from marketplaces that are not pinned to a commit, and plugins can ship hooks and MCP servers. | Review each pack before adopting it, and record the adoption in the product brief. | B1 | Open |
| A compromised upstream release arrives in a Renovate update. | Renovate PRs pass the same gates and code-owner review as any other; tool updates are re-pinned against published checksums or attestations. | P3 | Open |
| An agent force-pushes or makes gh changes as the user by accident. | The git guard hook blocks force pushes, git or gh commands it cannot read, and gh changes made as the user. It guards against accidents; the ruleset is the enforcement. | — | Built |
| A tampered abc release. | One signed binary. | P4 | Planned |

## Open questions

1. **The App's private key on the maintainer's laptop.** Which mitigation lands first: Claude Code's
   Bash sandbox with read denies, or moving the key off the laptop? See
   [Today](#today-abcs-own-development).
2. **The App's workflows permission.** Decided for abc's own development: git tokens no longer
   carry it, so workflow changes are the maintainer's to push. The App's installation still has
   the permission until the maintainer removes it from the App's settings.
3. **Plugins and skill packs.** How are adopted packs pinned and reviewed, given that their
   marketplaces are not pinned to a commit today?
4. **Worker enrolment.** Is Tailscale node identity enough to admit a worker, or does the user also
   approve each worker with a one-time enrolment token? How is a worker revoked?
5. **One App per install.** Each self-hosted install needs its own GitHub App and private key. How
   does a user create it, and where exactly is the key stored before the vault exists?
6. **Agent egress.** Agents need their provider's API. Can the sandbox restrict egress to an
   allowlist, and how are other destinations (package registries, documentation) granted?
7. **Claude credentials inside the sandbox.** `claude` must read its own login, so a
   prompt-injected agent may be able to read it too. Can the sandbox give `claude` its login
   without exposing it to the tools the agent runs?
8. **Tokens to agents in the product.** Are installation tokens passed through a git credential
   helper, as abc's own tooling does, an environment variable or a file, and how are they kept out
   of the agent's transcript?
9. **Webhook-triggered work.** Which events, from which people, may start agent work on a public
   repository without the user's approval?
10. **CSRF design.** Which exact checks (Fetch Metadata, Origin, a custom header) does the server
    require on state-changing requests?
11. **Shared tailnets.** v1 serves one person per server. What does the server do when another
    tailnet member, or someone the node is shared with, reaches it: reject, or show a
    request-access screen?
12. **PR artifact integrity.** If the artifact is a file on the PR branch, how does a reader verify
    that its entries match the event log and were not edited on the branch?
