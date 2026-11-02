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
Browser ──B1──▶ Tailscale sidecar ──▶ abc server ──▶ Postgres
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
| Tailscale identity | Node keys of the sidecar and each worker; the user's tailnet login | Reaching the server, and being taken for the user or a worker. |
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

Status: **Open**. Until abc can build itself, the maintainer builds it with Claude Code sessions on
a laptop. This section covers that setup, not the product.

- The getabc-dev-agent App's private key lives in `~/.config/abc/agent-app/` on the laptop.
- Agent sessions mint App tokens through `cargo xtask git-credential` and `cargo xtask gh`. They run
  without a sandbox, as the maintainer's user, and the maintainer's own gh login is on the same
  machine.
- `.claude/settings.json` denies the Read tool on `~/.config/abc/agent-app/` and `~/.config/gh/`,
  and denies `gh auth token` and `security find-*-password`. Bash can still read the key, because
  the git guard catches accidents only. A prompt-injected session can therefore mint tokens with
  all of the App's permissions: contents, pull requests and workflows write.
- The `main` ruleset bounds what those tokens can do: nothing merges without a code owner's
  approval after the last push, and the admin bypass works only through a pull request. Within
  that bound, a session can still push any other branch and open or change pull requests. It can
  also add or edit workflows on a branch, and workflows that run from this repository's branches
  get its secrets, such as `GEMINI_API_KEY`.
- The maintainer's gh login is a repository admin. A session that reads it can act as the
  maintainer, and the ruleset does not bound that.

Mitigation candidates: run sessions in Claude Code's Bash sandbox with reads of
`~/.config/abc/agent-app/` and `~/.config/gh/` denied; move the App key off the laptop so sessions
only ever receive tokens; keep an admin login out of agent sessions.

## Boundaries and threats

### B1 · Browser to server, through the Tailscale sidecar

The server sits behind the official Tailscale sidecar. `tailscale serve` adds identity headers
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
| Elevation of privilege | A prompt-injected agent reads or writes outside its change: other repositories, `~/.ssh`, other worktrees. | The sandbox exposes only the change's worktree. Anything else needs a folder grant: scoped, time-limited or task-limited, revocable and logged. | I3 | Planned |
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
requests and workflows. Each token is minted for one use, for this repository alone, with only the
permissions that use needs: git gets contents and workflows write; `cargo xtask gh` gets contents
and pull requests write and read access to CI results, and revokes its token when gh exits.

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
2. **The App's workflows permission.** Does the agent need to write workflows, or should workflow
   changes be human-only, given that branch workflows run with repository secrets?
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
