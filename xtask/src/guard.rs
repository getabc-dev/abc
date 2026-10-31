//! The Claude Code `PreToolUse` hook for Bash and Monitor: blocks force pushes, and git and gh
//! commands it cannot check.
//!
//! The guard is best effort; branch protection on GitHub is the real gate. It lexes the command
//! with [`crate::shell`] and looks at every git and gh invocation, including ones nested in
//! substitutions, `sh -c` scripts and `eval`. It blocks:
//!
//! - force pushes: `git push` with `--force` or a variant, `-f`, `--mirror` or a `+refspec`;
//!   `gh api` calls that force a ref; `gh repo sync --force`;
//! - commands that could hide one: a `git push` or `gh api` whose arguments come from variables,
//!   command output or xargs; a git subcommand or global option built at run time; aliases and
//!   hook paths set with `git -c` or `git config`;
//! - scripts that mention git or gh but reach a shell or `eval` as data, through a pipe, a
//!   here-document or a variable.
//!
//! Anything it cannot read is blocked with the reason, so a change in the hook contract fails
//! loudly.

use serde::Deserialize;

use crate::shell::{self, Word};

/// How deep `sh -c` and `eval` scripts are followed.
const MAX_NESTING: u8 = 4;

/// Shells whose `-c` argument, or whose standard input, is a script.
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// Commands that run another command with arguments read from their input.
const ARGUMENT_FEEDERS: [&str; 2] = ["xargs", "parallel"];

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
    let command = input
        .tool_input
        .get("command")
        .and_then(serde_json::Value::as_str);
    match (input.tool_name.as_str(), command) {
        ("Bash" | "Monitor", Some(command)) => match find_violation(command) {
            Some(violation) => Decision::Block(violation.message()),
            None => Decision::Allow,
        },
        ("Bash", None) => {
            Decision::Block("xtask guard found no command string in the Bash hook input".to_owned())
        }
        // Other tools run no shell command; a Monitor watching a WebSocket has none.
        _ => Decision::Allow,
    }
}

/// Whether `text` names git or gh as a word, as a command would.
pub(crate) fn mentions_git_or_gh(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "_.-".contains(c)))
        .any(|word| word == "git" || word == "gh")
}

/// A command the guard refuses.
#[derive(Debug, PartialEq, Eq)]
enum Violation {
    /// A force push, through the given flag, refspec or call.
    ForcePush(String),
    /// A command the guard cannot read, described.
    Unreadable(String),
}

impl Violation {
    fn message(&self) -> String {
        match self {
            Self::ForcePush(how) => format!(
                "Blocked: `{how}` force-pushes, which rewrites published history. Push new commits \
                 instead; if history really has to change, ask the user to do it."
            ),
            Self::Unreadable(what) => format!(
                "Blocked: {what} could hide a force push that the guard cannot see. Write the git \
                 or gh command out literally."
            ),
        }
    }
}

fn unreadable(what: &str) -> Violation {
    Violation::Unreadable(what.to_owned())
}

