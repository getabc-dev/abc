# abc

Rule IDs are stable: a removed rule leaves its number unused, and reviewers cite rules by ID.

abc is an open-source, self-hosted software factory for developers. Agents take a change from idea
to plan, tasks, build, review and ship, and every PR carries one HTML artifact that lets a person
trust it without reading the code. Other people will run abc and fund it through donations, so it
is built to production grade from the first commit.

Product decisions and research live in the abc product brief (Claude Doc):
https://claude.ai/artifact/JUbZzoDFMpqZnCSrn4i1Jh

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

- **A1 ·** The maintainers build abc with agents writing 100% of the code and tests; people decide
  and review. Contributions from any author are welcome and pass the same gates and review. Choose
  languages, libraries and patterns for what makes agent-written code correct and verifiable: strict
  static types, exhaustive checks, fast deterministic tests, clear machine-readable errors. Human
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
- **I4 ·** Every action records its actor: a person, an agent, an enrolled worker or the server
  itself. Everything belongs to a workspace.
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
- **Web app and phone:** React with strict TypeScript 7 (the native compiler), Vite, TanStack
  Router and Query, Tailwind CSS v4 with its tokens as CSS variables in `@theme`, and React Aria
  Components; pnpm, and an installable PWA. It lives in `web/`; each screen follows the Elm
  architecture: a discriminated-union message type, a pure `update(model, message)` and effects at
  the edge. The `abc` binary embeds the built app (`web/dist`) and serves it beside the API.
  Biome, Oxlint with type-aware rules, `tsc`, knip and Vitest gate it, and Playwright runs it in
  Chromium end to end.
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

