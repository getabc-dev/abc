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
pinned in `xtask/tools.toml` are built for those three. Claude Code sessions in this repository
need macOS for now (see [Claude Code](#claude-code)).

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
  configuration, `Cargo.toml` and `Cargo.lock`, the agent settings and hook scripts, `AGENTS.md`,
  `CLAUDE.md` and `xtask/`. AGENTS.md has the full list. Gate files also need a code owner's review.

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
[CLAUDE.md](CLAUDE.md) for the details. These parts affect contributors:

- **The sandbox.** Every Bash command runs in Claude Code's sandbox, with no way to rerun it
  outside. It cannot read your gh login, your keychains or `~/.ssh/`, write outside the checkout,
  or reach hosts other than GitHub, crates.io and the toolchain's and pinned tools' downloads.
  `~/.cargo` is read-only in it, so run `cargo fetch` yourself when a change adds a crate. Agents
  never use Docker. Sessions need macOS for now: the hooks run repository code only under macOS
  Seatbelt, and elsewhere every Bash call is blocked.
- **git credentials.** Its `env` block points git's credential helper for github.com at
  `cargo xtask git-credential`, which asks the maintainers' token broker for an agent App token.
  Without that broker, git cannot push from your sessions, and the sandbox keeps your own
  credentials out of them too. Push and open your PR from your own terminal.
- **The hooks.** Hooks run outside the sandbox, so they run repository code through
  `.claude/hooks/jail.sh`. When the agent stops, the Stop hook runs `cargo xtask check --changed`
  in it, then the watchdog, which runs your own `claude -p` with Haiku on the diff. It uses your
  Claude quota. Every edit Claude makes under `.claude/` asks you first.
- **Plugins.** The file enables the `pstack@cursor-plugins` and `egm@egm` plugins from GitHub
  marketplaces that are not pinned to a commit. Plugins can ship hooks and MCP servers that run as
  you, outside the sandbox. Review them before you trust the folder in Claude Code, as the
  maintainers do before adopting a pack (rule B1).

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
