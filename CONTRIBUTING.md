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
cargo xtask setup    # install the pinned tools into .tools/, verifying each download
cargo xtask doctor   # confirm every pinned tool is installed at its pinned version
```

Run `cargo xtask setup` again in every new worktree, and whenever `xtask/tools.toml` changes.

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
cargo fmt --all              # fix formatting; the gate only checks it
cargo xtask check            # format check, clippy, tests, doctests and rustdoc on the workspace
cargo xtask check --changed  # the same, limited to the crates your change affects
```

CI runs more gates on every PR: Integrity, Coverage, Mutants, a Gemini review and the dependency,
secret and workflow scans. The Commands section of [AGENTS.md](AGENTS.md#commands) lists them with
the command behind each.

## Test changes

The Integrity check compares your PR with its merge base and flags:

- deleted test files and deleted tests;
- tests newly marked `#[ignore]` or `#[should_panic]`;
- tests with fewer assertions than before;
- changes to gate files: CI and rulesets (`.github/`), cargo, lint, toolchain and tool
  configuration, `Cargo.toml` and `Cargo.lock` (the fuzz workspace's too, with its `deny.toml`),
  the agent settings, `AGENTS.md`, `CLAUDE.md` and `xtask/`. AGENTS.md has the full list. Gate files also need a code owner's review.

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
`xtask/vm/`, `rust-toolchain.toml` or the pinned tools change.

Per session:

```sh
git worktree add ../abc-my-change -b agent/my-change origin/main
cargo xtask vm start ../abc-my-change    # starts the broker if needed; a few seconds
cargo xtask vm shell ../abc-my-change    # Claude Code as the agent; add `bash` for a shell
cargo xtask vm auth ../abc-my-change     # how Claude Code is signed in, without secrets
cargo xtask vm sync ../abc-my-change     # move the worktree's branch to the VM's commits
cargo xtask vm stop ../abc-my-change     # revokes its tokens; --purge also drops its volumes
```

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
