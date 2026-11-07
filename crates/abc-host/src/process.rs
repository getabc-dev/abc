//! Running other programs, and showing their command lines to people.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};

use crate::HostError;

/// A program and its arguments, run with a clean slate of standard input.
#[derive(Debug, Clone)]
pub(crate) struct Invocation {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Invocation {
    pub(crate) fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    #[must_use]
    pub(crate) fn args<A: AsRef<OsStr>>(mut self, args: impl IntoIterator<Item = A>) -> Self {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn arguments(&self) -> &[OsString] {
        &self.args
    }

    /// The command line as a shell would take it, for messages.
    pub(crate) fn shown(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }

    fn spawn_error(&self, fix: &str, source: std::io::Error) -> HostError {
        HostError::Spawn {
            command: self.shown(),
            fix: fix.to_owned(),
            source,
        }
    }

    /// Runs it with no input and captures what it prints.
    pub(crate) fn output(&self, fix: &str) -> Result<Output, HostError> {
        self.command()
            .stdin(Stdio::null())
            .output()
            .map_err(|source| self.spawn_error(fix, source))
    }

    /// Runs it, and returns its standard output when it succeeds.
    pub(crate) fn run(&self, fix: &str) -> Result<String, HostError> {
        let output = self.output(fix)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(self.failed(output.status, &output, fix))
        }
    }

    /// Runs it on the person's terminal, for a program that asks them something.
    pub(crate) fn interactive(&self, fix: &str) -> Result<(), HostError> {
        let status = self
            .command()
            .status()
            .map_err(|source| self.spawn_error(fix, source))?;
        if status.success() {
            Ok(())
        } else {
            Err(HostError::Failed {
                command: self.shown(),
                status: status.to_string(),
                output: String::new(),
                fix: fix.to_owned(),
            })
        }
    }

    /// Starts it in the background, with no input and its output captured.
    pub(crate) fn spawn(&self, fix: &str) -> Result<Child, HostError> {
        self.command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| self.spawn_error(fix, source))
    }

    pub(crate) fn failed(&self, status: ExitStatus, output: &Output, fix: &str) -> HostError {
        let mut printed = String::from_utf8_lossy(&output.stderr).into_owned();
        if printed.trim().is_empty() {
            printed = String::from_utf8_lossy(&output.stdout).into_owned();
        }
        HostError::Failed {
            command: self.shown(),
            status: status.to_string(),
            output: last_lines(&printed, 20),
            fix: fix.to_owned(),
        }
    }
}

/// The last `count` lines of `text`.
fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(count);
    lines.get(skip..).unwrap_or_default().join("\n")
}

/// `word` as a shell reads it back: bare when it holds only characters no shell treats
/// specially, else in single quotes.
pub(crate) fn quote(word: &OsStr) -> String {
    let text = word.to_string_lossy();
    let plain = !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'.' | b'_' | b'-' | b':' | b'=' | b'@' | b',' | b'+' | b'%'
                )
        });
    if plain {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The first executable called `name` in `path`'s directories.
pub(crate) fn find_on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().as_bytes().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a file someone may execute.
pub(crate) fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Invocation, find_on_path, is_executable, last_lines, quote};
    use crate::HostError;

    #[test]
    fn words_are_quoted_only_when_a_shell_needs_it() {
        assert_eq!(quote(OsStr::new("/usr/bin/sudo")), "/usr/bin/sudo", "plain");
        assert_eq!(
            quote(OsStr::new("--socket=/a/b.sock")),
            "--socket=/a/b.sock",
            "flags"
        );
        assert_eq!(
            quote(OsStr::new("/Users/ada/Library/Application Support/x")),
            "'/Users/ada/Library/Application Support/x'",
            "a space"
        );
        assert_eq!(quote(OsStr::new("it's")), r"'it'\''s'", "a quote");
        assert_eq!(quote(OsStr::new("")), "''", "empty");
        assert_eq!(quote(OsStr::new("a$b")), "'a$b'", "a dollar");
    }

    #[test]
    fn a_command_line_shows_its_program_and_arguments() {
        let shown = Invocation::new("/bin/echo")
            .arg("a b")
            .args(["c", "d"])
            .shown();
        assert_eq!(shown, "/bin/echo 'a b' c d", "quoted as a shell would");
    }

    #[test]
    fn run_returns_output_or_a_failure_with_the_fix() {
        let printed = Invocation::new("/bin/sh")
            .args(["-c", "printf \"$0\"", "hello"])
            .run("fix it")
            .unwrap();
        assert_eq!(printed, "hello", "stdout");
        let error = Invocation::new("/bin/sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .run("fix it")
            .unwrap_err();
        let HostError::Failed {
            status,
            output,
            fix,
            ..
        } = &error
        else {
            panic!("{error:?}");
        };
        assert!(status.contains('3'), "{status}");
        assert_eq!(output, "err", "stderr first");
        assert_eq!(fix, "fix it", "the fix");
        let only_stdout = Invocation::new("/bin/sh")
            .args(["-c", "echo out; exit 1"])
            .run("f")
            .unwrap_err();
        assert!(
            matches!(&only_stdout, HostError::Failed { output: out, .. } if out == "out"),
            "stdout when stderr is empty: {only_stdout:?}"
        );
        let missing = Invocation::new("/nonexistent/program")
            .run("install it")
            .unwrap_err();
        assert!(
            matches!(&missing, HostError::Spawn { fix: said, .. } if said == "install it"),
            "{missing:?}"
        );
    }

    #[test]
    fn interactive_and_spawn_report_how_the_program_ended() {
        Invocation::new("/bin/sh")
            .args(["-c", "exit 0"])
            .interactive("f")
            .unwrap();
        let error = Invocation::new("/bin/sh")
            .args(["-c", "exit 4"])
            .interactive("do this")
            .unwrap_err();
        assert!(
            matches!(&error, HostError::Failed { fix, .. } if fix == "do this"),
            "{error:?}"
        );
        assert!(
            matches!(
                Invocation::new("/nonexistent").interactive("f"),
                Err(HostError::Spawn { .. })
            ),
            "cannot start"
        );
        let child = Invocation::new("/bin/sh")
            .args(["-c", "echo spawned"])
            .spawn("f")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.stdout, b"spawned\n", "captured");
        assert!(
            matches!(
                Invocation::new("/nonexistent").spawn("f"),
                Err(HostError::Spawn { .. })
            ),
            "cannot start"
        );
    }

    #[test]
    fn only_the_last_lines_are_kept() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc", "the end");
        assert_eq!(last_lines("a", 5), "a", "all of a short text");
    }

    #[test]
    fn programs_are_found_only_where_path_says_and_only_if_executable() {
        let scratch = tempfile::tempdir().unwrap();
        let first = scratch.path().join("first");
        let second = scratch.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("tool"), "").unwrap();
        std::fs::write(second.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(second.join("tool"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(!is_executable(&first.join("tool")), "not executable");
        assert!(!is_executable(&first), "a directory");
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            find_on_path(&path, "tool"),
            Some(second.join("tool")),
            "the first executable one"
        );
        assert_eq!(find_on_path(&path, "other"), None, "absent");
        // An empty entry means the current directory to a shell; abc never looks there. Each test
        // runs in a process of its own (nextest), so changing directory touches no other test.
        std::env::set_current_dir(&second).unwrap();
        assert_eq!(
            find_on_path(&OsString::from(":"), "tool"),
            None,
            "an empty entry finds nothing, not the current directory's program"
        );
        assert_eq!(
            find_on_path(&OsString::new(), "tool"),
            None,
            "an empty PATH finds nothing"
        );
    }
}
