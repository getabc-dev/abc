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
//!
//! Hooks run outside Claude Code's sandbox, so `.claude/settings.json` never runs this code
//! directly: it runs it through `.claude/hooks/jail.sh`, which holds it to the sandbox's limits
//! (no credentials, writes only to the session's checkout and a temporary directory, no network).
//! A hook can therefore act only in a worktree inside that checkout, such as Claude Code's own in
//! `.claude/worktrees/`. The stop hook's watchdog needs the user's claude login, so
//! `.claude/hooks/stop.sh` runs claude outside the jail on the prompt this hook writes.
//!
//! Claude Code cancels a hook at its `timeout` in `.claude/settings.json`: 300 seconds for
//! `PreToolUse` and 900 for `Stop`. A timed-out hook renders no decision, so the tool call runs or
//! the agent stops. The guard builds in `target/hook`, so it never waits on the agent's cargo lock.
//!
//! The `PreToolUse` guard catches accidents; it is not the enforcement. GitHub rulesets on `main`
//! (no force push, no deletion, required checks) and the abc-agent App's limited permissions are.
//!
//! When the `PreToolUse` guard cannot run, it blocks commands that mention git or gh and lets the
//! rest through, so a broken build never locks the agent out of the shell. When xtask cannot even
//! start, `xtask/hooks/pre-tool-use.sh` applies the same rule.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, bail, ensure};
use serde::Deserialize;

use crate::{check, guard, tools, watchdog};

/// The `PreToolUse` hook script. A checkout without it predates these hooks, so its xtask cannot
/// take a hook over.
const HOOK_SCRIPT: &str = "xtask/hooks/pre-tool-use.sh";

/// Marks a hook handed over by another checkout, which must not be handed on again.
const DELEGATED: &str = "XTASK_HOOK_DELEGATED";

/// The Claude Code hook events xtask handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Event {
    /// `PreToolUse` on Bash and Monitor: block force pushes and git or gh commands the guard
    /// cannot read.
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
    /// The cargo that runs another checkout's xtask.
    cargo: PathBuf,
    /// Where the stop hook writes the watchdog's prompt, when `.claude/hooks/stop.sh` asks for one.
    watchdog_prompt: Option<PathBuf>,
    /// Whether this hook runs inside the watchdog's own claude session, where it must do nothing.
    inside_watchdog: bool,
}

impl Context {
    pub(crate) fn from_env(own_root: PathBuf, watchdog_prompt: Option<PathBuf>) -> Self {
        Self {
            own_root,
            project_dir: std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from),
            delegated: std::env::var_os(DELEGATED).is_some(),
            cargo: PathBuf::from("cargo"),
            watchdog_prompt,
            inside_watchdog: std::env::var_os(watchdog::GUARD).is_some(),
        }
    }
}

/// The fields of the hook input this module reads.
#[derive(Deserialize)]
struct HookInput {
    cwd: Option<PathBuf>,
    /// `Stop` or `SubagentStop` for the stop hook.
    hook_event_name: Option<String>,
    /// Whether the agent is already working on because a stop hook blocked it.
    #[serde(default)]
    stop_hook_active: bool,
}

impl HookInput {
    /// Whether the watchdog judges this stop: the main agent's own, and not one a stop hook has
    /// already sent back to work, so the watchdog can never keep an agent going forever.
    fn watched(&self) -> bool {
        self.hook_event_name.as_deref() == Some("Stop") && !self.stop_hook_active
    }
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
    match event {
        Event::PreToolUse => match act(event, input, context, out) {
            Ok(code @ (0 | 2)) => Ok(code),
            Ok(code) => guard_unavailable(&format!("it exited {code}"), input, out),
            Err(error) => guard_unavailable(&format!("{error:#}"), input, out),
        },
        // The watchdog's own session stops without a check, so it never starts another watchdog.
        Event::Stop if context.inside_watchdog => Ok(0),
        Event::Stop => act(event, input, context, out),
    }
}

/// When the guard cannot run, blocks a command that mentions git or gh and lets any other through
/// with a warning.
fn guard_unavailable(reason: &str, input: &str, out: &mut impl Write) -> anyhow::Result<u8> {
    let command = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(fields) => fields
            .pointer("/tool_input/command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(_) => Some(String::from("git")),
    };
    if command.as_deref().is_some_and(guard::mentions_git_or_gh) {
        writeln!(
            out,
            "Blocked: the git guard could not run ({reason}), so git and gh commands are blocked \
             until it can. Commands that do not mention git or gh still run."
        )?;
        Ok(2)
    } else {
        writeln!(
            out,
            "warning: the git guard could not run ({reason}); allowing a command that does not \
             mention git or gh"
        )?;
        Ok(0)
    }
}

