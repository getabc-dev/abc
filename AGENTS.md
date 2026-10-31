# abc

Rule IDs are stable: a removed rule leaves its number unused, and reviewers cite rules by ID.

abc is an open-source, self-hosted software factory for developers. Agents take a change from idea
to plan, tasks, build, review and ship, and every PR carries one HTML artifact that lets a person
trust it without reading the code. Other people will run abc and fund it through donations, so it
is built to production grade from the first commit.

Product decisions and research live in the abc product brief (Claude Doc):
https://claude.ai/artifact/1yeSnW2FFpd99v8ftvMisg

## STRICT: this project is in development

abc has no users, no releases and no data to preserve. Build it fundamentally correct. These rules
override any default habit of keeping changes small or safe.

- **S1 · Correctness over everything.** Never shrink, defer or avoid the right change because of
  its blast radius, effort or time. Do the change the design needs, however large.
- **S2 · No backward compatibility.** Break things freely: rename, restructure, delete. No shims,
  adapters, deprecation paths, compatibility flags or "v2" copies beside the old one.
- **S3 · No backfills or migrations for old states.** Nothing needs preserving. Reset instead.
- **S4 · No legacy and no tech debt, ever.** No placeholder TODOs, stubs, workarounds, duplicated
  concepts or dead code. After every PR the whole codebase is in its correct state, not only the
  part the PR touched.
- **S5 · Fix what you find.** A defect or wrong design you notice outside your task's scope gets
  fixed properly, in the same PR or a dedicated one, before you move on. Never step around it.
- **S6 · First principles.** Design from what the product needs, not from what already exists. If
  existing code is wrong, replace it.
- **S7 · Scope of these rules.** They are about code and design in this repo. They never permit
  destructive actions on anyone's machine, data, accounts or secrets.

## AI writes all the code

- **A1 ·** Agents write 100% of the code and tests; people decide and review. Choose languages,
  libraries and patterns for what makes agent-written code correct and verifiable: strict static
  types, exhaustive checks, fast deterministic tests, clear machine-readable errors. Human
  familiarity, typing effort and verbosity are not costs.

## Production grade

- **P1 · Research before deciding.** For any significant choice, check current best practice in
  primary docs and on the internet, then record the decision and its reasons in the product brief.
- **P2 · Tests with every change:** unit tests for logic, integration tests against real
  dependencies (Postgres, git, containers) rather than mocks, end-to-end tests for user flows.
  Tests are deterministic.
- **P3 · CI is the gate.** Types, lint, tests and security scans must pass. Never skip, weaken or
  delete a test to get green; fix the cause.
- **P4 · Others run this.** Installation, upgrades, configuration errors and security must work for
  a stranger on their own machines, not only for us.

## abc builds abc

- **B1 ·** We follow the practices abc enforces. Every PR goes through abc's review pipeline once
  it exists; until then, run `/review`, `/code-review` and `/security-review` (or `/cso`) before
  merging. Skills from gstack, pstack (poteto's pack) and other packs join our pipeline as we
  evaluate them; record each adoption in the product brief.

## Product invariants

Code must uphold these. Changing one needs an explicit decision in the product brief.

- **I1 ·** Never read, store, proxy or log a user's Claude subscription credentials. Agents run the
  user's own unmodified `claude` on the user's machine. Provider API keys live only in the vault.
- **I2 ·** Production secrets never reach dev environments, agents, logs or PR artifacts.
- **I3 ·** An agent can touch only its change's code and environment. Anything else needs a folder
  grant from the user: scoped, time-limited or task-limited, revocable and logged.
- **I4 ·** Every action records its actor, a person or an agent. Everything belongs to a workspace.
  Every request passes through one permission check. v1 serves one person per server, but no code
  assumes a single user.
- **I5 ·** Monitors and integrations use read-only scopes unless writing is their purpose.
- **I6 ·** The PR artifact is one self-contained HTML file on the PR branch. Every entry records who
  wrote it. The agent that raised a finding never closes it.

## Stack

Chosen on 31 Oct 2026 through research against the rules above; the reasons are in the product
brief.

- **Server and worker:** Rust in one Cargo workspace, shipped as one signed binary
  (`abc server`, `abc worker`).
- **Web app and phone:** React, strict TypeScript, Vite, TanStack Router and Query; an installable
  PWA.
- **Types:** Rust is the source of truth. TypeScript types and JSON Schema are generated from it,
  never written by hand.
- **Data:** Postgres on the server and a SQLite outbox on each worker, both through sqlx with
  queries checked at compile time.
- **Events:** an append-only event log plus views, written in the same transaction. Every write
  goes through the event path.
- **Agents:** `claude -p` with stream-json on the user's own login, Codex through its app-server,
  other agents over ACP, model APIs over HTTP.
- **Network:** the user's `tailscaled` on workers and the official Tailscale sidecar on the server.
  Only the webhook path is public.
- **Sandbox:** Seatbelt on macOS; bubblewrap and seccomp on Linux.
- **Licence:** MIT.

## Commands

Everything runs through `cargo xtask`, and agent hooks and CI call the same commands. The Rust
toolchain is pinned in `rust-toolchain.toml`; rustup installs it on first use.

| Command | What it does |
| --- | --- |
| `cargo xtask setup` | Installs the tools pinned in `xtask/tools.toml` into `.tools/`, checking the sha256 of each archive and of the executable in it. Run it in every new clone or worktree, and after the list changes. |
| `cargo xtask doctor` | Fails unless every pinned tool is installed, matches its pinned sha256 and reports its pinned version. |
| `cargo xtask check` | Format check, clippy (`--all-targets --locked -D warnings`), nextest, doctests and rustdoc with warnings denied, on the whole workspace. Property tests run with a fixed seed. |
| `cargo xtask check --changed` | The same, limited to the crates affected by changes since the merge base with the default branch (`upstream/HEAD`, else `origin/HEAD`). |
| `cargo xtask tool <name> [args]` | Runs a pinned tool from `.tools/`, such as `cargo xtask tool cargo-deny check`. Never run these tools from `PATH`. |
| `cargo xtask pin` | After a version changes in `xtask/tools.toml`, downloads each target's archive and records its sha256 and its executable's. To add a tool, write zeros for both and run it. |
| `cargo fmt --all` | Fixes formatting. |

CI also runs `cargo xtask doctor`, `cargo xtask tool cargo-deny check`, `cargo xtask tool
cargo-shear --deny-warnings --locked`, `cargo xtask tool osv-scanner scan source --lockfile
Cargo.lock`, `cargo xtask tool zizmor --pedantic .github`, `cargo xtask tool pinact run --check
--verify-comment` and `cargo xtask tool gitleaks git --redact --no-banner .`.

The git guard in the agent hooks blocks force pushes and git or gh commands it cannot read, but
it is best effort: branch protection on GitHub is the real gate.

Commit the `proptest-regressions/` files proptest writes when a property test fails: they replay
the failing cases first on every run.

Lints are set for every crate in the root `Cargo.toml`. `#[allow]` is denied: silence a lint only
with `#[expect(lint, reason = "...")]`, and only when the code is right.
