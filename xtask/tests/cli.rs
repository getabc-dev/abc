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

    #[test]
    fn the_vm_guard_answers_when_the_workspace_does_not_build() {
        // A project whose workspace cannot resolve, and a cargo that fails if anything runs it.
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/broken\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join("crates/broken")).unwrap();
        std::fs::write(
            project.path().join("crates/broken/Cargo.toml"),
            "[package]\nname = \"broken\"\n[dependencies]\nabc-server = { workspace = true, features = [\"nope\"] }\n",
        )
        .unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = |name: &str, body: String| {
            let path = bin.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        };
        script("cargo", "echo 'cargo ran' >&2\nexit 101\n".to_owned());
        // The image's xtask: the real binary, past the billing check that needs a real VM.
        script(
            "abc-xtask",
            format!(
                "[ \"$1\" = vm ] && exit 0\nexec {} \"$@\"\n",
                env!("CARGO_BIN_EXE_xtask")
            ),
        );
        let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks/pre-tool-use.sh");
        let run = |command: &str| {
            let input = format!(
                r#"{{"cwd":"{}","tool_name":"Bash","tool_input":{{"command":"{command}"}}}}"#,
                project.path().display()
            );
            let mut child = Command::new("sh")
                .arg(&hook)
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        bin.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .env("ABC_VM", "1")
                .env("CLAUDE_PROJECT_DIR", project.path())
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
            (output.status.code(), stderr(&output))
        };
        let (code, said) = run("git push --force origin agent/x");
        assert_eq!(code, Some(2), "the guard still blocks a force push: {said}");
        assert!(!said.contains("cargo ran"), "nothing built: {said}");
        let (allowed, also) = run("git status");
        assert_eq!(allowed, Some(0), "and allows what is safe: {also}");
        assert!(!also.contains("cargo ran"), "nothing built: {also}");
    }
}
