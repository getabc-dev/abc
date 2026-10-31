//! The Claude Code `PreToolUse` hook for Bash: blocks git commands that skip hooks or force-push.
//!
//! It reads the hook input JSON, lexes the command with [`crate::shell`], and looks at every git
//! invocation in it, including ones nested in substitutions, `sh -c` scripts and `eval`. Anything
//! it cannot read is blocked with the reason, so a change in the hook contract fails loudly.

use serde::Deserialize;

use crate::shell;

/// How deep `sh -c` and `eval` scripts are followed.
const MAX_NESTING: u8 = 4;

/// Shells whose `-c` argument is itself a script.
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// The hook's verdict on one tool call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    /// Block the call; the message goes back to the agent.
    Block(String),
}

/// The fields of Claude Code's hook input this guard reads.
#[derive(Deserialize)]
struct HookInput {
    tool_name: String,
    tool_input: serde_json::Value,
}

/// Decides whether the tool call described by the hook input JSON may run.
pub(crate) fn decide(hook_input: &str) -> Decision {
    let input: HookInput = match serde_json::from_str(hook_input) {
        Ok(input) => input,
        Err(error) => {
            return Decision::Block(format!(
                "xtask guard could not read the hook input: {error}"
            ));
        }
    };
    if input.tool_name != "Bash" {
        return Decision::Allow;
    }
    let Some(command) = input
        .tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
    else {
        return Decision::Block(
            "xtask guard found no command string in the Bash hook input".to_owned(),
        );
    };
    match find_violation(command) {
        Some(violation) => Decision::Block(violation.message()),
        None => Decision::Allow,
    }
}

/// A git shortcut the guard refuses.
#[derive(Debug, PartialEq, Eq)]
enum Violation {
    /// `--no-verify`, or `-n` on `git commit`: skips the repository's git hooks.
    NoVerify,
    /// A force push, through the given flag or refspec.
    ForcePush(String),
}

impl Violation {
    fn message(&self) -> String {
        match self {
            Self::NoVerify => {
                "Blocked: --no-verify skips the repository's git hooks. Run the command \
                               without it and fix what the hooks report."
                    .to_owned()
            }
            Self::ForcePush(how) => format!(
                "Blocked: `{how}` force-pushes, which rewrites published history. Push new commits \
                 instead; if history really has to change, ask the user to do it."
            ),
        }
    }
}

/// Finds the first violation in `script`, following nested `sh -c` scripts and `eval`.
fn find_violation(script: &str) -> Option<Violation> {
    let mut pending = vec![(script.to_owned(), 0_u8)];
    while let Some((text, depth)) = pending.pop() {
        for words in shell::simple_commands(&text) {
            if let Some(violation) = git_violation(&words) {
                return Some(violation);
            }
            if depth < MAX_NESTING {
                let inner_depth = depth.saturating_add(1);
                pending.extend(
                    nested_scripts(&words)
                        .into_iter()
                        .map(|inner| (inner, inner_depth)),
                );
            }
        }
    }
    None
}