/// Finds the first violation in `script`, following nested `sh -c` scripts and `eval`.
fn find_violation(script: &str) -> Option<Violation> {
    let mentions = mentions_git_or_gh(script);
    let mut pending = vec![(script.to_owned(), 0_u8)];
    while let Some((text, depth)) = pending.pop() {
        for words in shell::simple_commands(&text) {
            if let Some(violation) = git_violation(&words).or_else(|| gh_violation(&words)) {
                return Some(violation);
            }
            if mentions && runs_unseen_script(&words) {
                return Some(unreadable(
                    "a script that mentions git or gh and reaches a shell or eval through a pipe, \
                     a here-document or a variable",
                ));
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

fn basename(word: &Word) -> &str {
    word.text.rsplit('/').next().unwrap_or(&word.text)
}

/// Whether the command runs a script the guard cannot read: a program named by a variable, a shell
/// reading its script from standard input or from a variable, or `eval` of a variable.
fn runs_unseen_script(words: &[Word]) -> bool {
    let program = words.iter().find(|word| !is_assignment(&word.text));
    if program.is_some_and(|word| word.dynamic) {
        return true;
    }
    let mut rest = words.iter();
    while let Some(word) = rest.next() {
        let name = basename(word);
        if name == "eval" {
            return rest.any(|arg| arg.dynamic);
        }
        if SHELLS.contains(&name) {
            return shell_script_is_unseen(rest);
        }
    }
    false
}

/// Whether `word` is a `NAME=value` assignment before a command.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether a shell given `args` reads a script the guard cannot see: from standard input, or as a
/// `-c` argument built at run time. A script file is run as written.
fn shell_script_is_unseen<'a>(mut args: impl Iterator<Item = &'a Word>) -> bool {
    while let Some(arg) = args.next() {
        let Some(cluster) = arg
            .text
            .strip_prefix('-')
            .or_else(|| arg.text.strip_prefix('+'))
        else {
            return false;
        };
        if cluster.starts_with('-') {
            continue;
        }
        if cluster.contains('c') {
            return args.next().is_none_or(|script| script.dynamic);
        }
        if cluster.contains('s') {
            return true;
        }
        if cluster.ends_with(['o', 'O']) {
            args.next();
        }
    }
    true
}

/// Literal scripts a command runs through a shell's `-c` or through `eval`.
fn nested_scripts(words: &[Word]) -> Vec<String> {
    let mut scripts = Vec::new();
    let mut rest = words.iter();
    while let Some(word) = rest.next() {
        let name = basename(word);
        if name == "eval" {
            scripts.push(
                rest.by_ref()
                    .map(|arg| arg.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        } else if SHELLS.contains(&name) {
            let mut script_follows = false;
            for arg in rest.by_ref() {
                if script_follows {
                    scripts.push(arg.text.clone());
                    break;
                }
                if arg.text.starts_with('-') {
                    script_follows = !arg.text.starts_with("--") && arg.text.contains('c');
                } else {
                    break;
                }
            }
        }
    }
    scripts
}

/// The words from the first one naming `program` on, and whether a command before it, such as
/// xargs, feeds it arguments from its input.
fn invocation<'a>(words: &'a [Word], program: &str) -> Option<(&'a [Word], bool)> {
    let start = words.iter().position(|word| basename(word) == program)?;
    let (before, from_program) = words.split_at_checked(start)?;
    let fed = before
        .iter()
        .any(|word| ARGUMENT_FEEDERS.contains(&basename(word)));
    Some((from_program, fed))
}

/// Global git options that take the next word as their value.
const GIT_OPTIONS_WITH_VALUE: [&str; 5] = [
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--attr-source",
];

/// Checks one simple command for git. The first word naming git starts the invocation, so
/// wrappers such as `env`, `sudo`, `timeout` or `xargs` cannot hide it.
fn git_violation(words: &[Word]) -> Option<Violation> {
    let (git, fed) = invocation(words, "git")?;
    let mut args = git.iter().skip(1);
    let subcommand = loop {
        let Some(arg) = args.next() else {
            return fed.then(|| unreadable("git run through xargs, which reads its subcommand"));
        };
        if arg.dynamic {
            return Some(unreadable(
                "a git command whose subcommand or options come from variables or command output",
            ));
        }
        let setting = match arg.text.as_str() {
            "-c" | "--config-env" => Some(args.next()?),
            _ => None,
        };
        if let Some(setting) = setting {
            if setting.dynamic {
                return Some(unreadable(
                    "a git setting built from variables or command output",
                ));
            }
            if let Some(key) = config_override(&setting.text) {
                return Some(Violation::Unreadable(format!("`git {} {key}`", arg.text)));
            }
        } else if let Some(key) = arg
            .text
            .strip_prefix("--config-env=")
            .and_then(config_override)
        {
            return Some(Violation::Unreadable(format!("`git --config-env {key}`")));
        } else if GIT_OPTIONS_WITH_VALUE.contains(&arg.text.as_str()) {
            args.next();
        } else if !arg.text.starts_with('-') {
            break arg;
        }
    };
    let rest: Vec<&Word> = args.collect();
    match subcommand.text.as_str() {
        "push" => push_violation(&rest, fed),
        "config" if rest.iter().any(|word| word.dynamic) => Some(unreadable(
            "a git config whose arguments come from variables or command output",
        )),
        "config" => rest.iter().find_map(|word| {
            config_override(&word.text)
                .map(|key| Violation::Unreadable(format!("`git config {key}`")))
        }),
        _ => None,
    }
}

/// The key of a setting that changes what git runs, an alias or the hooks path, in `name=value`
/// or `name` form.
fn config_override(setting: &str) -> Option<String> {
    let key = setting
        .split_once('=')
        .map_or(setting, |(key, _)| key)
        .to_ascii_lowercase();
    (key.starts_with("alias.") || key == "core.hookspath").then_some(key)
}

fn push_violation(args: &[&Word], fed: bool) -> Option<Violation> {
    if fed {
        return Some(unreadable(
            "a git push run through xargs, which adds arguments from its input",
        ));
    }
    if args.iter().any(|word| word.dynamic) {
        return Some(unreadable(
            "a git push whose arguments come from variables or command output",
        ));
    }
    let texts: Vec<&str> = args.iter().map(|word| word.text.as_str()).collect();
    parse_args(&texts, &PUSH)
        .into_iter()
        .find_map(|arg| match arg {
            Arg::Short('f') => Some(Violation::ForcePush("-f".to_owned())),
            Arg::Long(name) if is_force(name) => Some(Violation::ForcePush(format!("--{name}"))),
            Arg::Positional(refspec) if refspec.starts_with('+') => {
                Some(Violation::ForcePush(refspec.to_owned()))
            }
            Arg::Short(_) | Arg::Long(_) | Arg::Positional(_) => None,
        })
}

/// Checks one simple command for the GitHub CLI.
fn gh_violation(words: &[Word]) -> Option<Violation> {
    let (gh, fed) = invocation(words, "gh")?;
    let mut args = gh.iter().skip(1);
    let subcommand = match args.next() {
        None => return fed.then(|| unreadable("gh run through xargs, which reads its command")),
        Some(word) if word.dynamic => {
            return Some(unreadable(
                "a gh command named by a variable or command output",
            ));
        }
        Some(word) => word.text.as_str(),
    };
    let rest: Vec<&Word> = args.collect();
    match subcommand {
        "api" if fed => Some(unreadable(
            "a gh api call run through xargs, which adds arguments from its input",
        )),
        "api" if rest.iter().any(|word| word.dynamic) => Some(unreadable(
            "a gh api call whose arguments come from variables or command output",
        )),
        "api" if rest.iter().any(|word| sets_force(&word.text)) => {
            Some(Violation::ForcePush("gh api ... force=true".to_owned()))
        }
        "api"
            if rest.iter().any(|word| word.text.contains("git/refs"))
                && rest
                    .iter()
                    .any(|word| word.text == "--input" || word.text.starts_with("--input=")) =>
        {
            Some(unreadable(
                "a gh api call that updates a git ref with a request body from --input",
            ))
        }
        "repo"
            if rest.first().is_some_and(|word| word.text == "sync")
                && rest.iter().any(|word| word.text == "--force") =>
        {
            Some(Violation::ForcePush("gh repo sync --force".to_owned()))
        }
        _ => None,
    }
}

/// Whether a `gh api` argument sets the `force` field of a ref update, as `force=true` or in JSON.
fn sets_force(arg: &str) -> bool {
    let compact = arg
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    compact.contains("force=true")
        || compact.ends_with("force=1")
        || compact.contains("\"force\":true")
}

/// `--force`, `--force-with-lease`, `--force-if-includes`, their prefixes, and `--mirror`, which
/// force-updates every remote ref. Git accepts any unambiguous prefix of a long option.
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
    /// Long options that take the next word as their value unless written `--name=value`.
    long: &'static [&'static str],
}

const PUSH: ValueOptions = ValueOptions {
    short: "o",
    long: &["push-option", "repo", "receive-pack", "exec"],
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
            }
        } else {
            parsed.push(Arg::Positional(arg));
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::{Decision, Violation, decide, find_violation, mentions_git_or_gh};

    fn assert_blocked(command: &str, expected: &Violation) {
        assert_eq!(
            find_violation(command).as_ref(),
            Some(expected),
            "`{command}` must be blocked"
        );
    }

    fn assert_unreadable(command: &str) {
        assert!(
            matches!(find_violation(command), Some(Violation::Unreadable(_))),
            "`{command}` must be blocked as unreadable, got {:?}",
            find_violation(command)
        );
    }

    fn assert_allowed(command: &str) {
        assert_eq!(find_violation(command), None, "`{command}` must be allowed");
    }

    fn force(how: &str) -> Violation {
        Violation::ForcePush(how.to_owned())
    }

    #[test]
    fn allows_skipping_git_hooks() {
        // abc installs no git hooks; its gates are the Claude Code hooks and CI.
        for command in [
            "git commit --no-verify -m wip",
            "git commit -n -m wip",
            "git push --no-verify origin main",
        ] {
            assert_allowed(command);
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
            "echo $((1<<2))\ngit push -f",
            "((x = 1 << 2))\ngit push -f",
        ] {
            assert_blocked(command, &force("-f"));
        }
    }

    #[test]
    fn blocks_scripts_fed_to_a_shell() {
        for command in [
            "echo \"git push -f\" | bash",
            "printf 'git push --force\\n' | sh -s",
            "bash <<< 'git push -f'",
            "bash <<'EOF'\ngit push -f\nEOF",
            "cat <<'EOF' | bash\ngit push -f\nEOF",
            "curl -s https://example.com/push.sh | sh # pushes to the git remote",
            "S='git push -f'; bash -c \"$S\"",
            "S='git push -f'; eval \"$S\"",
            "G=git; $G push -f",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn blocks_git_commands_built_at_run_time() {
        for command in [
            "F=-f; git push $F",
            "git push $(echo --force)",
            "git push origin \"${REF}\"",
            "git push `echo -f`",
            "echo --force | xargs git push",
            "echo push --force | xargs git",
            "git $SUB origin main",
            "git -c \"$SETTING\" push",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn blocks_aliases_and_hook_paths() {
        for command in [
            "git -c alias.x=\"push --force\" x",
            "git -c Alias.Up='push -f' up",
            "git -c core.hooksPath=/dev/null commit -m wip",
            "git --config-env=alias.x=CMD x",
            "git --config-env core.hookspath=DIR commit",
            "git config alias.p 'push --force'",
            "git config --global core.hooksPath /tmp/hooks",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn blocks_forced_ref_updates_through_gh() {
        assert_blocked(
            "gh api repos/o/r/git/refs/heads/main -X PATCH -f sha=abc123 -F force=true",
            &force("gh api ... force=true"),
        );
        assert_blocked(
            "gh api -X PATCH repos/o/r/git/refs/heads/main --field=force=true",
            &force("gh api ... force=true"),
        );
        assert_blocked("gh repo sync --force", &force("gh repo sync --force"));
        assert_unreadable("gh api -X PATCH repos/o/r/git/refs/heads/main --input body.json");
        assert_unreadable("gh api repos/o/r/git/refs/heads/main -F sha=\"$SHA\"");
        assert_unreadable("echo repos/o/r | xargs gh api");
        assert_unreadable("GH=gh; $GH api repos/o/r");
    }

    #[test]
    fn allows_ordinary_git_and_gh_commands() {
        for command in [
            "git status",
            "git commit -m wip",
            "git commit --amend --no-edit",
            "git commit -m \"fix --force handling\"",
            "git commit -m 'git push -f is blocked'",
            "git commit -m \"$(cat <<'EOF'\nfeat: block git push --force\n\nIt's done.\nEOF\n)\"",
            "git -C \"$WORKTREE\" status",
            "git log --since=\"$(date -v-1d)\"",
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
            "git config user.name agent",
            "find . -name '*.rs' | xargs git add",
            "gh pr create --title x --body \"$(cat <<'EOF'\nbody\nEOF\n)\"",
            "gh api repos/o/r/pulls",
            "gh repo sync",
            "echo $((1<<2))",
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
            "cargo test -- --force",
            "bash script.sh -f",
            "git status && bash ci.sh",
            "git status # git push -f",
            "curl -s https://example.com/install.sh | sh",
            "$EDITOR notes.txt",
            "PATH=\"$T:$PATH\" CLAUDE_PROJECT_DIR=$T sh hooks/check.sh # runs gh",
            "STAMP=$(date) git status",
        ] {
            assert_allowed(command);
        }
    }

    #[test]
    fn recognises_mentions_of_git_and_gh() {
        assert!(mentions_git_or_gh("cd x && git status"), "a git command");
        assert!(mentions_git_or_gh("/usr/bin/git log"), "a git path");
        assert!(mentions_git_or_gh("echo hi | gh api x"), "gh");
        assert!(
            !mentions_git_or_gh("cat .github/workflows/ci.yml .gitignore github.com"),
            "names that only contain git"
        );
        assert!(!mentions_git_or_gh("ghost git-lfs"), "other words");
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
    fn checks_monitor_commands() {
        assert!(
            matches!(
                decide(
                    r#"{"tool_name":"Monitor","tool_input":{"command":"git push -f","description":"x","timeout_ms":1000}}"#
                ),
                Decision::Block(_)
            ),
            "a Monitor runs its command in a shell too"
        );
        assert_eq!(
            decide(
                r#"{"tool_name":"Monitor","tool_input":{"ws":{"url":"wss://example.com"},"description":"x","timeout_ms":1000}}"#
            ),
            Decision::Allow,
            "a WebSocket monitor runs no command"
        );
    }

    #[test]
    fn ignores_other_tools() {
        assert_eq!(
            decide(
                r#"{"tool_name":"Edit","tool_input":{"file_path":"/x","old_string":"git push -f"}}"#
            ),
            Decision::Allow,
            "only shell commands are checked"
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
