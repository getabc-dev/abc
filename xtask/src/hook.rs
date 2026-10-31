//! Claude Code hooks: find the checkout the agent is working in, then act there.
//!
//! Claude Code starts every hook from the directory the session began in, but the agent may have
//! moved since, for example into a worktree. The hook input's `cwd` says where it is now; without
//! one, the session's project directory stands in. The hook resolves that directory's repository
//! root and then:
//!
//! - runs here when the root is this checkout;
//! - hands the input to that checkout's own `cargo xtask hook` when it is another worktree of this
//!   repository, so the checkout is held to its own toolchain, tools and gate;
//! - does nothing when the directory is in no repository, or in a different one.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;

use crate::{check, guard, tools};

/// Marks a hook handed over by another checkout, which must not be handed on again.
const DELEGATED: &str = "XTASK_HOOK_DELEGATED";

/// The Claude Code hook events xtask handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Event {
    /// `PreToolUse` on Bash: block git commands that skip hooks or force-push.
    PreToolUse,
    /// `Stop` and `SubagentStop`: run `cargo xtask check --changed`.
    Stop,
}

impl Event {
    const fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre-tool-use",
            Self::Stop => "stop",
        }
    }
}

/// What a hook knows about where it runs.
#[derive(Debug)]
pub(crate) struct Context {
    /// The root of the checkout this xtask was built from.
    own_root: PathBuf,
    /// `$CLAUDE_PROJECT_DIR`: the directory the session started in.
    project_dir: Option<PathBuf>,
    /// Whether another checkout's xtask handed this hook over.
    delegated: bool,
}

impl Context {
    pub(crate) fn from_env(own_root: PathBuf) -> Self {
        Self {
            own_root,
            project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
            delegated: std::env::var_os(DELEGATED).is_some(),
        }
    }
}

/// The fields of the hook input this module reads.
#[derive(Deserialize)]
struct HookInput {
    cwd: Option<PathBuf>,
}

/// Where the agent is working, relative to this checkout.
#[derive(Debug, PartialEq, Eq)]
enum Location {
    Here,
    /// Another worktree of this repository, by its root.
    Worktree(PathBuf),
    /// Outside this repository, for the given reason.
    Elsewhere(String),
}

/// Runs the hook for `event` on the hook input JSON and returns the exit code for Claude Code.
/// Messages for the agent go to `out`.
pub(crate) fn run(
    event: Event,
    input: &str,
    context: &Context,
    out: &mut impl Write,
) -> anyhow::Result<u8> {
    let cwd = match (event, serde_json::from_str::<HookInput>(input)) {
        (_, Ok(fields)) => fields.cwd,
        // The guard blocks input it cannot read and gives the parse error as the reason.
        (Event::PreToolUse, Err(_)) => return guard(input, out),
        (Event::Stop, Err(error)) => {
            return Err(error).context("could not read the Stop hook input");
        }
    };
    let location = locate(
        cwd.as_deref(),
        context.project_dir.as_deref(),
        &context.own_root,
    )?;
    match location {
        Location::Here => match event {
            Event::PreToolUse => guard(input, out),
            // Exit 1 when the check fails; the hook command turns any failure into a block.
            Event::Stop => Ok(u8::from(!check::run(&context.own_root, true, out)?)),
        },
        Location::Worktree(root) => {
            ensure!(
                !context.delegated,
                "{} was handed this hook, but the agent is in another checkout: {}",
                context.own_root.display(),
                root.display()
            );
            delegate(&root, event, input)
        }
        Location::Elsewhere(reason) => {
            if event == Event::Stop {
                writeln!(out, "xtask hook: {reason}; no checks run")?;
            }
            Ok(0)
        }
    }
}

fn guard(input: &str, out: &mut impl Write) -> anyhow::Result<u8> {
    match guard::decide(input) {
        guard::Decision::Allow => Ok(0),
        guard::Decision::Block(message) => {
            writeln!(out, "{message}")?;
            Ok(2)
        }
    }
}

/// Finds the checkout containing `cwd`, or the project directory when there is no `cwd`.
fn locate(
    cwd: Option<&Path>,
    project_dir: Option<&Path>,
    own_root: &Path,
) -> anyhow::Result<Location> {
    let Some(dir) = cwd.or(project_dir) else {
        bail!("the hook input has no cwd and CLAUDE_PROJECT_DIR is not set");
    };
    let Some(root) = git_path(dir, &["rev-parse", "--show-toplevel"])? else {
        return Ok(Location::Elsewhere(format!(
            "{} is not in a git repository",
            dir.display()
        )));
    };
    let own_root = own_root
        .canonicalize()
        .with_context(|| format!("could not resolve {}", own_root.display()))?;
    if root == own_root {
        return Ok(Location::Here);
    }
    let common_dir = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    if git_path(&root, &common_dir)? == git_path(&own_root, &common_dir)? {
        Ok(Location::Worktree(root))
    } else {
        Ok(Location::Elsewhere(format!(
            "{} is not a checkout of this repository",
            root.display()
        )))
    }
}

/// Runs git in `dir` and resolves the path it prints. `None` when git refuses, as it does outside a
/// repository or in a directory that no longer exists.
fn git_path(dir: &Path, args: &[&str]) -> anyhow::Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Resolve from the directory alone, never from an inherited repository override.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let printed = String::from_utf8(output.stdout).context("git printed a non-UTF-8 path")?;
    let path = Path::new(printed.trim_end_matches(['\n', '\r']));
    path.canonicalize()
        .map(Some)
        .with_context(|| format!("could not resolve {}", path.display()))
}

