# Contributing to abc

Thank you for helping. abc is in early development, so expect large changes and few stable parts.

[AGENTS.md](AGENTS.md) is the rulebook for everyone who changes this repository, people and agents
alike. Read it first. Reviewers cite its rules by ID, such as P3 or I2. This guide does not repeat
them.

## AI policy

abc is built by agents. Agents write the code and tests; people decide and review (rule A1).

- **AI-written code is welcome and normal.** It is how this project is built.
- **The same gates apply to everyone.** Every change, from a person or an agent, must pass
  `cargo xtask check`, CI and review before it merges.
- **Tests are never weakened to make code pass** (rule P3). Do not skip, loosen or delete a test to
  get green; fix the cause. If your PR changes an existing test, explain why in a
  `## Test changes` section of the PR description. A test change without a stated reason will not
  be merged.
- **You are accountable for what you submit.** Read and understand your change, whoever or
  whatever wrote it. "The agent did it" is not an answer in review.
- **Follow AGENTS.md.** It binds agents and people equally.

## Set up

You need git, rustup and network access. The Rust toolchain is pinned in `rust-toolchain.toml`,
and rustup installs it on first use.

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

Before you open a PR, run the gate that CI runs:

```sh
cargo xtask check            # format, clippy, tests, doctests and rustdoc on the whole workspace
cargo xtask check --changed  # the same, limited to the crates your change affects
cargo fmt --all              # fix formatting
```

The full list of `cargo xtask` commands and the extra scans CI runs are in the Commands section of
[AGENTS.md](AGENTS.md#commands).

If you use Claude Code, the hooks in `.claude/settings.json` run `cargo xtask check --changed`
when the agent stops, and [CLAUDE.md](CLAUDE.md) explains them.

## Open a pull request

- Keep one change per PR, and describe what it does and why.
- Add a `## Test changes` section whenever you change an existing test (see the AI policy).
- CI must pass. Reviews follow rule B1 in AGENTS.md.

## Report a security issue

Do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Conduct

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Licence

By contributing, you agree that your contribution is licensed under the [MIT licence](LICENSE).
