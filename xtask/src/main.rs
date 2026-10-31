//! Repository automation for abc, run as `cargo xtask <command>`.

mod check;
mod guard;
mod hook;
mod shell;
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
    /// Check formatting, run clippy and run the tests: the gate stop hooks and CI share.
    Check {
        /// Lint and test only the crates affected by changes since the merge base with
        /// origin/main, including uncommitted and untracked files.
        #[arg(long)]
        changed: bool,
    },
    /// Install the tools pinned in xtask/tools.toml into .tools/, verifying each download.
    Setup,
    /// Check that every pinned tool is installed at its pinned version.
    Doctor,
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
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(error) => match writeln!(io::stderr().lock(), "error: {error:#}") {
            // With stderr gone there is nowhere left to report; the exit code still fails.
            Ok(()) | Err(_) => ExitCode::FAILURE,
        },
    }
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
        Task::Setup => {
            tools::setup(&root, &mut io::stdout().lock())?;
            Ok(ExitCode::SUCCESS)
        }
        Task::Doctor => tools::doctor(&root, &mut io::stdout().lock()),
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

    use super::{Cli, repo_root};

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
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