/// Hands the hook to the checkout at `root`, which runs it with its own xtask and toolchain.
fn delegate(root: &Path, event: Event, input: &str) -> anyhow::Result<u8> {
    let mut child = Command::new("cargo")
        .args(["xtask", "hook", event.name()])
        .current_dir(root)
        // Let the checkout's rust-toolchain.toml choose the toolchain, not the one running here.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env(DELEGATED, OsStr::new("1"))
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run cargo xtask in {}", root.display()))?;
    child
        .stdin
        .take()
        .context("the delegated hook has no stdin")?
        .write_all(input.as_bytes())
        .context("could not pass the hook input on")?;
    let status = child
        .wait()
        .context("could not wait for the delegated hook")?;
    Ok(tools::status_code(status))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{Context, Event, Location, locate, run};
    use crate::testing::{git, repository};

    /// A repository with a worktree nested inside it, where Claude Code puts its worktrees.
    fn repository_with_nested_worktree() -> (TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let main = repository(&temp.path().join("abc"));
        git(
            &main,
            &["worktree", "add", "--quiet", ".claude/worktrees/feature"],
        );
        let worktree = main.join(".claude/worktrees/feature");
        fs::create_dir_all(worktree.join("crates/deep")).unwrap();
        (temp, main, worktree.canonicalize().unwrap())
    }

    fn context(own_root: &Path, project_dir: Option<&Path>) -> Context {
        Context {
            own_root: own_root.to_path_buf(),
            project_dir: project_dir.map(Path::to_path_buf),
            delegated: false,
        }
    }

    fn input(cwd: Option<&Path>, command: &str) -> String {
        let mut fields = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
        });
        if let Some(cwd) = cwd {
            fields["cwd"] = serde_json::Value::from(cwd.to_str().unwrap());
        }
        fields.to_string()
    }

    #[test]
    fn a_nested_worktree_is_located_by_its_own_root() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let deep = worktree.join("crates/deep");
        assert_eq!(
            locate(Some(&deep), Some(&main), &main).unwrap(),
            Location::Worktree(worktree.clone()),
            "from the main checkout, the worktree is another checkout of the same repository"
        );
        assert_eq!(
            locate(Some(&deep), Some(&main), &worktree).unwrap(),
            Location::Here,
            "the worktree's own xtask runs the hook itself"
        );
        assert_eq!(
            locate(Some(&main.join(".claude")), None, &main).unwrap(),
            Location::Here,
            "a directory of the main checkout outside the worktree belongs to the main checkout"
        );
    }

    #[test]
    fn an_absent_cwd_falls_back_to_the_project_directory() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        assert_eq!(
            locate(None, Some(&main), &main).unwrap(),
            Location::Here,
            "the project directory stands in for cwd"
        );
        assert_eq!(
            locate(None, Some(&worktree), &main).unwrap(),
            Location::Worktree(worktree.clone()),
            "a project directory in a worktree hands the hook over"
        );
        let error = locate(None, None, &main).unwrap_err().to_string();
        assert!(error.contains("CLAUDE_PROJECT_DIR"), "{error}");
    }

    #[test]
    fn a_cwd_takes_precedence_over_the_project_directory() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        assert_eq!(
            locate(Some(&worktree), Some(&main), &main).unwrap(),
            Location::Worktree(worktree),
            "the agent's cwd wins"
        );
    }

    #[test]
    fn the_guard_reads_the_project_directory_when_cwd_is_absent() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            &input(None, "git push --force"),
            &context(&main, Some(&main)),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 2, "a force push in the project directory is blocked");
        assert!(
            String::from_utf8(out).unwrap().contains("force-pushes"),
            "the agent is told why"
        );
    }

    #[test]
    fn outside_any_repository_the_guard_allows_without_checks() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let outside = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            &input(Some(outside.path()), "git push --force"),
            &context(&main, Some(&main)),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0, "nothing is checked outside the repository");
        assert!(out.is_empty(), "and nothing is said");
    }

    #[test]
    fn outside_any_repository_stop_passes_with_a_note() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let outside = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = run(
            Event::Stop,
            &input(Some(outside.path()), ""),
            &context(&main, Some(&main)),
            &mut out,
        )
        .unwrap();
        let note = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "the agent may stop");
        assert_eq!(note.lines().count(), 1, "one line: {note:?}");
        assert!(
            note.contains("is not in a git repository; no checks run"),
            "{note:?}"
        );
    }

    #[test]
    fn another_repository_is_not_checked() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let other = repository(&temp.path().join("other"));
        assert_eq!(
            locate(Some(&other), Some(&main), &main).unwrap(),
            Location::Elsewhere(format!(
                "{} is not a checkout of this repository",
                other.display()
            )),
            "another repository's xtask is never run"
        );
    }

    #[test]
    fn a_handed_over_hook_is_not_handed_on() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let mut handed_over = context(&main, Some(&main));
        handed_over.delegated = true;
        let error = run(
            Event::PreToolUse,
            &input(Some(&worktree), "git status"),
            &handed_over,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("another checkout"), "{error}");
    }

    #[test]
    fn unreadable_input_blocks_the_guard_and_fails_stop() {
        let (_temp, main, _worktree) = repository_with_nested_worktree();
        let mut out = Vec::new();
        let code = run(
            Event::PreToolUse,
            "not json",
            &context(&main, Some(&main)),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 2, "the guard blocks input it cannot read");
        let error = run(
            Event::Stop,
            "not json",
            &context(&main, Some(&main)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("could not read the Stop hook input"),
            "{error:#}"
        );
    }
}