/// Finds where the agent works and runs the hook there.
fn act(event: Event, input: &str, context: &Context, out: &mut impl Write) -> anyhow::Result<u8> {
    let fields = match (event, serde_json::from_str::<HookInput>(input)) {
        (_, Ok(fields)) => fields,
        // The guard blocks input it cannot read and gives the parse error as the reason.
        (Event::PreToolUse, Err(_)) => return guard(input, out),
        (Event::Stop, Err(error)) => {
            return Err(error).context("could not read the Stop hook input");
        }
    };
    let location = locate(
        fields.cwd.as_deref(),
        context.project_dir.as_deref(),
        &context.own_root,
    )?;
    match location {
        Location::Here => match event {
            Event::PreToolUse => guard(input, out),
            // Exit 1 when the check fails; the hook command turns any failure into a block.
            Event::Stop => {
                let passed = check::run(
                    &context.own_root,
                    true,
                    &tools::pinned(&context.own_root, "cargo-nextest")?,
                    None,
                    out,
                )?;
                finish_stop(passed, &fields, context, out)
            }
        },
        // GitHub is the gate, so a checkout that cannot take the hook is noted, never locked out.
        Location::Worktree(root) if !root.join(HOOK_SCRIPT).is_file() => {
            writeln!(
                out,
                "xtask hook: {} has no {HOOK_SCRIPT}; nothing is checked there",
                root.display()
            )?;
            Ok(0)
        }
        Location::Worktree(root) => {
            ensure!(
                !context.delegated,
                "{} was handed this hook, but the agent is in another checkout: {}",
                context.own_root.display(),
                root.display()
            );
            delegate(
                &context.cargo,
                &root,
                event,
                context.watchdog_prompt.as_deref(),
                input,
            )
        }
        Location::Elsewhere(reason) => {
            if event == Event::Stop {
                writeln!(out, "xtask hook: {reason}; no checks run")?;
            }
            Ok(0)
        }
    }
}

