//! The xtask binary end to end: its entry point, exit codes and environment.

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

    /// Runs the xtask binary with `args`, `stdin` and extra environment `env`, isolated from the
    /// caller's GitHub Actions variables, App files and token broker by a home of its own.
    fn xtask(args: &[&str], stdin: &str, env: &[(&str, &Path)]) -> Output {
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(args)
            .env("HOME", home.path())
            .env_remove("GITHUB_REPOSITORY")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITHUB_EVENT_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// A git repository with a `base` branch and one commit on top that changes nothing a gate sees.
    fn repository(path: &Path) {
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "-c",
                    "user.name=xtask",
                    "-c",
                    "user.email=xtask@example.com",
                ])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        std::fs::create_dir_all(path).unwrap();
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "start"]);
        git(&["branch", "base"]);
        std::fs::write(path.join("README.md"), "x\n").unwrap();
        git(&["add", "--all"]);
        git(&["commit", "--quiet", "-m", "change"]);
    }

    #[test]
    fn usage_errors_exit_1_and_commands_report_their_errors() {
        let unknown = xtask(&["no-such-command"], "", &[]);
        assert_eq!(unknown.status.code(), Some(1), "never 2, a hook's block");
        let missing = xtask(&["--root", "/no/such/tree", "task", "show"], "", &[]);
        assert_eq!(missing.status.code(), Some(1), "{}", stderr(&missing));
        assert!(
            stderr(&missing).contains("error: --root /no/such/tree does not exist"),
            "{}",
            stderr(&missing)
        );
    }

    #[test]
    fn runs_a_gate_on_the_tree_at_root() {
        let checked = xtask(&["rulesets", "check"], "", &[]);
        assert!(checked.status.success(), "{}", stderr(&checked));
        assert!(
            stderr(&checked).contains("matches CI"),
            "{}",
            stderr(&checked)
        );
    }

    #[test]
    fn reads_github_actions_variables_from_the_environment() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("tree");
        repository(&tree);
        let payload = temp.path().join("event.json");
        std::fs::write(
            &payload,
            r#"{"action": "opened", "pull_request": {"body": ""}}"#,
        )
        .unwrap();
        let root = tree.to_str().unwrap();
        let args = ["integrity", "--root", root, "--base", "base", "--pr", "7"];
        let bare = xtask(&args, "", &[]);
        assert!(
            !bare.status.success(),
            "a pull request run needs GitHub Actions"
        );
        assert!(
            stderr(&bare).contains("GITHUB_REPOSITORY is not set"),
            "{}",
            stderr(&bare)
        );
        let actions = xtask(
            &args,
            "",
            &[
                ("GITHUB_REPOSITORY", Path::new("o/r")),
                ("GITHUB_TOKEN", Path::new("token")),
                ("GITHUB_EVENT_PATH", &payload),
            ],
        );
        assert!(actions.status.success(), "{}", stderr(&actions));
        assert!(
            stderr(&actions).contains("no deleted, skipped or weakened tests"),
            "{}",
            stderr(&actions)
        );
    }

    #[test]
    fn runs_the_pinned_tools_of_its_own_checkout() {
        let version = xtask(&["tool", "cargo-nextest", "--version"], "", &[]);
        assert!(version.status.success(), "{}", stderr(&version));
        assert!(
            String::from_utf8_lossy(&version.stdout).contains("cargo-nextest"),
            "the pinned cargo-nextest ran"
        );
    }

    #[test]
    fn the_credential_helper_needs_the_app() {
        let other_host = xtask(
            &["git-credential", "get"],
            "protocol=https\nhost=example.com\n\n",
            &[],
        );
        assert!(other_host.status.success(), "{}", stderr(&other_host));
        assert!(other_host.stdout.is_empty(), "nothing for other hosts");
        let github = xtask(
            &["git-credential", "get"],
            "protocol=https\nhost=github.com\n\n",
            &[],
        );
        assert_eq!(github.status.code(), Some(1), "no broker, no token");
        assert!(github.stdout.is_empty(), "and nothing for git to use");
        assert!(
            stderr(&github).contains("token broker"),
            "{}",
            stderr(&github)
        );
    }

    #[test]
    fn gh_runs_only_with_a_token_from_the_broker() {
        let gh = xtask(&["gh", "--", "auth", "status"], "", &[]);
        assert_eq!(gh.status.code(), Some(1), "no broker, no gh");
        assert!(stderr(&gh).contains("token broker"), "{}", stderr(&gh));
    }

    #[test]
    fn the_broker_needs_the_app() {
        let broker = xtask(&["broker"], "", &[]);
        assert_eq!(broker.status.code(), Some(1), "no App files, no broker");
        assert!(stderr(&broker).contains("app.json"), "{}", stderr(&broker));
    }

    #[test]
    fn the_watchdog_judges_claudes_answer() {
        let answer =
            |result: &str| serde_json::json!({"type": "result", "result": result}).to_string();
        let on_track = xtask(
            &["watchdog"],
            &answer(r#"{"on_track": true, "reasons": []}"#),
            &[],
        );
        assert_eq!(on_track.status.code(), Some(0), "{}", stderr(&on_track));
        let drifted = xtask(
            &["watchdog"],
            &answer(r#"{"on_track": false, "reasons": ["off task (S1)"]}"#),
            &[],
        );
        assert_eq!(drifted.status.code(), Some(2), "keeps the agent working");
        assert!(
            stderr(&drifted).contains("off task (S1)"),
            "{}",
            stderr(&drifted)
        );
    }
}