/// Scripts a command runs through a shell's `-c` or through `eval`.
fn nested_scripts(words: &[String]) -> Vec<String> {
    let mut scripts = Vec::new();
    let mut rest = words.iter();
    while let Some(word) = rest.next() {
        let name = basename(word);
        if name == "eval" {
            scripts.push(
                rest.by_ref()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        } else if SHELLS.contains(&name) {
            let mut script_follows = false;
            for arg in rest.by_ref() {
                if script_follows {
                    scripts.push(arg.clone());
                    break;
                }
                if arg.starts_with('-') {
                    script_follows = !arg.starts_with("--") && arg.contains('c');
                } else {
                    break;
                }
            }
        }
    }
    scripts
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Global git options that take the next word as their value.
const GIT_OPTIONS_WITH_VALUE: [&str; 7] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--attr-source",
];

/// Checks one simple command. The first word naming git starts the invocation, so wrappers such as
/// `env`, `sudo`, `timeout` or `xargs` cannot hide it.
fn git_violation(words: &[String]) -> Option<Violation> {
    let mut args = words
        .iter()
        .skip_while(|word| basename(word) != "git")
        .skip(1);
    let subcommand = loop {
        let arg = args.next()?;
        if GIT_OPTIONS_WITH_VALUE.contains(&arg.as_str()) {
            args.next();
        } else if !arg.starts_with('-') {
            break arg.as_str();
        }
    };
    let args: Vec<&str> = args.map(String::as_str).collect();
    match subcommand {
        "commit" => parse_args(&args, &COMMIT)
            .into_iter()
            .find_map(|arg| match arg {
                Arg::Short('n') => Some(Violation::NoVerify),
                Arg::Long(name) if is_no_verify(name) => Some(Violation::NoVerify),
                Arg::Short(_) | Arg::Long(_) | Arg::Positional(_) => None,
            }),
        "push" => parse_args(&args, &PUSH)
            .into_iter()
            .find_map(|arg| match arg {
                Arg::Short('f') => Some(Violation::ForcePush("-f".to_owned())),
                Arg::Long(name) if is_force(name) => {
                    Some(Violation::ForcePush(format!("--{name}")))
                }
                Arg::Long(name) if is_no_verify(name) => Some(Violation::NoVerify),
                Arg::Positional(refspec) if refspec.starts_with('+') => {
                    Some(Violation::ForcePush(refspec.to_owned()))
                }
                Arg::Short(_) | Arg::Long(_) | Arg::Positional(_) => None,
            }),
        _ => parse_args(&args, &OTHER)
            .into_iter()
            .find_map(|arg| match arg {
                Arg::Long(name) if is_no_verify(name) => Some(Violation::NoVerify),
                Arg::Short(_) | Arg::Long(_) | Arg::Positional(_) => None,
            }),
    }
}

/// Git accepts any unambiguous prefix of a long option; `--no-veri` is the shortest for
/// `--no-verify`, since `--no-ver` could also be `--no-verbose`.
fn is_no_verify(name: &str) -> bool {
    name.len() >= "no-veri".len() && "no-verify".starts_with(name)
}

/// `--force`, `--force-with-lease`, `--force-if-includes`, their prefixes, and `--mirror`, which
/// force-updates every remote ref.
fn is_force(name: &str) -> bool {
    let force = name.starts_with("for")
        && ["force", "force-with-lease", "force-if-includes"]
            .iter()
            .any(|option| option.starts_with(name));
    let mirror = name.len() >= "mir".len() && "mirror".starts_with(name);
    force || mirror
}

/// The options of a git subcommand that take a value.
struct ValueOptions {
    /// Short options whose value is the rest of the word or the next word.
    short: &'static str,
    /// Short options whose optional value can only be the rest of the word.
    short_attached: &'static str,
    /// Long options that take the next word as their value unless written `--name=value`.
    long: &'static [&'static str],
}

const COMMIT: ValueOptions = ValueOptions {
    short: "mFCct",
    short_attached: "Su",
    long: &[
        "message",
        "file",
        "reuse-message",
        "reedit-message",
        "fixup",
        "squash",
        "author",
        "date",
        "template",
        "cleanup",
        "trailer",
        "pathspec-from-file",
    ],
};

const PUSH: ValueOptions = ValueOptions {
    short: "o",
    short_attached: "",
    long: &["push-option", "repo", "receive-pack", "exec"],
};

const OTHER: ValueOptions = ValueOptions {
    short: "",
    short_attached: "",
    long: &[],
};

