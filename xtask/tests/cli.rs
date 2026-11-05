//! The xtask binary end to end: its entry point, exit codes and environment.

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Output, Stdio};

    /// Runs the xtask binary with `args`, `stdin` and extra environment `env`, isolated from the
    /// caller's GitHub Actions variables and App files.
    fn xtask(args: &[&str], stdin: &str, env: &[(&str, &Path)]) -> Output {
        let config = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(args)
            .env("XDG_CONFIG_HOME", config.path())
            .env_remove("GITHUB_REPOSITORY")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITHUB_EVENT_PATH")
            .env_remove("ABC_AGENT_BROKER")
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
    fn usage_errors_answer_nothing_and_commands_report_their_errors() {
        for args in [&["no-such-command"][..], &["hook", "post-tool-use"]] {
            // A usage error exits before reading its input, so it is given none.
            let unknown = xtask(args, "", &[]);
            assert_ne!(unknown.status.code(), Some(0), "{args:?}");
            assert!(
                unknown.stdout.is_empty(),
                "{args:?}: a usage error prints no hook answer, so it can never read as one"
            );
        }
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
    fn the_credential_helper_needs_the_broker() {
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
            stderr(&github).contains("ABC_AGENT_BROKER is not set"),
            "{}",
            stderr(&github)
        );
    }

    #[test]
    fn the_broker_logs_from_its_threads_and_stops() {
        // Outside any checkout, which it refuses to run from, with a test App and a short home.
        let install = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let exe = install.path().join("xtask");
        std::fs::copy(env!("CARGO_BIN_EXE_xtask"), &exe).unwrap();
        let config = install.path().join("config");
        let app = config.join("abc/agent-app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("app.json"),
            r#"{"client_id": "Iv23test", "installation_id": 42}"#,
        )
        .unwrap();
        let key = app.join("private-key.pem");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/agent-app-test-key.pem"),
            &key,
        )
        .unwrap();
        std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let home = install.path().join("h");
        let mut broker = Command::new(&exe)
            .args(["broker", "--session", "abc-t"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let dir = home.join(".config/abc/agent-broker");
        while std::os::unix::net::UnixStream::connect(dir.join("abc-t.ctl")).is_err() {
            assert!(
                broker.try_wait().unwrap().is_none(),
                "the broker exited early"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A refusal is logged from the thread that answered it.
        let mut asker = std::os::unix::net::UnixStream::connect(dir.join("abc-t.sock")).unwrap();
        asker.write_all(b"nope\n").unwrap();
        let mut reply = String::new();
        std::io::Read::read_to_string(&mut asker, &mut reply).unwrap();
        assert!(reply.starts_with("error "), "{reply}");
        let mut control = std::os::unix::net::UnixStream::connect(dir.join("abc-t.ctl")).unwrap();
        control
            .set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        control.write_all(b"stop\n").unwrap();
        let mut stopped = String::new();
        std::io::Read::read_to_string(&mut control, &mut stopped)
            .expect("the broker stops, its log never blocked");
        assert_eq!(stopped, "stopped revoked 0 of 0 tokens\n");
        let output = broker.wait_with_output().unwrap();
        assert!(output.status.success());
        let log = String::from_utf8_lossy(&output.stderr);
        assert!(
            log.contains("broker abc-t: refused a request: no scope is named \"nope\""),
            "{log}"
        );
    }

    /// Runs `sh xtask/hooks/hook.sh event` in the checkout `project` with the real cargo, on
    /// `input`, and returns its exit code, stdout and stderr.
    fn launch(project: &Path, event: &str, input: &str) -> (Option<i32>, String, String) {
        let mut child = Command::new("sh")
            .arg(project.join("xtask/hooks/hook.sh"))
            .arg(event)
            .env("CLAUDE_PROJECT_DIR", project)
            .env_remove("ABC_VM")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("CARGO")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr(&output),
        )
    }

    /// A copy of this repository's tracked files at a fixed place, so its xtask builds
    /// incrementally across runs into the copy's target/hook, which is kept. The copy is shared
    /// by every test process on this machine, as when a main agent's and a subagent's stop hooks
    /// check at once, so it comes with an exclusive lock that the caller holds until it is done.
    fn copy_of_the_repository() -> (std::path::PathBuf, std::fs::File) {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let copy = tmp.join("broken-workspace");
        std::fs::create_dir_all(&copy).unwrap();
        let lock = std::fs::File::create(tmp.join("broken-workspace.lock")).unwrap();
        lock.lock().unwrap();
        for entry in std::fs::read_dir(&copy).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
        }
        let listed = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output()
            .unwrap();
        assert!(listed.status.success(), "git ls-files");
        for file in String::from_utf8(listed.stdout).unwrap().split('\0') {
            let from = repo.join(file);
            if file.is_empty() || !from.is_file() {
                continue;
            }
            let to = copy.join(file);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(&from, &to).unwrap();
        }
        repository_in(&copy);
        (copy, lock)
    }

    /// Makes `path` a git repository with one commit of everything in it.
    fn repository_in(path: &Path) {
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
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["add", "--all"]);
        git(&["commit", "--quiet", "--no-verify", "-m", "copy"]);
    }

    #[test]
    fn the_hooks_answer_when_the_main_workspace_does_not_resolve() {
        let (copy, _held) = copy_of_the_repository();
        // #20: a crate asks for a feature its dependency does not have, so the main workspace
        // cannot resolve and nothing in it builds.
        let manifest = copy.join("crates/abc-server/Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        let broken = text.replace(
            r#"abc-store = { path = "../abc-store", features = ["testing"] }"#,
            r#"abc-store = { path = "../abc-store", features = ["testing", "no-such-feature"] }"#,
        );
        assert_ne!(broken, text, "the dependency to break is still there");
        std::fs::write(&manifest, broken).unwrap();
        let metadata = Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--locked", "--offline"])
            .current_dir(&copy)
            .env_remove("CARGO")
            .output()
            .unwrap();
        assert!(
            !metadata.status.success(),
            "the main workspace must not resolve: {}",
            stderr(&metadata)
        );

        let force_push =
            r#"{"tool_name":"Bash","tool_input":{"command":"git push --force origin main"}}"#;
        let (pushed, push_answer, push_log) = launch(&copy, "pre-tool-use", force_push);
        assert_eq!(pushed, Some(0), "{push_log}");
        let denied: serde_json::Value = serde_json::from_str(&push_answer).unwrap();
        assert_eq!(
            denied["hookSpecificOutput"]["permissionDecision"], "deny",
            "{push_answer}"
        );
        let reason = denied["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("force-pushes") && !reason.contains("could not"),
            "the guard itself judged it: {reason}"
        );
        let status = r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
        let (listed, status_answer, status_log) = launch(&copy, "pre-tool-use", status);
        assert_eq!(
            (listed, status_answer.as_str()),
            (Some(0), ""),
            "allowed: {status_log}"
        );

        let stop = r#"{"hook_event_name":"Stop","stop_hook_active":false}"#;
        let (stopped, stop_answer, stop_log) = launch(&copy, "stop", stop);
        assert_eq!(stopped, Some(0), "{stop_log}");
        let held: serde_json::Value = serde_json::from_str(&stop_answer).unwrap();
        assert_eq!(held["decision"], "block", "{stop_answer}");
        let held_for = held["reason"].as_str().unwrap();
        assert!(
            held_for.contains("xtask stop hook: the check")
                && !held_for.contains("could not start"),
            "xtask's own stop hook held the stop, not the launcher's fallback: {held_for}"
        );
        assert!(
            copy.join(".abc/stop-state.json").is_file(),
            "and counted it in its state"
        );
    }
}
