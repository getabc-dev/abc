# Contributing to abc

Thank you for helping. abc is in early development, so expect large changes and few stable parts.

[AGENTS.md](AGENTS.md) is the rulebook for every change to this repository, whoever makes it. Read
it first. Reviewers cite its rules by ID, such as P3 or I2. This guide does not repeat them.

## Who may contribute

Pull requests from people and from agents are both welcome. AI-written code is normal here: rule
A1 describes how the maintainers build abc, with agents writing the code and people deciding and
reviewing.

- **The same gates apply to every author.** Every change must pass `cargo xtask check`, every CI
  check and a code owner's review before it merges.
- **Tests are never weakened to make code pass** (rule P3). Do not skip, loosen or delete a test to
  get green; fix the cause. A test change needs a stated reason (see
  [Test changes](#test-changes)).
- **You answer for what you submit.** Read and understand your change, whoever or whatever wrote
  it. "The agent did it" is not an answer in review.
- **Follow AGENTS.md.**

## Set up

Supported platforms are macOS on Apple silicon (aarch64) and Linux on x86_64 or aarch64: the tools
pinned in `xtask/tools.toml` are built for those three.

You need:

- git;
- rustup, which installs the toolchain pinned in `rust-toolchain.toml` on first use;
- a C linker: the Xcode Command Line Tools on macOS (`xcode-select --install`), or a C compiler
  such as gcc or clang on Linux;
- network access, to download the pinned tools.

```sh
git clone https://github.com/getabc-dev/abc.git
cd abc
cargo xtask setup    # install the pinned tools into .tools/, verifying each download,
                     # node and pnpm for the web app among them
cargo xtask doctor   # confirm every pinned tool is installed at its pinned version
```

Run `cargo xtask setup` again in every new worktree, and whenever `xtask/tools.toml` changes. You
never need a node or pnpm of your own: `cargo xtask pnpm` runs the pinned ones.

## Build and run abc

The `abc` binary embeds the web app (`web/`), so build it with xtask, which builds the web app
first:

```sh
cargo xtask build    # web/dist with the pinned pnpm, then target/release/abc with it embedded
```

A plain `cargo build` of the `abc` crate without a web build fails and names `cargo xtask build`.
`abc server` serves the app on its sidecar socket beside the API, behind the Tailscale sidecar.

To work on the web app with Vite's dev server and its instant reloads:

```sh
cargo xtask pnpm install     # web/node_modules from web/pnpm-lock.yaml
cargo xtask pnpm run dev     # http://localhost:5173
cargo xtask pnpm add -E <package>@<version>   # add a dependency; commit web/pnpm-lock.yaml
```

pnpm refuses any package published less than 3 days ago, as Renovate waits 3 days to propose
one. To see the app served by the real binary in Chromium, as CI does:

```sh
cargo xtask e2e      # build abc, serve it on a fresh database, run web/e2e/ with Playwright
```

On Linux, Chromium needs system libraries: `cargo xtask pnpm exec playwright install --with-deps
chromium` installs them with sudo. In an agent VM, Chromium and its libraries come with the image.

## Work on a change

Use one git worktree per change, so parallel changes and agents never share a checkout:

```sh
git fetch origin
git worktree add -b <branch> ../abc-<branch> origin/main
cd ../abc-<branch>
cargo xtask setup
```

Before you open a PR, run the gate locally:

```sh
cargo fmt --all              # fix formatting; the gate only checks it (and xtask's:
                             # cargo fmt --all --manifest-path xtask/Cargo.toml)
cargo xtask check            # format, clippy, tests, doctests and rustdoc on both workspaces,
                             # shellcheck on the shell scripts, and the web app's Biome,
                             # Oxlint, tsc, knip and Vitest
cargo xtask check --changed  # the same, limited to the crates your change affects, and to the
                             # web gates when web/ changed
cargo xtask pnpm exec biome check --write   # format the web app and apply Biome's safe fixes
```

Run gates one at a time, in the foreground. `check`, `coverage`, `mutants`, `fuzz` and `e2e`
share one lock, `gate.lock` in the checkout's git directory (`.git/worktrees/<name>/` for a
worktree), where neither `cargo clean` nor deleting `target/` can remove it from under a running
gate: a second gate waits for the first and says once which gate it waits for, and the Stop hook
waits up to five minutes, then sends the agent back to wait rather than start `check` beside
another gate. The lock is the operating system's, so a gate that was killed leaves nothing that
blocks the next. Two gates at once do not fit in an agent VM's memory: in S0 PR 5a the
out-of-memory killer took the linker, twice.

CI runs more gates on every PR: End-to-end (`cargo xtask e2e`), Integrity, Coverage, Mutants, a
Gemini review and the dependency, secret and workflow scans. The Commands section of [AGENTS.md](AGENTS.md#commands) lists them with
the command behind each.

## Test changes

The Integrity check compares your PR with its merge base and flags:

- deleted test files and deleted tests;
- tests newly marked `#[ignore]` or `#[should_panic]`;
- tests with fewer assertions than before;
- changes to gate files: CI and rulesets (`.github/`), cargo, lint, toolchain and tool
  configuration, `Cargo.toml` and `Cargo.lock` (the fuzz workspace's too, with its `deny.toml`),
  the agent settings, `AGENTS.md`, `CLAUDE.md` and `xtask/`. AGENTS.md has the full list. Gate
  files also need a code owner's review.

Each flagged test needs an entry in the `## Test changes` section of your PR description, in the
form `` `path::test`: reason ``. Name the test as the check reports it (`<file>::<test>`, or the
file alone for a deleted test file), then the requirement or behaviour that changed:

```markdown
## Test changes

- `xtask/src/a.rs::tests::accepts_tabs`: tabs are now rejected (plan step 3), so this test is gone.
```

The pull request template fills the section with "None", which is right only when no test change
was flagged. "To make the code pass" is not a reason, and the Gemini review blocks it.

Any finding, test or gate file, also fails the check until an approver listed in
`.github/integrity-approvers` adds the `integrity-approved` label. The label must come after your
last push and last description edit; a new push or edit makes it stale. The label approves the
reasons, so the entries are needed with or without it.

## Claude Code

If you work with Claude Code, the committed `.claude/settings.json` applies to your sessions. Read
[CLAUDE.md](CLAUDE.md) for the details. Two parts affect contributors:

- **The watchdog.** When the agent stops, the Stop hook runs `cargo xtask check --changed` and then
  the watchdog, which runs your own `claude -p` with Haiku on the diff. It uses your Claude quota.
- **Plugins.** The file enables the `pstack@cursor-plugins` and `egm@egm` plugins from GitHub
  marketplaces that are not pinned to a commit. Plugins can ship hooks and MCP servers that run as
  you. Review them before you trust the folder in Claude Code, as the maintainers do before
  adopting a pack (rule B1).

## Agent sessions in microVMs (maintainers)

The maintainers run every agent session in a microVM of its own, one per worktree (D37):
`cargo xtask vm` starts [microsandbox](https://github.com/superradcompany/microsandbox), pinned in
`xtask/tools.toml`. The VM sees only its worktree and a git repository holding only that branch;
its network reaches only the hosts in `xtask/vm/egress-allow.txt`; git and `cargo xtask gh` act as
the getabc-dev-agent App with tokens from a broker on the host, which alone reads the App's key;
and Claude Code runs on your subscription only. [docs/threat-model.md](docs/threat-model.md) says
what the VM keeps out and what it does not.

It needs macOS on Apple silicon with Apple's [`container`](https://github.com/apple/container) to
build the image (`brew install container`, then `container system start`), or Linux with KVM
(`/dev/kvm` readable and writable) and Docker or Podman. Run every `cargo xtask vm` command from
your main clone, never from a worktree an agent works in: they run on the host with your access.
xtask refuses to run them from a linked worktree, which prevents the mistake but is no control.

Once:

```sh
# The token broker, from main, outside any checkout (`vm start` starts one per session):
cargo install --locked --git https://github.com/getabc-dev/abc --branch main xtask \
  --root ~/.local/share/abc/broker
# The App's files, readable by you alone: app.json ({"client_id": ..., "installation_id": ...})
# and private-key.pem (mode 600), in ~/.config/abc/agent-app/.
# Your Claude subscription token for the VMs:
claude setup-token
security add-generic-password -a "$USER" -s abc-vm-claude-code-oauth-token -w
# On Linux: secret-tool store --label abc-vm service abc-vm-claude-code-oauth-token
cargo xtask setup             # installs msb with the other tools
cargo xtask vm image          # builds the agent image and loads it (about 20 minutes)
```

Paste the token at the prompt, never on the command line. Rebuild the image when
`xtask/vm/`, `rust-toolchain.toml`, the pinned tools or `@playwright/test` in `web/package.json`
change: the image holds Playwright's Chromium, since VMs never download browsers.

Per session:

```sh
git worktree add ../abc-my-change -b agent/my-change origin/main
cargo xtask vm start ../abc-my-change    # starts the broker if needed; a few seconds
cargo xtask vm shell ../abc-my-change    # Claude Code as the agent; add `bash` for a shell
cargo xtask vm run ../abc-my-change --log run.log -- claude -p "..."
                                         # no terminal needed: the output reaches run.log line by
                                         # line while it runs, and it exits with the command's status
cargo xtask vm auth ../abc-my-change     # how Claude Code is signed in, without secrets
cargo xtask vm sync ../abc-my-change     # move the worktree's branch to the VM's commits
cargo xtask vm push ../abc-my-change     # bring commits made on the host into the running VM
cargo xtask vm stop ../abc-my-change     # revokes its tokens; --purge also drops its volumes
cargo xtask vm gc                        # lists stopped sessions whose worktree is gone or whose
                                         # branch GitHub merged, with their disk; --yes purges them
```

`vm gc` keeps a finished session whose own repository holds commits that are on no branch of
`origin` and in no merged pull request, naming them: the VM may have committed work nobody synced.
Fetch them from the repository it names, delete that repository, and run `vm gc --yes` again.

`vm shell` is the interactive way in. To drive a session from a script or another agent, use
`vm run`: it needs no pseudo-terminal, so no `script` wrapper, and writes stdout and stderr to the
log as they come (microsandbox holds back the output of a command without a terminal unless asked
to stream it, which `vm run` does). The log must be outside the worktree, which the VM can write,
and `vm run` refuses a symbolic link there rather than follow it.

The session's `target/` and `.tools/` are disks of the VM's own, mounted over those directories of
the worktree. **Never delete or move `target/` or `.tools/` in a worktree while its VM runs**:
the VM loses the disk, and builds would land on the worktree mount it shares with the host. The VM
notices: every tool call and stop fails, naming the fix, until you run `cargo xtask vm stop` and
`cargo xtask vm start` for the worktree. To reclaim their space, stop the session and purge it
(`vm stop --purge`, or `vm gc --yes` once its branch is merged).

`vm stop` always stops the session's VM, and its token broker: one that does not answer within a
minute gets SIGTERM, which makes it hang up on its clients, revoke its tokens and remove its
sockets and pid file in `~/.config/abc/agent-broker/`, and SIGKILL a minute later if it is still
the broker; its tokens then expire within the hour. When the broker may still run (it cannot be
signalled, outlives SIGKILL, or listens without a pid file), `vm stop` leaves its files, stops the
VM, and fails naming what to stop by hand.

`start` refuses unless `claude auth status` in the VM reports the subscription token
(`authMethod` `oauth_token`). The launcher strips `ANTHROPIC_API_KEY` and every other API route
from what it passes, and the VM refuses to start, or to run a session or a tool call, if one
appears in its environment or in a Claude Code settings file. To rotate the token, replace the
keychain item and restart the session; revoke old tokens in your claude.ai settings. A
setup-token token lasts about a year. In the VM only Claude Code gets it, though the commands
Claude Code runs inherit it; shells you open with `cargo xtask vm shell <worktree> bash` do not.

Agents push only `agent/<topic>` branches, and never tags or workflow changes: GitHub's rulesets
refuse the App the rest. Apply `.github/rulesets/` with `cargo xtask rulesets apply` (your own gh
login) when they change.

microsandbox writes the output of every command run in a VM, the Claude Code session included, to
`exec.log` under `~/.local/share/abc/msb/<version>/sandboxes/<name>/logs/` while the VM exists;
`stop` deletes the VM with its logs, so stop sessions you have finished. It is beta and breaks
compatibility between releases: after a version bump in `xtask/tools.toml`, run
`cargo xtask setup` and `cargo xtask vm image`, and `stop --purge` and `start` each session (they
hold only caches); each version keeps its state apart, so delete the old version's directory.

## Release abc (maintainers)

A tag `v<version>` that a maintainer pushes releases abc: the Release workflow
(`.github/workflows/release.yml`) builds the web app once, then abc with it embedded and abc's
private tailscaled and tailscale CLI for macOS and Linux on arm64 and x86_64, renders the Homebrew formula for them and installs, tests and lints it with
Homebrew on each of the four platforms, each on a native runner (`macos-26` for Apple silicon,
`macos-15-intel` for Intel Macs, `ubuntu-26.04` for x86_64 Linux and `ubuntu-24.04-arm` for
arm64 Linux). Only then does it attest every archive and `SHA256SUMS`, publish the GitHub Release
and push the formula to [getabc-dev/homebrew-tap](https://github.com/getabc-dev/homebrew-tap).
Every pull request that changes what a release builds from (the workspace, the web app,
`packaging/`, xtask or the two release workflows) runs the same builds, formula and installs, with
Rust caches, as the `Release dry run` check (`.github/workflows/release-dry-run.yml`), publishing
nothing; any other pull request skips them, and the check passes. A release never reads a cache.

**No tag `v*` is pushed before I2 lands.** The formula already installs Postgres 18 and abc's
tailscaled and tailscale, the foundation `abc server setup` (I2) builds on, but until I2 an install
gives a binary that sets nothing up.

1. **Bump the version.** In a pull request, set `[workspace.package] version` in `Cargo.toml` to
   the new release number, then update both lockfiles that name the workspace's crates, since
   `check` builds with `--locked` and never updates them itself, and run the checks:

   ```sh
   cargo update --workspace
   cargo update --workspace --manifest-path fuzz/Cargo.toml
   cargo xtask check
   ```

   Merge it.
2. **Push the tag** on that merge commit; only maintainers may create tags:

   ```sh
   git fetch origin && git tag --sign v0.2.0 origin/main && git push origin v0.2.0
   ```

   The workflow refuses a tag that is not the workspace's version, or whose commit is not on
   `main` (`cargo xtask release check-tag`). Releases queue: a tag pushed while another release
   runs waits for it, and only one waits, so push the next tag once the previous release is done.
3. **Check the release.** The run's jobs all pass; the release lists eight archives and
   `SHA256SUMS`; the tap has a commit `abc 0.2.0`; and on a clean machine:

   ```sh
   gh release download v0.2.0 --repo getabc-dev/abc --pattern 'abc-*-aarch64-apple-darwin.tar.gz'
   gh attestation verify abc-0.2.0-aarch64-apple-darwin.tar.gz --repo getabc-dev/abc
   brew install getabc-dev/tap/abc && abc --version
   ```

**When a run fails.** Use *Re-run failed jobs*: the re-run reuses the archives the run already
built and attested, so the publish job resumes the release where it stopped. It creates the
release as a draft only when the tag has none, uploads only the assets it lacks, never replaces
one, and publishes the draft once every asset is up; the tap job pushes only when the tap holds an
older formula, and refuses to move it backwards. A full re-run (*Re-run all jobs*) builds the
archives again, which need not match byte for byte the ones a draft already holds: the publish job
then refuses, and while the release is still a draft that nobody has seen, delete it and re-run.
A failure that the code causes is fixed in a pull request and released as a new version under a
new tag: never move or reuse a tag. The tailscaled and tailscale it carries, and the Go that builds them with
go.dev's sha256 for each archive, are pinned in `packaging/tailscale.toml`, which Renovate bumps,
and the formula's template is `packaging/homebrew/abc.rb`; `cargo xtask release` builds,
packages and renders them as the workflow does. `Formula/abc.rb` in the tap is rendered from that
template: never edit it there by hand.

Once, before the first release: create the public repository `getabc-dev/homebrew-tap`, and a
fine-grained token for that repository alone with Contents read and write. Store it as the secret
`HOMEBREW_TAP_TOKEN` of the `release` environment, which only the tap job uses, and limit that
environment's deployments to tags `v*`.

## Open a pull request

- Keep one change per PR. Fill in the template: what and why, the plan it carries out, and test
  changes.
- Every CI check must pass, and a code owner must approve after your last push.

## Report a security issue

Do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Conduct

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Licence

By contributing, you agree that your contribution is licensed under the [MIT licence](LICENSE).
