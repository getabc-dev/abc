//! Repository automation for abc, run as `cargo xtask <command>`.

mod app;
mod check;
mod coverage;
mod git;
mod guard;
mod hook;
mod integrity;
mod mutants;
mod review;
mod rulesets;
mod shell;
mod task;
#[cfg(test)]
mod testing;
mod tools;
mod watchdog;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Repository automation for abc")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

/// What `cargo xtask rulesets` does.
#[derive(Debug, Subcommand)]
enum RulesetAction {
    /// Check .github/rulesets/main.json against the CI workflows.
    Check,
    /// Put the ruleset on GitHub with your own gh login, replacing the rulesets it supersedes.
    Apply,
}

/// What `cargo xtask task` does with the current task.
#[derive(Debug, Subcommand)]
enum TaskAction {
    /// Record the task you are starting, for the stop hook's watchdog.
    Set {
        /// What the task is, in a sentence or two.
        text: String,
    },
    /// Print the recorded task.
    Show,
    /// Forget the recorded task.
    Clear,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Check formatting, run clippy, the tests, the doctests and rustdoc: the gate stop hooks and
    /// CI share.
    Check {
        /// Check only the crates affected by changes since the merge base with the default branch
        /// (upstream/HEAD, else origin/HEAD), including uncommitted and untracked files.
        #[arg(long)]
        changed: bool,
    },
    /// Run the tests under coverage and require 90% of the changed, instrumented lines covered.
    Coverage {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
    },
    /// Run cargo-mutants on the Rust lines changed since the merge base and fail on missed mutants.
    Mutants {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
        /// How many mutants to build and test at once.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
        jobs: u32,
    },
    /// Report deleted, skipped or weakened tests and changed gate files since the merge base.
    Integrity {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
        /// In CI, the pull request whose `integrity-approved` label can accept the findings. Its
        /// description is read from the event payload at `GITHUB_EVENT_PATH`.
        #[arg(long)]
        pr: Option<u64>,
        /// A file holding the pull request description, whose "## Test changes" section must give
        /// a reason for each flagged test change; read instead of the event payload.
        #[arg(long)]
        description: Option<PathBuf>,
    },
    /// git's credential helper: answers `get` for github.com with an agent App token on stdout.
    GitCredential {
        /// The operation git asks for: get, store or erase.
        operation: String,
    },
    /// Run gh as the agent App, with a fresh installation token in `GH_TOKEN`.
    Gh {
        /// Arguments passed to gh.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Have Gemini review the changes since the merge base against AGENTS.md (D8).
    Review {
        /// The revision whose merge base with HEAD the diff starts from.
        #[arg(long)]
        base: String,
    },
    /// Record, show or clear the task this worktree's agent is working on.
    #[command(name = "task")]
    Work {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Check or apply the branch rules for main in .github/rulesets/main.json.
    Rulesets {
        #[command(subcommand)]
        action: RulesetAction,
    },
    /// Install the tools pinned in xtask/tools.toml into .tools/, verifying each download.
    Setup {
        /// Install only these tools; all of them when none are named.
        tools: Vec<String>,
    },
    /// Check that every pinned tool, or each one named, is installed at its pinned version.
    Doctor {
        /// Check only these tools; all of them when none are named.
        tools: Vec<String>,
    },
    /// Record the sha256 of every pinned tool's archive for every target in xtask/tools.toml.
    Pin,
    /// Run a pinned tool from .tools/ in the repository root.
    Tool {
        /// The tool's name in xtask/tools.toml.
        name: String,
        /// Arguments passed to the tool.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run a Claude Code hook on the hook input from stdin, in the checkout the agent is in.
    /// Exit code 2 blocks the tool call or keeps the agent working.
    Hook {
        /// The hook event.
        #[arg(value_enum)]
        event: hook::Event,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = usage_exit_code(&error);
            return match error.print() {
                // With the terminal gone there is nowhere left to report; the code still tells.
                Ok(()) | Err(_) => ExitCode::from(code),
            };
        }
    };
    match run(cli.command) {
        Ok(code) => code,
        Err(error) => match writeln!(io::stderr().lock(), "error: {error:#}") {
            // With stderr gone there is nowhere left to report; the exit code still fails.
            Ok(()) | Err(_) => ExitCode::FAILURE,
        },
    }
}

/// The exit code for clap's help, version and usage errors. Clap exits 2 on usage errors, but 2
/// is how a Claude Code hook blocks an action, so a hook command xtask does not understand exits 1
/// and reports a hook error instead of posing as a decision.
fn usage_exit_code(error: &clap::Error) -> u8 {
    u8::from(error.use_stderr())
}

/// Success when a gate passed, failure otherwise.
fn verdict(passed: bool) -> ExitCode {
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run(task: Task) -> anyhow::Result<ExitCode> {
    let root = repo_root()?;
    let report = &mut io::stderr().lock();
    match task {
        Task::Check { changed } => Ok(verdict(check::run(&root, changed, report)?)),
        Task::Coverage { base } => Ok(verdict(coverage::run(&root, &base, report)?)),
        Task::Mutants { base, jobs } => Ok(verdict(mutants::run(&root, &base, jobs, report)?)),
        Task::Integrity {
            base,
            pr,
            description,
        } => {
            let pull_request = pr.map(integrity::PullRequest::from_env).transpose()?;
            let event = pr.and_then(|_| std::env::var_os("GITHUB_EVENT_PATH").map(PathBuf::from));
            let description = integrity::description(description.as_deref(), event)?;
            Ok(verdict(integrity::run(
                &root,
                &base,
                pull_request.as_ref(),
                description.as_deref(),
                report,
            )?))
        }
        Task::Review { base } => Ok(verdict(review::run_live(&root, &base, report)?)),
        Task::GitCredential { operation } => {
            let mut request = String::new();
            io::stdin()
                .read_to_string(&mut request)
                .context("could not read git's credential request")?;
            app::credential(&operation, &request, &mut io::stdout().lock(), || {
                app::App::load(&app::config_dir()?)?.token(app::GITHUB_API)
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Gh { args } => {
            let token = app::App::load(&app::config_dir()?)?.token(app::GITHUB_API)?;
            app::gh(Path::new("gh"), &args, &token)
        }
        Task::Rulesets { action } => {
            match action {
                RulesetAction::Check => {
                    rulesets::check(&root)?;
                    writeln!(report, "rulesets: {} matches CI", rulesets::RULESET)?;
                }
                RulesetAction::Apply => {
                    rulesets::apply(&root, Path::new("gh"), &mut io::stdout().lock())?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Task::Work { action } => {
            match action {
                TaskAction::Set { text } => task::set(&root, &text)?,
                TaskAction::Show => match task::read(&root)? {
                    Some(text) => writeln!(io::stdout().lock(), "{text}")?,
                    None => writeln!(report, "no task recorded")?,
                },
                TaskAction::Clear => task::clear(&root)?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Task::Setup { tools: only } => {
            let out = &mut io::stdout().lock();
            tools::setup(&root, &only, out)?;
            app::configure_git(&root, &app::config_dir()?, out)?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Doctor { tools: only } => tools::doctor(&root, &only, &mut io::stdout().lock()),
        Task::Pin => {
            tools::pin(&root, &mut io::stdout().lock())?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Tool { name, args } => tools::run(&root, &name, &args),
        Task::Hook { event } => {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .context("could not read the hook input from stdin")?;
            let context = hook::Context::from_env(root);
            Ok(ExitCode::from(hook::run(event, &input, &context, report)?))
        }
    }
}

/// The repository root: the workspace that contains xtask.
fn repo_root() -> anyhow::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask is not inside the repository")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::CommandFactory;

    use clap::Parser as _;

    use super::{Cli, RulesetAction, Task, repo_root, run, usage_exit_code, verdict};

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dispatches_commands() {
        assert_eq!(
            run(Task::Rulesets {
                action: RulesetAction::Check
            })
            .unwrap(),
            std::process::ExitCode::SUCCESS,
            "the checked-in ruleset is in step with CI"
        );
        assert_eq!(
            verdict(true),
            std::process::ExitCode::SUCCESS,
            "a passing gate"
        );
        assert_eq!(
            verdict(false),
            std::process::ExitCode::FAILURE,
            "a failing gate"
        );
    }

    #[test]
    fn usage_errors_never_exit_2() {
        let unknown = Cli::try_parse_from(["xtask", "guard"]).unwrap_err();
        assert_eq!(
            usage_exit_code(&unknown),
            1,
            "an unknown command must not read as a hook's block"
        );
        let bad_event = Cli::try_parse_from(["xtask", "hook", "post-tool-use"]).unwrap_err();
        assert_eq!(
            usage_exit_code(&bad_event),
            1,
            "an unknown hook event fails with 1"
        );
        let help = Cli::try_parse_from(["xtask", "--help"]).unwrap_err();
        assert_eq!(usage_exit_code(&help), 0, "help is not an error");
    }

    #[test]
    fn workspace_rust_version_is_the_pinned_toolchain() {
        let root = repo_root().unwrap();
        let toolchain: toml_edit::DocumentMut =
            fs::read_to_string(root.join("rust-toolchain.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let manifest: toml_edit::DocumentMut = fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            manifest["workspace"]["package"]["rust-version"].as_str(),
            toolchain["toolchain"]["channel"].as_str(),
            "Cargo.toml's rust-version must equal the channel in rust-toolchain.toml"
        );
    }
}