Everything runs through `cargo xtask`, from the repository root, and agent hooks and CI call the
same commands. xtask is a Cargo workspace of its own (`xtask/Cargo.toml`, locked in
`xtask/Cargo.lock`, its lints a copy of the root's that an xtask test keeps equal), so no crate of
the main workspace can keep it from building. The Rust toolchain is pinned in
`rust-toolchain.toml`; rustup installs it on first use.

Agents start every task with `cargo xtask task set "<the task>"`. When the agent stops, the
watchdog compares the diff with that task and the rules above, and keeps it working if it drifted.

| Command | What it does |
| --- | --- |
| `cargo xtask setup [--for-tests] [TOOLS]` | Installs the tools pinned in `xtask/tools.toml` into `.tools/`, checking the sha256 of each archive or crate and of the executable. Run it in every new clone or worktree, and after the list changes. It never touches git config. `--for-tests` installs the tools marked `for-tests`, those the tests run, with any named; every CI job that runs tests uses it, and `rulesets check` fails one that does not. |
| `cargo xtask doctor [--for-tests] [TOOLS]` | Fails unless every pinned tool, or each one named or marked `for-tests`, is installed, matches its pinned sha256 and reports its pinned version. |
| `cargo xtask check` | Format check, clippy (`--all-targets --locked -D warnings`), nextest, sqlx's query cache check, doctests and rustdoc with warnings denied, on both workspaces, the main one and xtask's, and the pinned shellcheck on the shell scripts. Then the web app's gates (`web/package.json`'s scripts): it installs `web/node_modules` with the pinned pnpm (`--frozen-lockfile`), builds `web/dist`, and runs Biome (format and lint), Oxlint with type-aware rules, `tsc` (TypeScript 7), knip (unused files, exports and dependencies) and Vitest; each failure names its fix. Property tests and Vitest's shuffled order run with a fixed seed. Database tests use the warm Postgres 18 container (below). It first fails when the changes since the merge base with the default branch are larger than the Gemini review accepts (the diff `review` sends, without generated files), so an oversized change is split before CI; files not yet added to git are not counted. `check`, `coverage`, `mutants`, `fuzz` and `e2e` hold one gate lock (`target/gate.lock`, released when the gate ends or dies) so that only one heavy gate runs at a time: a second waits, naming the one running; `check --wait-at-most <seconds>`, as the stop hook runs it (300 s), exits 75 once that time has passed instead. |
| `cargo xtask check --changed` | The same, limited to the crates affected by changes since the merge base with the default branch (`upstream/HEAD`, else `origin/HEAD`), and to the web gates when `web/` or `xtask/tools.toml` changed. The `abc` crate embeds `web/dist`, so a change to the main workspace builds the web app first when no build is there. |
| `cargo xtask build` | Builds the web app with the pinned pnpm, then the `abc` binary with it embedded (`cargo build --release -p abc`), into `target/release/abc`. A plain `cargo build` of `abc` without a web build fails and names this command. |
| `cargo xtask pnpm [args]` | Runs the pinned pnpm in `web/` with the pinned node first on `PATH`: `install` after `web/package.json` changes, `run dev` for Vite's dev server, `exec biome check --write` to format. pnpm refuses packages published less than 3 days ago (`web/pnpm-workspace.yaml`). |
| `cargo xtask e2e` | Builds abc as `build` does, starts `abc server` on a fresh database on the test Postgres server behind `web/e2e/sidecar.ts`, a stand-in for the Tailscale sidecar that answers over HTTPS on the server's public origin, as Serve does, and runs Playwright's tests (`web/e2e/`) in Chromium, which it installs first; on Linux, `cargo xtask pnpm exec playwright install --with-deps chromium` installs its system libraries, and agent VMs have Chromium in their image. |
| `cargo xtask fuzz [--seconds N]` | Fuzzes protocol message decoding (target `decode` in `fuzz/`, a Cargo workspace of its own so normal builds never compile libFuzzer) with the pinned cargo-fuzz for N seconds (default 60), starting from the committed seeds in `fuzz/seeds/decode/`. It first runs rustfmt and clippy (`--locked`, warnings denied) on `fuzz/`, which `check` does not cover; `fuzz/Cargo.toml` copies the root's lints, and an xtask test keeps them equal. A failing input is saved under `fuzz/artifacts/decode/`; once fixed, add it to the seeds. |
| `cargo xtask integrity --base <rev> [--description <file>]` | Reads Rust tests and the web app's (`*.test.ts`, `*.test.tsx`, where `.skip`, `.todo`, `.fixme` and `x`-prefixed calls count as skipped and `.fails` as `should_panic`). Lists deleted test files and tests, tests a commit of the branch added and a later one deleted, newly skipped or `should_panic` tests, tests with fewer assertions, and changed gate files since the merge base. In CI it fails on any of them unless an approver listed in `.github/integrity-approvers` at the merge base labelled the PR `integrity-approved` after its last push and last description edit; a run for a description edit treats any earlier label as stale. Each flagged test change also needs a reason in the PR description, label or not, and the description names no test it does not flag (below). |
| `cargo xtask coverage --base <rev>` | Runs the tests of both workspaces under cargo-llvm-cov and the web app's under Vitest's coverage-v8, and fails unless 90% of the changed, instrumented lines of shipped code, Rust and TypeScript together, are covered, listing the ones that are not. Test code (`#[cfg(test)]` items, test functions, test modules in their own files, `tests/`) does not count. |
| `cargo xtask mutants --base <rev> [--jobs N] [--shard k/n]` | Runs cargo-mutants on the Rust lines changed since the merge base, or one shard of them, and fails on any missed mutant: a change to the code that no test notices. A mutant whose tests hang counts as caught. The main workspace's mutants build in copies of the tree; xtask's are tested in place, one at a time, because its tests read the repository around it. |
| `cargo xtask review --base <rev>` | Gemini reviews the diff, the PR's title and description against this file as it stands at the merge base. Runs in CI; it blocks on a `block` verdict or any blocking finding, and fails closed on quota, size and missing-key errors. |
| `cargo xtask task set "<text>"` / `show` / `clear` | Records, prints or forgets the current task in `.abc/task.md` (not committed), for the watchdog. |
| `cargo xtask task blocked "<text>"` | Declares the task BLOCKED, in the stop hook's state (`.abc/stop-state.json`): what is missing and what was tried. The next Stop logs it in `.abc/blocked-log.md` and asks the watchdog whether the blocker is outside the agent's control; if so, or after 5 blocked stops in a row, the stop is allowed with this handoff, and otherwise it is sent back and counted (D39). |
| `cargo xtask gh -- <args>` | Runs gh as the agent App with a token from the broker, revoked when gh exits. Agents merge and change things on GitHub only through this or `cargo xtask pr`; the guard blocks plain `gh pr merge` and `gh api` writes. |
| `cargo xtask pr open --title <t> --body <file>` / `pr update <number> --body <file>` | Opens a pull request from the pushed branch, or replaces one's description, through the REST API as the agent App. First it refuses a tree with uncommitted changes, a `## Test changes` section that misses a test integrity flags or names one it does not, a diff over the review's limit counting the title and description, and, when `.github/` changed, rulesets that fail `rulesets check`. When someone else pushed the pull request's head last (the maintainer's workflow commits), `update` refuses unless HEAD changes the diff (`git diff --quiet`), since GitHub counts no other push as reviewable, and says to push it. The guard blocks `gh pr create` and `gh pr edit`, through `cargo xtask gh` too. |
| `cargo xtask pr test-changes` | Prints the `## Test changes` section for the current branch, naming every test integrity flags; only the reasons are left to write. |
| `cargo xtask pr checks <number>` | Lists the failing, cancelled and running checks of the pull request's head and of its latest merge-queue run, with each failing check's annotations: two API calls and one App token, read from GitHub's API, which agent VMs reach, unlike job logs. |
| `cargo xtask git-credential <op>` | git's credential helper for github.com in agent VMs (`xtask/vm/gitconfig`): it answers with an agent App token from the broker. Never run it yourself. |
| `cargo xtask broker --session <name>` | Started by `cargo xtask vm start`, one per session, from a build of main installed outside any checkout: serves that agent VM's App tokens, at most 120 an hour, and revokes them all when the session stops; the one process that reads the App's key. |
| `cargo xtask vm start\|shell\|auth\|sync\|push\|stop <worktree>`, `vm run <worktree> --log <file> -- <cmd>`, `vm gc [--yes]`, `vm image`, `vm ls` | For the maintainer only, from the main clone: runs agent sessions in microsandbox microVMs, one per worktree (D37; CONTRIBUTING.md). `vm run` drives a session with no terminal, writing the output to the log line by line; `vm gc` lists the stopped sessions whose worktree is gone or whose branch GitHub merged, with their disk, and purges them with `--yes`. `stop` always stops the session's broker: one that does not answer gets SIGTERM, then SIGKILL. In a VM, every tool call and stop fails, naming the fix, while `target/` or `.tools/` is not the session's own disk (`xtask/vm/own-disks`); never delete or move them while a VM runs. |
| `cargo xtask rulesets check` | Checks `.github/rulesets/main.json` against the CI workflows: every job that runs on pull requests and in the merge queue is a required check. |
| `cargo xtask rulesets apply` | For the maintainer only, with their own gh login: puts `.github/rulesets/` on GitHub (`main`, the `agent/*` branch limit and the tag rule), replacing the rulesets they supersede. Agents never run it. |
| `cargo xtask tool <name> [args]` | Runs a pinned tool from `.tools/`, such as `cargo xtask tool cargo-deny check`. Never run these tools from `PATH`. |
| `cargo xtask pin` | After a version changes in `xtask/tools.toml`, records each target's archive sha256 and its executable's, or a crate's sha256 from the crates.io index. To add a tool, write zeros and run it. |
| `cargo fmt --all && cargo fmt --all --manifest-path xtask/Cargo.toml` | Fixes formatting in both workspaces; `cargo fmt --all` alone formats only the main one. |

