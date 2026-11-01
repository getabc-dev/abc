//! Repository automation for abc, run as `cargo xtask <command>`.

mod check;
mod guard;
mod hook;
mod shell;
#[cfg(test)]
mod testing;
mod tools;

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

fn run(task: Task) -> anyhow::Result<ExitCode> {
    let root = repo_root()?;
    match task {
        Task::Check { changed } => {
            let passed = check::run(&root, changed, &mut io::stderr().lock())?;
            Ok(if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Task::Setup { tools: only } => {
            tools::setup(&root, &only, &mut io::stdout().lock())?;
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
            let code = hook::run(event, &input, &context, &mut io::stderr().lock())?;
            Ok(ExitCode::from(code))
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

    use super::{Cli, repo_root, usage_exit_code};

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
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