/// The stop hook's exit code once the check has run: 1 when it failed. Otherwise 0, after writing
/// the watchdog's prompt for the main agent's stop when `.claude/hooks/stop.sh` asked for one.
fn finish_stop(
    passed: bool,
    fields: &HookInput,
    context: &Context,
    out: &mut impl Write,
) -> anyhow::Result<u8> {
    if !passed {
        return Ok(1);
    }
    if let (true, Some(file)) = (fields.watched(), &context.watchdog_prompt) {
        watchdog::prepare(&context.own_root, file, out)?;
    }
    Ok(0)
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
    ensure!(
        dir.is_dir(),
        "the agent's working directory {} does not exist",
        dir.display()
    );
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
/// repository.
fn git_path(dir: &Path, args: &[&str]) -> anyhow::Result<Option<PathBuf>> {
    let output = git_in(dir)
        .args(args)
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

/// Git run in `dir`, resolving the repository from the directory alone, never from an inherited
/// `GIT_DIR` or `GIT_WORK_TREE`.
fn git_in(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    command
}

/// Hands the hook to the checkout at `root`, which runs it with its own xtask and toolchain, and
/// writes any watchdog prompt to `watchdog_prompt`.
fn delegate(
    cargo: &Path,
    root: &Path,
    event: Event,
    watchdog_prompt: Option<&Path>,
    input: &str,
) -> anyhow::Result<u8> {
    let mut child = Command::new(cargo)
        .args(["xtask", "hook", event.name()])
        .args(
            watchdog_prompt
                .into_iter()
                .flat_map(|file| [OsStr::new("--watchdog-prompt"), file.as_os_str()]),
        )
        .current_dir(root)
        // Let the checkout's rust-toolchain.toml choose the toolchain, not the one running here.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env(DELEGATED, OsStr::new("1"))
        .envs(
            (event == Event::PreToolUse)
                .then(|| ("CARGO_TARGET_DIR", root.join("target/hook"))),
        )
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run cargo xtask in {}", root.display()))?;
    feed(&mut child, input).context("could not pass the hook input on")?;
    let status = child
        .wait()
        .context("could not wait for the delegated hook")?;
    Ok(tools::status_code(status))
}

/// Writes `input` to `child`'s stdin and closes it. A child that exits without reading its input,
/// such as a hook that only prints a note or a checkout whose xtask fails to build, closes the
/// pipe first; its exit status then decides the outcome, so a broken pipe is not an error.
fn feed(child: &mut Child, input: &str) -> std::io::Result<()> {
    let Some(mut stdin) = child.stdin.take() else {
        return Ok(());
    };
    match stdin.write_all(input.as_bytes()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        written => written,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use tempfile::TempDir;

    use super::{Context, Event, Location, git_in, guard_unavailable, locate, run};
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
        fs::create_dir_all(worktree.join("xtask/hooks")).unwrap();
        fs::write(worktree.join(super::HOOK_SCRIPT), "").unwrap();
        (temp, main, worktree.canonicalize().unwrap())
    }

    fn context(own_root: &Path, project_dir: Option<&Path>) -> Context {
        Context {
            own_root: own_root.to_path_buf(),
            project_dir: project_dir.map(Path::to_path_buf),
            delegated: false,
            cargo: PathBuf::from("cargo"),
            watchdog_prompt: None,
            inside_watchdog: false,
        }
    }

    /// A script at `dir/cargo` that drains its input, records `CARGO_TARGET_DIR` in
    /// `dir/target-dir`, and exits with `code`.
    fn fake_cargo(dir: &Path, code: u8) -> PathBuf {
        let path = dir.join("cargo");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf %s \"$CARGO_TARGET_DIR\" > \"$(dirname \"$0\")/target-dir\"\nexit {code}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run_pre_tool_use(context: &Context, cwd: Option<&Path>, command: &str) -> (u8, String) {
        let mut out = Vec::new();
        let code = run(Event::PreToolUse, &input(cwd, command), context, &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
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
    fn a_missing_cwd_blocks_git_and_allows_the_rest() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let gone = temp.path().join("removed-worktree");
        let context = context(&main, Some(&main));

        let (push_code, push_out) = run_pre_tool_use(&context, Some(&gone), "git push origin main");
        assert_eq!(
            push_code, 2,
            "git is blocked when the guard cannot locate the agent: {push_out}"
        );
        assert!(
            push_out.contains("does not exist"),
            "the reason is given: {push_out}"
        );

        let (listing_code, listing_out) = run_pre_tool_use(&context, Some(&gone), "ls -la");
        assert_eq!(listing_code, 0, "other commands still run: {listing_out}");
        assert!(
            listing_out.starts_with("warning: "),
            "with a warning: {listing_out}"
        );

        let error = run(
            Event::Stop,
            &input(Some(&gone), ""),
            &context,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("does not exist"),
            "stop fails: {error:#}"
        );
    }

    #[test]
    fn a_worktree_without_xtask_hooks_is_noted_not_checked() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        fs::remove_file(worktree.join(super::HOOK_SCRIPT)).unwrap();
        let context = context(&main, Some(&main));
        let (code, out) = run_pre_tool_use(&context, Some(&worktree), "git push -f");
        assert_eq!(code, 0, "an older checkout is not locked out: {out}");
        assert_eq!(out.lines().count(), 1, "one line: {out}");
        assert!(out.contains("nothing is checked there"), "{out}");
        let mut stop_out = Vec::new();
        let stop = run(
            Event::Stop,
            &input(Some(&worktree), ""),
            &context,
            &mut stop_out,
        )
        .unwrap();
        assert_eq!(stop, 0, "the agent may stop");
        assert!(
            String::from_utf8(stop_out)
                .unwrap()
                .contains("nothing is checked there"),
            "with a note"
        );
    }

    #[test]
    fn a_worktree_whose_xtask_fails_blocks_git() {
        let (temp, main, worktree) = repository_with_nested_worktree();
        let mut broken = context(&main, Some(&main));
        broken.cargo = fake_cargo(temp.path(), 101);

        let (merge_code, merge_out) = run_pre_tool_use(&broken, Some(&worktree), "gh pr merge 1");
        assert_eq!(merge_code, 2, "{merge_out}");
        assert!(merge_out.contains("it exited 101"), "{merge_out}");
        let (build_code, build_out) = run_pre_tool_use(&broken, Some(&worktree), "cargo build");
        assert_eq!(build_code, 0, "{build_out}");

        broken.cargo = fake_cargo(temp.path(), 2);
        assert_eq!(
            run_pre_tool_use(&broken, Some(&worktree), "git push -f").0,
            2,
            "the worktree's own block passes through"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("target-dir")).unwrap(),
            worktree.join("target/hook").display().to_string(),
            "the worktree's guard builds apart from its cargo lock"
        );
    }

    #[test]
    fn feeding_a_child_that_exits_without_reading_is_not_an_error() {
        // A megabyte overflows any pipe buffer, so the write meets the closed pipe for certain.
        let input = "x".repeat(1 << 20);
        let mut child = Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        super::feed(&mut child, &input).unwrap();
        assert_eq!(
            child.wait().unwrap().code(),
            Some(3),
            "the child's exit status decides, not the broken pipe"
        );
    }

    /// The shells hooks must work under: the system `sh` and dash, Debian's and Ubuntu's `sh`.
    const SHELLS: [&str; 2] = ["sh", "/bin/dash"];

    /// Runs `args` under `shell` with `input` on stdin, the project directory `project` and
    /// `cargo` from `bin` first on `PATH`; returns the exit code and stderr.
    fn run_shell(
        shell: &str,
        args: &[&std::ffi::OsStr],
        project: &Path,
        bin: &Path,
        input: &str,
    ) -> (Option<i32>, String) {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let mut child = Command::new(shell)
            .args(args)
            .env("PATH", path)
            .env("CLAUDE_PROJECT_DIR", project)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("{shell} must be installed: {error}"));
        super::feed(&mut child, input).unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Runs the hook script under each shell, with `cargo` on `PATH` exiting `code`, and checks
    /// that every shell agrees; returns the exit code and the first shell's stderr.
    fn run_hook_script(code: u8, input: &str) -> (Option<i32>, String) {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), code);
        let script = crate::repo_root().unwrap().join(super::HOOK_SCRIPT);
        let results: Vec<(Option<i32>, String)> = SHELLS
            .iter()
            .map(|shell| {
                run_shell(
                    shell,
                    &[script.as_os_str()],
                    temp.path(),
                    temp.path(),
                    input,
                )
            })
            .collect();
        assert!(
            results.windows(2).all(|pair| pair[0].0 == pair[1].0),
            "the shells disagree: {results:?}"
        );
        results.into_iter().next().unwrap()
    }

    /// The command of each hook in `.claude/settings.json`, by event.
    fn hook_commands() -> Vec<(String, String)> {
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        settings["hooks"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(event, groups)| {
                groups.as_array().unwrap().iter().flat_map(move |group| {
                    group["hooks"].as_array().unwrap().iter().map(move |hook| {
                        (event.clone(), hook["command"].as_str().unwrap().to_owned())
                    })
                })
            })
            .collect()
    }

    #[test]
    fn hook_commands_note_a_checkout_without_xtask_and_let_it_be() {
        let empty = tempfile::tempdir().unwrap();
        let commands = hook_commands();
        assert_eq!(
            commands.len(),
            3,
            "PreToolUse, Stop and SubagentStop: {commands:?}"
        );
        for (event, command) in commands {
            for shell in SHELLS {
                let (code, stderr) = run_shell(
                    shell,
                    &["-c".as_ref(), command.as_ref()],
                    empty.path(),
                    empty.path(),
                    r#"{"tool_name":"Bash","tool_input":{"command":"git push -f"}}"#,
                );
                assert_eq!(code, Some(0), "{event} under {shell}: {stderr}");
                assert_eq!(stderr.lines().count(), 1, "{event} under {shell}: {stderr}");
                assert!(stderr.starts_with("xtask hook: "), "{event}: {stderr}");
            }
        }
    }

    #[test]
    fn locating_ignores_an_inherited_repository_override() {
        let git = git_in(Path::new("/"));
        let removed: Vec<_> = git
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_owned())
            .collect();
        assert_eq!(
            removed,
            ["GIT_DIR", "GIT_WORK_TREE"],
            "a hook run inside another git command must not inherit its repository"
        );
    }

    #[test]
    fn unparseable_input_blocks_when_the_guard_cannot_run() {
        let mut out = Vec::new();
        let code = guard_unavailable("the build failed", "not json", &mut out).unwrap();
        assert_eq!(code, 2, "no command can be read, so it may be git");
        assert!(
            String::from_utf8(out).unwrap().starts_with("Blocked: "),
            "and the agent is told"
        );
    }

    #[test]
    fn only_the_agent_environment_asks_the_app_for_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".gitconfig"),
            "[credential]\n\thelper = \"!f() { echo username=user; echo password=user-secret; }; f\"\n",
        )
        .unwrap();
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let fill = |agent: bool| {
            let mut command = Command::new("git");
            command
                .args(["credential", "fill"])
                .current_dir(&repo)
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_CONFIG_COUNT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if agent {
                for (name, value) in settings["env"].as_object().unwrap() {
                    command.env(name, value.as_str().unwrap());
                }
                // The App's helper itself, which needs the App's files, stands in as a fake.
                assert_eq!(
                    settings["env"]["GIT_CONFIG_VALUE_1"], "!cargo xtask git-credential",
                    "the real helper"
                );
                command.env(
                    "GIT_CONFIG_VALUE_1",
                    "!f() { echo username=x-access-token; echo password=app-token; }; f",
                );
            }
            let mut child = command.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"protocol=https\nhost=github.com\n\n")
                .unwrap();
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
        };
        assert!(
            fill(false).contains("password=user-secret"),
            "the user's terminal keeps the user's credentials"
        );
        let agent = fill(true);
        assert!(
            agent.contains("password=app-token"),
            "agents get the App's: {agent}"
        );
        assert!(
            !agent.contains("user-secret"),
            "and never the user's: {agent}"
        );
    }

    #[test]
    fn settings_wire_the_hooks_to_xtask() {
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        let hooks = &settings["hooks"];
        assert_eq!(
            hooks["PreToolUse"][0]["matcher"], "Bash|Monitor",
            "both tools that run shell commands are guarded"
        );
        let guard = &hooks["PreToolUse"][0]["hooks"][0];
        assert_eq!(
            guard["command"],
            "if [ -f \"$CLAUDE_PROJECT_DIR/.claude/hooks/jail.sh\" ] && [ -f \"$CLAUDE_PROJECT_DIR/xtask/hooks/pre-tool-use.sh\" ]; then cd \"$CLAUDE_PROJECT_DIR\" && sh .claude/hooks/jail.sh sh xtask/hooks/pre-tool-use.sh; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no .claude/hooks/jail.sh or xtask/hooks/pre-tool-use.sh; the git guard is off\" >&2; fi",
            "the guard runs the hook script in the jail"
        );
        assert_eq!(guard["timeout"], 300, "the guard is time-boxed");
        let stop_command = "if [ -f \"$CLAUDE_PROJECT_DIR/.claude/hooks/stop.sh\" ] && [ -f \"$CLAUDE_PROJECT_DIR/xtask/Cargo.toml\" ]; then sh \"$CLAUDE_PROJECT_DIR/.claude/hooks/stop.sh\"; else echo \"xtask hook: $CLAUDE_PROJECT_DIR has no .claude/hooks/stop.sh or xtask; no checks run\" >&2; fi";
        for event in ["Stop", "SubagentStop"] {
            let stop = &hooks[event][0]["hooks"][0];
            assert_eq!(stop["command"], stop_command, "{event} runs the check");
            assert_eq!(stop["timeout"], 900, "{event} is time-boxed");
        }
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!([
                "Bash(gh auth token)",
                "Bash(gh auth token *)",
                "Bash(gh auth status --show-token)",
                "Bash(gh auth status --show-token *)",
                "Bash(gh auth status * --show-token*)",
                "Bash(gh auth status -t)",
                "Bash(gh auth status -t *)",
                "Bash(gh auth status * -t*)",
                "Read(~/.config/gh/**)",
                "Read(~/.config/abc/agent-app/**)",
                "Bash(security find-generic-password *)",
                "Bash(security find-internet-password *)"
            ]),
            "agents cannot read the user's GitHub credentials (D3)"
        );
        assert_eq!(
            settings["env"],
            serde_json::json!({
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": "credential.https://github.com.helper",
                "GIT_CONFIG_VALUE_0": "",
                "GIT_CONFIG_KEY_1": "credential.https://github.com.helper",
                "GIT_CONFIG_VALUE_1": "!cargo xtask git-credential"
            }),
            "agent sessions alone push as the App: the empty helper drops the user's (D3)"
        );
        assert_eq!(
            hooks["SubagentStop"][0]["matcher"],
            "^(?!(?:Explore|Plan|claude-code-guide|statusline-setup|web-fetch)$).+",
            "read-only and internal subagents are exempt"
        );
    }

    #[test]
    fn the_hook_script_blocks_git_when_xtask_cannot_run() {
        let git_push = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"git push origin main"}}"#;
        let (code, stderr) = run_hook_script(101, git_push);
        assert_eq!(code, Some(2), "{stderr}");
        assert!(stderr.contains("could not run"), "{stderr}");
        assert_eq!(
            run_hook_script(1, git_push).0,
            Some(2),
            "exit 1 is a failure to run the guard, not its verdict"
        );

        let quoted =
            r#"{"tool_name":"Bash","tool_input":{"command":"echo \"x\" && gh api repos/o/r"}}"#;
        assert_eq!(run_hook_script(101, quoted).0, Some(2), "escaped quotes");
    }

    #[test]
    fn the_hook_script_lets_other_commands_through_when_xtask_cannot_run() {
        let listing = r#"{"cwd":"/home/me/git/abc","tool_name":"Bash","tool_input":{"command":"ls .github .gitignore"}}"#;
        let (code, stderr) = run_hook_script(101, listing);
        assert_eq!(
            code,
            Some(0),
            "a git path in cwd is not a git command: {stderr}"
        );
        assert!(stderr.starts_with("warning: "), "{stderr}");

        let socket = r#"{"tool_name":"Monitor","tool_input":{"ws":{"url":"wss://example.com"}}}"#;
        assert_eq!(run_hook_script(101, socket).0, Some(0), "no command at all");
    }

    #[test]
    fn the_hook_script_builds_xtask_apart_from_the_agents_builds() {
        let temp = tempfile::tempdir().unwrap();
        fake_cargo(temp.path(), 0);
        let script = crate::repo_root().unwrap().join(super::HOOK_SCRIPT);
        run_shell("sh", &[script.as_os_str()], temp.path(), temp.path(), "{}");
        assert_eq!(
            fs::read_to_string(temp.path().join("target-dir")).unwrap(),
            temp.path().join("target/hook").display().to_string(),
            "the guard never waits on the lock of target/"
        );
    }

    #[test]
    fn the_hook_script_passes_on_the_guards_verdict() {
        let input = r#"{"tool_name":"Bash","tool_input":{"command":"git push -f"}}"#;
        assert_eq!(run_hook_script(2, input).0, Some(2), "a block");
        assert_eq!(run_hook_script(0, input).0, Some(0), "an allow");
    }

    #[test]
    fn the_watchdog_judges_only_the_main_agents_first_stop() {
        let fields = |json: &str| serde_json::from_str::<super::HookInput>(json).unwrap();
        assert!(
            fields(r#"{"hook_event_name": "Stop", "stop_hook_active": false}"#).watched(),
            "the main agent stopping"
        );
        assert!(
            !fields(r#"{"hook_event_name": "Stop", "stop_hook_active": true}"#).watched(),
            "an agent a stop hook already sent back is never blocked again"
        );
        assert!(
            !fields(r#"{"hook_event_name": "SubagentStop"}"#).watched(),
            "subagents have no task of their own"
        );
        assert!(!fields("{}").watched(), "an unnamed event is not watched");
    }

    #[test]
    fn the_watchdogs_own_session_stops_without_checks() {
        let (temp, main, _worktree) = repository_with_nested_worktree();
        let mut inside = context(&main, Some(&main));
        inside.inside_watchdog = true;
        inside.cargo = fake_cargo(temp.path(), 101);
        let mut out = Vec::new();
        assert_eq!(
            run(Event::Stop, "not json", &inside, &mut out).unwrap(),
            0,
            "no check, no watchdog, no recursion"
        );
        assert!(out.is_empty(), "and nothing to say");
    }

    #[test]
    fn a_handed_over_hook_is_not_handed_on() {
        let (_temp, main, worktree) = repository_with_nested_worktree();
        let mut handed_over = context(&main, Some(&main));
        handed_over.delegated = true;
        let error = run(
            Event::Stop,
            &input(Some(&worktree), ""),
            &handed_over,
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("another checkout"), "{error}");
        let (code, out) = run_pre_tool_use(&handed_over, Some(&worktree), "git status");
        assert_eq!(code, 2, "the guard fails closed for git: {out}");
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

    /// The project's Claude Code settings.
    fn settings() -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(crate::repo_root().unwrap().join(".claude/settings.json")).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn settings_sandbox_every_agent_command() {
        let settings = settings();
        let sandbox = &settings["sandbox"];
        assert_eq!(sandbox["enabled"], true, "agents' commands run sandboxed");
        assert_eq!(
            sandbox["failIfUnavailable"], true,
            "a session without the sandbox does not start"
        );
        assert_eq!(
            sandbox["allowUnsandboxedCommands"], false,
            "no escape hatch: the model cannot rerun a command outside the sandbox"
        );
        assert_eq!(
            sandbox["enableWeakerNetworkIsolation"], true,
            "gh, a Go program, verifies TLS through macOS's trustd; without it `cargo xtask gh` \
             fails, and a sandboxed command can already send anything to GitHub"
        );
        for key in [
            "excludedCommands",
            "autoAllowBashIfSandboxed",
            "enableWeakerNestedSandbox",
        ] {
            assert!(sandbox.get(key).is_none(), "{key} stays at its default");
        }
        for key in [
            "allowAllUnixSockets",
            "allowLocalBinding",
            "allowMachLookup",
        ] {
            assert!(
                sandbox["network"].get(key).is_none(),
                "{key} would open local services, the Docker socket or the keychain"
            );
        }
        assert_eq!(
            sandbox["filesystem"],
            serde_json::json!({"denyRead": [
                "~/.config/gh",
                format!("~/{}", crate::app::CONFIG_DIR),
                "~/Library/Keychains",
                "~/.ssh"
            ]}),
            "the user's gh login, the App's key, the keychains gh and git keep tokens in, and SSH \
             keys are unreadable; writes stay at Claude Code's default, the checkout"
        );
        assert_eq!(
            sandbox["network"]["allowedDomains"],
            serde_json::json!([
                "github.com",
                "api.github.com",
                "objects.githubusercontent.com",
                "release-assets.githubusercontent.com",
                "crates.io",
                "index.crates.io",
                "static.crates.io",
                "static.rust-lang.org",
                "api.osv.dev"
            ]),
            "GitHub, crates, the toolchain and the pinned tools' downloads, nothing else"
        );
        assert_eq!(
            settings["permissions"]["ask"],
            serde_json::json!(["Edit(/.claude/**)"]),
            "the user approves every edit to the settings and the jail"
        );
    }

    /// The trusted hook scripts in `.claude/hooks/`.
    fn trusted_script(name: &str) -> PathBuf {
        crate::repo_root().unwrap().join(".claude/hooks").join(name)
    }

    /// Whether this process may apply a Seatbelt profile: false on other systems and inside a
    /// sandbox, such as Claude Code's or the jail itself.
    fn can_apply_seatbelt() -> bool {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                "(version 1)(deny default)(allow process-exec)(allow file-read*)",
            ])
            .arg("/usr/bin/true")
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn the_jail_confines_repository_code_or_refuses_to_run_it() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        for secret in [
            ".config/gh/hosts.yml",
            ".config/abc/agent-app/private-key.pem",
            "Library/Keychains/login.keychain-db",
            ".ssh/id_ed25519",
        ] {
            let path = home.join(secret);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "secret").unwrap();
        }
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".claude/worktrees/feature/.claude")).unwrap();
        fs::create_dir_all(project.join(".git/hooks")).unwrap();
        let share = temp.path().join("share");
        fs::create_dir_all(&share).unwrap();
        let scratch = temp.path().join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        let probe = "for secret in .config/gh/hosts.yml .config/abc/agent-app/private-key.pem \
                     Library/Keychains/login.keychain-db .ssh/id_ed25519; do \
                       cat \"$HOME/$secret\" >/dev/null 2>&1 && echo \"read $secret\"; done; \
                     for file in \"$CLAUDE_PROJECT_DIR/built\" \"$CLAUDE_PROJECT_DIR/.claude/worktrees/feature/built\" \
                       \"$TMPDIR/scratch\" \"$SHARE/answer\"; do \
                       touch \"$file\" 2>/dev/null && echo \"wrote ${file##*/}\"; done; \
                     for file in \"$HOME/.zshrc\" \"$CLAUDE_PROJECT_DIR/.claude/settings.json\" \
                       \"$CLAUDE_PROJECT_DIR/.claude/hooks\" \"$CLAUDE_PROJECT_DIR/.git/hooks/pre-push\" \
                       \"$CLAUDE_PROJECT_DIR/.claude/worktrees/feature/.claude/settings.json\" \
                       \"$CLAUDE_PROJECT_DIR/.mcp.json\" \"$CLAUDE_PROJECT_DIR/.vscode\"; do \
                       touch \"$file\" 2>/dev/null && echo \"wrote $file\"; done; \
                     echo ran";
        let output = Command::new("sh")
            .arg(trusted_script("jail.sh"))
            .args(["--share".as_ref(), share.as_os_str()])
            .args(["sh", "-c", probe])
            .env("HOME", &home)
            .env("TMPDIR", &scratch)
            .env("CLAUDE_PROJECT_DIR", &project)
            .env("SHARE", share.canonicalize().unwrap())
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if can_apply_seatbelt() {
            assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
            assert_eq!(
                stdout, "wrote built\nwrote built\nwrote scratch\nwrote answer\nran\n",
                "no credential read, and writes only to the checkout, its worktrees' code, the \
                 jail's temporary directory and the shared one: {stderr}"
            );
        } else {
            assert_eq!(output.status.code(), Some(2), "{stdout}{stderr}");
            assert!(stdout.is_empty(), "nothing ran unconfined: {stdout}");
            assert!(
                stderr.starts_with("abc jail: ") && stderr.contains("the command did not run"),
                "{stderr}"
            );
        }
        assert_eq!(
            fs::read_dir(&scratch).unwrap().count(),
            0,
            "the jail removed its temporary directory"
        );
    }

    #[test]
    fn the_jail_refuses_without_a_project_or_a_command() {
        let refusal = |args: &[&str], project: Option<&str>| {
            let mut command = Command::new("sh");
            command.arg(trusted_script("jail.sh")).args(args);
            match project {
                Some(project) => command.env("CLAUDE_PROJECT_DIR", project),
                None => command.env_remove("CLAUDE_PROJECT_DIR"),
            };
            let output = command.output().unwrap();
            (
                output.status.code(),
                String::from_utf8(output.stdout).unwrap(),
                String::from_utf8(output.stderr).unwrap(),
            )
        };
        for (args, project) in [
            (&["echo", "ran"][..], None),
            (&["echo", "ran"][..], Some("/nonexistent/abc")),
            (&[][..], Some("/")),
            (
                &["--share", "/nonexistent/share", "echo", "ran"][..],
                Some("/"),
            ),
            (&["--share", "/"][..], Some("/")),
        ] {
            let (code, stdout, stderr) = refusal(args, project);
            assert_eq!(code, Some(2), "{args:?} {project:?}: {stderr}");
            assert!(stdout.is_empty(), "{args:?} {project:?}: nothing ran");
            assert!(stderr.starts_with("abc jail: "), "{stderr}");
        }
    }

    /// A copy of `stop.sh` in `dir/.claude/hooks`, beside a jail that runs its command as it is,
    /// and fake `cargo` and `claude` in `dir/bin`. The fake cargo writes the watchdog's prompt when
    /// `PROMPT` is set and exits `CHECK` for the check; for the verdict it records claude's answer
    /// and exits `VERDICT`. The fake claude records its arguments, environment and prompt and runs
    /// `CLAUDE`.
    fn stop_hook(dir: &Path) -> PathBuf {
        let hooks = dir.join(".claude/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let stop = hooks.join("stop.sh");
        fs::copy(trusted_script("stop.sh"), &stop).unwrap();
        fs::write(
            hooks.join("jail.sh"),
            "[ \"$1\" = --share ] && shift 2\n\"$@\"\n",
        )
        .unwrap();
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        crate::testing::script(
            &bin.join("cargo"),
            "case \"$2 $3\" in\n\
             'hook stop') cat >/dev/null; [ -n \"$PROMPT\" ] && printf %s \"$PROMPT\" > \"$5\"; exit \"${CHECK:-0}\" ;;\n\
             'watchdog ') cat > \"$OUT/answer\"; exit \"${VERDICT:-0}\" ;;\n\
             esac\nexit 99\n",
        );
        crate::testing::script(
            &bin.join("claude"),
            "printf '%s\\n' \"$@\" > \"$OUT/arguments\"\nprintf %s \"$ABC_WATCHDOG\" > \"$OUT/guard\"\n\
             cat > \"$OUT/prompt\"\neval \"$CLAUDE\"\n",
        );
        stop
    }

    /// Runs the stop hook in `dir` with the hook input `{}`, the fakes' settings in `vars`, and
    /// returns its exit code, stdout and stderr.
    fn run_stop(dir: &Path, vars: &[(&str, &str)]) -> (Option<i32>, String, String) {
        let stop = stop_hook(dir);
        let path = format!(
            "{}:{}",
            dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut child = Command::new("sh")
            .arg(stop)
            .env("PATH", path)
            .env("CLAUDE_PROJECT_DIR", dir)
            .env("OUT", dir)
            .envs(vars.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        super::feed(&mut child, "{}").unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    #[test]
    fn the_stop_hook_keeps_an_agent_working_while_the_check_fails() {
        let temp = tempfile::tempdir().unwrap();
        let (code, stdout, stderr) = run_stop(temp.path(), &[("CHECK", "1"), ("PROMPT", "p")]);
        assert_eq!(code, Some(2), "{stderr}");
        assert_eq!(stdout, "xtask stop hook: keep working; see stderr\n");
        assert!(
            !temp.path().join("arguments").exists(),
            "no watchdog while the check fails"
        );
        let (passed, _, passed_err) = run_stop(temp.path(), &[]);
        assert_eq!(passed, Some(0), "{passed_err}");
        assert!(
            !temp.path().join("arguments").exists(),
            "no prompt, no watchdog"
        );
    }

    #[test]
    fn the_stop_hook_asks_claude_outside_the_jail_with_nothing_to_act_with() {
        let temp = tempfile::tempdir().unwrap();
        let answer = r#"{"result": "{\"on_track\": true, \"reasons\": []}"}"#;
        let claude = format!("printf '%s' '{answer}'");
        let (code, _, stderr) = run_stop(
            temp.path(),
            &[("PROMPT", "Judge this."), ("CLAUDE", &claude)],
        );
        assert_eq!(code, Some(0), "{stderr}");
        let arguments = fs::read_to_string(temp.path().join("arguments")).unwrap();
        assert_eq!(
            arguments,
            "-p\n--model\nhaiku\n--output-format\njson\n--settings\n{\"disableAllHooks\": true}\n\
             --tools\n\n--strict-mcp-config\n",
            "the cheapest model, no hooks, no tools, no MCP servers"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("guard")).unwrap(),
            "1",
            "ABC_WATCHDOG stops its session's own stop hook"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("prompt")).unwrap(),
            "Judge this.",
            "the prompt the jailed check wrote"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("answer")).unwrap(),
            answer,
            "the answer goes back into the jail to be judged"
        );
        let (drifted, _, drifted_err) = run_stop(
            temp.path(),
            &[
                ("PROMPT", "Judge this."),
                ("CLAUDE", &claude),
                ("VERDICT", "2"),
            ],
        );
        assert_eq!(
            drifted,
            Some(2),
            "a drift keeps the agent working: {drifted_err}"
        );
        let (unjudged, _, unjudged_err) = run_stop(
            temp.path(),
            &[
                ("PROMPT", "Judge this."),
                ("CLAUDE", &claude),
                ("VERDICT", "101"),
            ],
        );
        assert_eq!(unjudged, Some(0), "{unjudged_err}");
        assert!(
            unjudged_err.contains("could not judge claude's answer"),
            "{unjudged_err}"
        );
    }

    #[test]
    fn the_stop_hook_steps_aside_when_claude_fails_or_is_slow() {
        let temp = tempfile::tempdir().unwrap();
        let (failed, _, stderr) = run_stop(
            temp.path(),
            &[("PROMPT", "Judge this."), ("CLAUDE", "exit 1")],
        );
        assert_eq!(failed, Some(0), "{stderr}");
        assert!(stderr.contains("claude failed"), "{stderr}");
        assert!(!temp.path().join("answer").exists(), "nothing to judge");
        let started = std::time::Instant::now();
        let (slow, _, slow_err) = run_stop(
            temp.path(),
            &[
                ("PROMPT", "Judge this."),
                ("CLAUDE", "exec sleep 30"),
                ("ABC_WATCHDOG_SECONDS", "1"),
            ],
        );
        assert_eq!(slow, Some(0), "{slow_err}");
        assert!(
            slow_err.contains("took longer than 1 seconds"),
            "{slow_err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "claude was stopped at the deadline"
        );
    }

    #[test]
    fn a_passed_check_writes_the_watchdogs_prompt_for_the_main_agent_alone() {
        let temp = tempfile::tempdir().unwrap();
        let repo = repository(&temp.path().join("repo"));
        crate::task::set(&repo, "Sandbox the agents").unwrap();
        let prompt = temp.path().join("prompt.md");
        let mut asked = context(&repo, Some(&repo));
        asked.watchdog_prompt = Some(prompt.clone());
        let fields = |json: &str| serde_json::from_str::<super::HookInput>(json).unwrap();
        let main_stop = fields(r#"{"hook_event_name": "Stop"}"#);
        let mut out = Vec::new();
        assert_eq!(
            super::finish_stop(false, &main_stop, &asked, &mut out).unwrap(),
            1,
            "a failed check keeps the agent working"
        );
        assert!(!prompt.exists(), "and asks no watchdog");
        let subagent = fields(r#"{"hook_event_name": "SubagentStop"}"#);
        assert_eq!(
            super::finish_stop(true, &subagent, &asked, &mut out).unwrap(),
            0
        );
        assert!(!prompt.exists(), "subagents are not judged");
        assert_eq!(
            super::finish_stop(true, &main_stop, &context(&repo, Some(&repo)), &mut out).unwrap(),
            0
        );
        assert!(!prompt.exists(), "no prompt unless stop.sh asks for one");
        assert_eq!(
            super::finish_stop(true, &main_stop, &asked, &mut out).unwrap(),
            0
        );
        assert!(
            fs::read_to_string(&prompt)
                .unwrap()
                .contains("Sandbox the agents"),
            "the main agent's stop is judged on its task"
        );
    }
}