The gates take `--root <path>` to check another tree with the pinned tools of the checkout xtask
was built from.

Database tests get a fresh, uniquely named database each (`abc_store::testing::TestDatabase`) on
the server `ABC_TEST_DATABASE_URL` names. `.cargo/config.toml` sets it to one warm Postgres 18
container per machine, `abc-postgres` on `127.0.0.1:55432`: the pinned `postgres:18.6` image,
`--restart unless-stopped`, data on tmpfs, and trust authentication published on loopback only,
so nothing off the machine can reach it. `check`, `coverage` and `mutants` start it with Docker
when it is not running; setting `ABC_TEST_DATABASE_URL` points the tests at another server instead,
and a `DATABASE_URL` is never used. Test databases are dropped when their test ends, and leftovers
of killed runs (`abc_test_*`, `abc_query_cache_*`) are swept after a day. sqlx's query macros
always read the committed `.sqlx/` cache (`SQLX_OFFLINE`), and `check` fails when a cache no
longer matches its queries, saying how to regenerate it.

CI runs `check`, `e2e`, `integrity`, `coverage`, `mutants` (in eight shards, reported as one
`Mutants` check), `review` and `fuzz` (for a minute; for an hour nightly on `main`) on every pull
request, and
`cargo xtask tool cargo-deny check`, `cargo xtask tool cargo-shear --deny-warnings --locked`, both
again on the fuzz workspace (`cargo xtask tool cargo-deny --manifest-path fuzz/Cargo.toml --config
fuzz/deny.toml --locked check`, `cargo xtask tool cargo-shear --deny-warnings --locked fuzz`) and
on xtask's (`cargo xtask tool cargo-deny --manifest-path xtask/Cargo.toml --config xtask/deny.toml
--locked check`, `cargo xtask tool cargo-shear --deny-warnings --locked xtask`), `cargo xtask tool
osv-scanner scan source --lockfile Cargo.lock --lockfile fuzz/Cargo.lock --lockfile
xtask/Cargo.lock --lockfile web/pnpm-lock.yaml`, `cargo
xtask tool zizmor --pedantic .github`, `cargo xtask tool pinact run --check --verify-comment` and
`cargo xtask tool gitleaks git --redact --no-banner --verbose .`. Every job is a required check on
`main`. Integrity, Coverage, Mutants and Gemini review run the base commit's xtask on the pull
request's tree (`--root`), so a pull request cannot change the gates that judge it; a pull request
that adds a gate fails that gate, which says it is bootstrapping. Only the review job has
`GEMINI_API_KEY`, and it runs nothing from the pull request's tree. Inside GitHub Actions
(`GITHUB_ACTIONS=true`), `review`, `mutants`, `coverage` and `integrity` also print their findings
as annotations, at most ten of each level per step as GitHub keeps them; elsewhere their output is
unchanged.

A PR that deletes, skips or weakens a test explains each one in its description, in a
`## Test changes` section: one entry per test, naming the test in backticks as the integrity check
reports it (`<file>::<test>`, or the file for a deleted test file), then the requirement or
behaviour that changed. An entry for a test the check does not flag fails it too. A test change
that only makes the code pass is not a reason; the Gemini review blocks it. `cargo xtask pr
test-changes` prints the section with every flagged test. `.github/pull_request_template.md` has the section, with "None" for a PR that
changes no tests.

