# Threat model

First draft, November 2026. abc is in development, so most of this describes the design, not
running code. Each mitigation is marked **Built**, **Planned** or **Open** (no mitigation decided yet). Rule IDs such as I2 refer to
[AGENTS.md](../AGENTS.md).

Report vulnerabilities as [SECURITY.md](../SECURITY.md) explains.

## What exists today

- **Built:** the worker-server handshake in `crates/abc-protocol`: message types, JSON encoding and
  protocol version negotiation. There is no transport, no authentication and no server or worker
  binary yet.
- **Built:** the repository guardrails: `cargo xtask` with tools pinned by sha256, CI scans
  (cargo-deny, osv-scanner, gitleaks, zizmor, pinact), the agent git guard hook, and GitHub
  rulesets on `main`.
- **Planned:** everything else in this document.

## System

```text
Browser ──B1──▶ Tailscale sidecar ──▶ abc server ──▶ Postgres
                                       ▲      ▲
                                    B3 │      │ B2 (public webhook path)
                                       │      │
                                  abc worker  GitHub
                                       │ B4    ▲
                                       ▼       │
                                 agent sandbox ┘ B5
```

B1 and B3 run over the user's tailnet. B2 is the only public path. B5 goes from the agent to
GitHub over the internet.

## Assets

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| User code | Git repositories on GitHub; worktrees on workers | Theft, or a malicious change merged under the user's name. |
| Claude login | The user's machine, managed by `claude` itself | Full use of the user's subscription. abc never reads it (I1). |
| Production secrets | The vault on the server (planned) | Access to the user's production systems (I2). |
| Provider API keys | The vault on the server (planned) | Billable model access (I1). |
| GitHub App private key | The server (planned) | Mints installation tokens for every repository the App is installed on. |
| Installation tokens | Minted by the server, handed to agents | Write access to code and PRs for up to one hour. |
| Tailscale identity | Node keys of the sidecar and each worker; the user's tailnet login | Reaching the server, and being taken for the user or a worker. |
| Event log and PR artifact | Postgres; the PR branch | The record people trust instead of reading code (I4, I6). |
| The gates | CI, `cargo xtask`, rulesets | Weakened gates let any other attack merge. |

## Actors

- **The user:** owns the server, the workers and the tailnet. Trusted.
- **Other tailnet members and devices:** shared tailnets, tagged servers, a phone that was lost.
  Not trusted with abc unless the permission check allows them (I4).
- **Agents:** run the user's tools on the user's login, but act on untrusted input: repository
  content, issues, PR comments, web pages and dependencies. Treat every agent as possibly
  prompt-injected.
- **Contributors to the user's repositories:** can put text in front of agents.
- **The internet:** reaches only the public webhook path.
- **Upstream suppliers:** crates, pinned tools, GitHub Actions, Claude Code, Codex, Tailscale.

## Boundaries and threats

### B1 · Browser to server, through the Tailscale sidecar

The server sits behind the official Tailscale sidecar. `tailscale serve` adds identity headers
(`Tailscale-User-Login`, `Tailscale-User-Name`, `Tailscale-User-Profile-Pic`) to tailnet requests
and strips copies sent by the client. It adds none for tagged devices or for Funnel traffic.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | A request reaches the server without passing through the sidecar and carries forged identity headers. | The server listens only where the sidecar can reach it, and trusts identity headers only on that listener. The public webhook path never reads them. | I4 | Planned |
| Spoofing | A request from a tagged device arrives with no identity headers. | No identity means no access. There is no default user. | I4 | Planned |
| Spoofing | Cross-site request forgery: a web page the user visits makes the browser send requests to the server's tailnet name, and the sidecar adds the user's identity. | State-changing requests require same-origin Fetch Metadata and Origin checks; no permissive CORS; the server checks the Host header against DNS rebinding. | I4 | Planned |
| Tampering | Traffic altered on the way. | WireGuard between devices; HTTPS from the sidecar. | — | Planned |
| Repudiation | An action that cannot be traced to a person. | Every action records its actor in the event log. | I4 | Planned |
| Information disclosure | A production secret shown in the web app. | Secrets never leave the vault for display, logs or PR artifacts. | I2 | Planned |
| Denial of service | A tailnet device floods the server. | Request size and rate limits per identity. | P4 | Planned |
| Elevation of privilege | A tailnet member reaches another workspace's data. | One permission check on every request; everything belongs to a workspace. | I4 | Planned |