/// One parsed argument; option values are skipped.
#[derive(Debug, PartialEq, Eq)]
enum Arg<'a> {
    Short(char),
    /// A long option's name, without `--` or `=value`.
    Long(&'a str),
    Positional(&'a str),
}

fn parse_args<'a>(args: &[&'a str], options: &ValueOptions) -> Vec<Arg<'a>> {
    let mut parsed = Vec::new();
    let mut rest = args.iter().copied();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            parsed.extend(rest.by_ref().map(Arg::Positional));
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            parsed.push(Arg::Long(name));
            if !attached && options.long.contains(&name) {
                rest.next();
            }
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) {
            let mut chars = cluster.chars();
            while let Some(c) = chars.next() {
                parsed.push(Arg::Short(c));
                if options.short.contains(c) {
                    if chars.as_str().is_empty() {
                        rest.next();
                    }
                    break;
                }
                if options.short_attached.contains(c) {
                    break;
                }
            }
        } else {
            parsed.push(Arg::Positional(arg));
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::{Decision, Violation, decide, find_violation};

    fn assert_blocked(command: &str, expected: &Violation) {
        assert_eq!(
            find_violation(command).as_ref(),
            Some(expected),
            "`{command}` must be blocked"
        );
    }

    fn assert_allowed(command: &str) {
        assert_eq!(find_violation(command), None, "`{command}` must be allowed");
    }

    fn force(how: &str) -> Violation {
        Violation::ForcePush(how.to_owned())
    }

    #[test]
    fn blocks_no_verify_on_commit() {
        for command in [
            "git commit --no-verify -m wip",
            "git commit -m wip --no-verify",
            "git commit -n -m wip",
            "git commit -anm wip",
            "git commit -am wip --no-veri",
            "git commit --amend --no-verify",
            "git commit -C HEAD -n",
            "git -c user.name=agent commit -n",
            "git -C repo commit --no-verify",
        ] {
            assert_blocked(command, &Violation::NoVerify);
        }
    }

    #[test]
    fn blocks_no_verify_on_any_subcommand() {
        for command in [
            "git push --no-verify",
            "git merge --no-verify feature",
            "git rebase --no-verify main",
            "git am --no-verify patch",
        ] {
            assert_blocked(command, &Violation::NoVerify);
        }
    }

    #[test]
    fn blocks_force_pushes() {
        assert_blocked("git push --force", &force("--force"));
        assert_blocked("git push -f origin main", &force("-f"));
        assert_blocked("git push -uf origin feature", &force("-f"));
        assert_blocked("git push origin main --force", &force("--force"));
        assert_blocked("git push --forc", &force("--forc"));
        assert_blocked("git push --force-with-lease", &force("--force-with-lease"));
        assert_blocked(
            "git push --force-with-lease=main:abc123 origin main",
            &force("--force-with-lease"),
        );
        assert_blocked(
            "git push --force-if-includes",
            &force("--force-if-includes"),
        );
        assert_blocked("git push --mirror backup", &force("--mirror"));
        assert_blocked("git push origin +main", &force("+main"));
        assert_blocked(
            "git push origin +HEAD:refs/heads/main",
            &force("+HEAD:refs/heads/main"),
        );
        assert_blocked("git push origin -- +main", &force("+main"));
        assert_blocked("git push \"--force\"", &force("--force"));
        assert_blocked("git push $'--force'", &force("--force"));
    }

    #[test]
    fn finds_git_behind_wrappers_and_operators() {
        for command in [
            "cd repo && git push -f",
            "FOO=1 git push -f",
            "env GIT_TRACE=1 git push -f",
            "timeout 60 git push -f",
            "/usr/bin/git push -f",
            "git status; git push -f",
            "true || git push -f",
            "git status | cat & git push -f",
            "git status\ngit push -f",
            "(git push -f)",
            "echo $(git push -f)",
            "echo \"$(git push -f)\"",
            "echo `git push -f`",
            "bash -c 'git push -f'",
            "sh -lc \"cd repo && git push -f\"",
            "sudo -u ci bash -c 'git push -f'",
            "eval git push -f",
            "eval 'git push -f'",
            "bash -c \"sh -c 'git push -f'\"",
        ] {
            assert_blocked(command, &force("-f"));
        }
    }

    #[test]
    fn allows_ordinary_git_commands() {
        for command in [
            "git status",
            "git commit -m wip",
            "git commit --amend --no-edit",
            "git commit -F message.txt",
            "git commit -C HEAD",
            "git commit -m -n",
            "git commit -m \"fix --no-verify handling\"",
            "git commit -m 'git push -f is blocked'",
            "git commit -m wip -- path/--no-verify",
            "git push",
            "git push origin main",
            "git push -u origin feature",
            "git push --tags",
            "git push -o ci.skip origin main",
            "git push -of origin main",
            "git push --no-force-with-lease",
            "git push origin :stale-branch",
            "git log -n 3",
            "git fetch -f",
            "git checkout -f main",
            "git -C /tmp/git push origin main",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn allows_commands_that_only_mention_git() {
        for command in [
            "echo 'git push -f'",
            "grep -r \"push --force\" .",
            "rm -f target/x",
            "cargo test -- --no-verify",
            "bash script.sh -f",
            "git status # git push -f",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn allows_heredoc_commit_messages() {
        assert_allowed(
            "git commit -m \"$(cat <<'EOF'\nfeat: block git push --force\n\nIt's done; never use --no-verify.\nEOF\n)\"",
        );
    }

    #[test]
    fn decides_from_hook_input() {
        let blocked = decide(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git push --force","description":"push"}}"#,
        );
        let Decision::Block(message) = blocked else {
            panic!("a force push must be blocked, got {blocked:?}");
        };
        assert!(
            message.contains("`--force` force-pushes"),
            "the message names the flag: {message}"
        );
        assert_eq!(
            decide(r#"{"tool_name":"Bash","tool_input":{"command":"git push origin main"}}"#),
            Decision::Allow,
            "an ordinary push is allowed"
        );
    }

    #[test]
    fn ignores_other_tools() {
        assert_eq!(
            decide(
                r#"{"tool_name":"Edit","tool_input":{"file_path":"/x","old_string":"git push -f"}}"#
            ),
            Decision::Allow,
            "only Bash commands are checked"
        );
    }

    #[test]
    fn blocks_unreadable_hook_input() {
        for input in [
            "",
            "not json",
            r#"{"tool_input":{"command":"ls"}}"#,
            r#"{"tool_name":"Bash","tool_input":{}}"#,
            r#"{"tool_name":"Bash","tool_input":{"command":7}}"#,
        ] {
            assert!(
                matches!(decide(input), Decision::Block(_)),
                "input the guard cannot read must block: {input}"
            );
        }
    }
}