```markdown
## Test changes

- `xtask/src/a.rs::tests::accepts_tabs`: tabs are now rejected (plan step 3), so this test is gone.
```

The gate files are the files that decide what passes: CI and rulesets (`.github/`), cargo, lint,
toolchain and tool configuration, `Cargo.toml` and `Cargo.lock` (the fuzz workspace's too, with
`fuzz/deny.toml`), the web app's manifest, lockfile, pnpm, TypeScript, Vite, Biome, Oxlint, knip and
Playwright configuration, the agent settings, this file and `CLAUDE.md`, and all of `xtask/`, its
workspace's manifest, lockfile and `deny.toml` included. `GATES` in
`xtask/src/integrity/mod.rs` lists them, and a test keeps `.github/CODEOWNERS` identical. A change
to one needs the integrity approval and a code owner's review.

**Break glass.** The ruleset's one bypass lets a repository admin merge a pull request without
its checks or reviews; GitHub logs every use. Use it only when a gate itself is broken and
blocks every pull request, when Gemini is down for longer than the change can wait, or for the
maintainer's own gate-file change, which no other approver can review. Say why in the pull
request.

Agents push only branches named `agent/<topic>`: the `branches` ruleset refuses the agent App
every other branch, and the `tags` ruleset every tag. git tokens cannot change workflow files;
a change that needs one goes to the maintainer.

The git guard in the agent hooks catches accidents: force pushes, git or gh commands it cannot
read, gh changes made as the user instead of through `cargo xtask gh`, and pull requests opened or
edited with gh instead of `cargo xtask pr`. It cannot stop a
deliberate bypass, since no text check of a shell command can, and it does not try to. Enforcement
is on GitHub: the ruleset on `main` forbids force pushes and deletion and requires a code owner's
approval and every check, and agents act as the getabc-dev-agent GitHub App with limited
permissions.

Commit the `proptest-regressions/` files proptest writes when a property test fails: they replay
the failing cases first on every run.

Lints are set for every crate in the root `Cargo.toml`. `#[allow]` is denied: silence a lint only
with `#[expect(lint, reason = "...")]`, and only when the code is right.