### B2 · GitHub to server, on the public webhook path

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | Forged webhook deliveries. | Verify the `X-Hub-Signature-256` HMAC in constant time before parsing the body. | P4 | Planned |
| Tampering | Replayed deliveries. | Record delivery IDs and process each once. | — | Planned |
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

### B5 · Agent to GitHub, through the getabc-dev-agent App

Agents act on GitHub as a GitHub App, using installation tokens that expire after one hour.
Today the getabc-dev-agent App is installed on this repository to build abc itself. It has read
access to actions, checks, metadata and statuses, and write access to contents, pull requests and
workflows.

| STRIDE | Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- | --- |
| Spoofing | An agent passes off its work as a person's approval. | Plan and PR approval come from the user's own account, never from the App. | A1 | Planned |
| Tampering | An agent force-pushes or deletes `main`. | Ruleset "Protect main" forbids force pushes and deletion, with no bypass actors. | P3 | Built |
| Tampering | An agent merges without passing the gates. | Ruleset "main: checks must pass" requires the three CI checks. | P3 | Built |
| Tampering | An agent pushes a commit to `main` that already passed CI on a branch, skipping PR approval. | Not covered: no ruleset requires a pull request or an approving review (see below). | P3 | Open |
| Tampering | An agent edits CI workflows, which the App's workflows permission allows, to weaken the gates. | Human review of every PR. Not enforced by a ruleset today (see below). | P3 | Open |
| Tampering | An agent forges who wrote an entry in the PR artifact, or closes its own finding. | The artifact is rendered from the event log, where actors are recorded by the server. | I6 | Planned |
| Information disclosure | An installation token leaks into logs or the PR artifact. | Tokens expire after one hour; abc redacts them from logs and artifacts; each token is narrowed to the change's repository and the permissions the step needs. | I2, I5 | Planned |
| Elevation of privilege | The App's private key leaks and mints tokens for every installation. | The key stays on the server, in the vault. Workers and agents only ever get short-lived tokens. | I2 | Planned |
| Elevation of privilege | Monitors use write tokens. | Monitors and integrations request read-only tokens. | I5 | Planned |

### Supply chain and the gates

| Threat | Mitigation | Rule | Status |
| --- | --- | --- | --- |
| A tampered tool download. | `cargo xtask setup` checks the sha256 of each archive and of the executable in it; `cargo xtask doctor` re-checks in CI; `cargo xtask tool` hashes each tool before running it. | P3 | Built |
| A compromised or retagged GitHub Action. | Actions are pinned to full commit SHAs, checked by pinact; zizmor audits the workflows. | P3 | Built |
| A vulnerable or unwanted dependency. | cargo-deny (advisories, licences, bans, sources) and osv-scanner in CI. | P3 | Built |
| A secret committed to git. | gitleaks scans the whole history in CI. | I2 | Built |
| An agent force-pushes by accident. | The git guard hook blocks force pushes and git or gh commands it cannot read. It is a guard against accidents; the rulesets are the enforcement. | — | Built |
| A tampered abc release. | One signed binary. | P4 | Planned |

## Open questions

1. **Branch protection gaps.** No ruleset requires a pull request or an approving review on `main`,
   so the App, which can write contents, could push a commit that already passed CI straight to
   `main`. Should `main` require a PR with an approving human review and code owner review of
   `.github/workflows`?
2. **The App's workflows permission.** Does the agent need to write workflows, or should workflow
   changes be human-only?
3. **Worker enrolment.** Is Tailscale node identity enough to admit a worker, or does the user also
   approve each worker with a one-time enrolment token? How is a worker revoked?
4. **One App per install.** Each self-hosted install needs its own GitHub App and private key. How
   does a user create it, and where exactly is the key stored before the vault exists?
5. **Agent egress.** Agents need their provider's API. Can the sandbox restrict egress to an
   allowlist, and how are other destinations (package registries, documentation) granted?
6. **Claude credentials inside the sandbox.** `claude` must read its own login, so a
   prompt-injected agent may be able to read it too. Can the sandbox give `claude` its login
   without exposing it to the tools the agent runs?
7. **Tokens to agents.** Are installation tokens passed through a git credential helper, an
   environment variable or a file, and how are they kept out of the agent's transcript?
8. **CSRF design.** Which exact checks (Fetch Metadata, Origin, a custom header) does the server
   require on state-changing requests?
9. **Shared tailnets.** v1 serves one person per server. What does the server do when another
   tailnet member reaches it: reject, or show a request-access screen?
10. **PR artifact integrity.** If the artifact is a file on the PR branch, how does a reader verify
    that its entries match the event log and were not edited on the branch?
