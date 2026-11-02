//! Helpers for tests: real git repositories, an HTTP server that never leaves the process, and
//! scripts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs git in `dir`, isolated from the user's and the system's git configuration, and returns
/// what it prints.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Creates a repository at `path` with one empty commit on `main` and returns its canonical path.
pub(crate) fn repository(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet"]);
    git(path, &["commit", "--quiet", "--allow-empty", "-m", "start"]);
    path.canonicalize().unwrap()
}

/// The requests a [`fake_http`] agent has received, each as HTTP/1.1 text: the request line, the
/// headers its caller set (names in lowercase, as ureq sends them) and the body.
#[derive(Clone)]
pub(crate) struct Requests(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Requests {
    /// The requests so far, in order.
    pub(crate) fn taken(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// An agent built from `config` whose requests never leave the process: each one is recorded and
/// answered with the next of `responses` (a status such as 201, and a JSON body). A 4xx or 5xx
/// answer is an error when the configuration says so, as it would be from a real server. Tests
/// need no port, so they pass inside Claude Code's sandbox, which forbids binding one.
pub(crate) fn fake_http(
    config: ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
    responses: Vec<(u16, &'static str)>,
) -> (ureq::Agent, Requests) {
    use std::fmt::Write as _;
    use std::io::Read as _;
    use std::sync::{Arc, Mutex, OnceLock};

    let requests = Requests(Arc::new(Mutex::new(Vec::new())));
    let recorded = requests.clone();
    let queue = Mutex::new(std::collections::VecDeque::from(responses));
    let status_as_error = Arc::new(OnceLock::new());
    let raises = Arc::clone(&status_as_error);
    let answer = move |request: ureq::http::Request<ureq::SendBody<'_>>,
                       _next: ureq::middleware::MiddlewareNext<'_>|
          -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let (parts, sent) = request.into_parts();
        let mut text = format!(
            "{} {} HTTP/1.1\r\n",
            parts.method,
            parts.uri.path_and_query().map_or("/", |path| path.as_str())
        );
        for (name, value) in &parts.headers {
            write!(text, "{name}: {}\r\n", value.to_str().unwrap()).unwrap();
        }
        text.push_str("\r\n");
        sent.into_reader().read_to_string(&mut text).unwrap();
        recorded.0.lock().unwrap().push(text);
        let (status, body) = queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("a response for every request");
        if status >= 400 && *raises.get().unwrap() {
            return Err(ureq::Error::StatusCode(status));
        }
        Ok(ureq::http::Response::builder()
            .status(status)
            .body(
                ureq::Body::builder()
                    .mime_type("application/json")
                    .data(body),
            )
            .unwrap())
    };
    let agent: ureq::Agent = config.middleware(answer).build().into();
    status_as_error
        .set(agent.config().http_status_as_error())
        .unwrap();
    (agent, requests)
}

/// Writes an executable shell script at `path`.
pub(crate) fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// A repository with a `base` branch, then a commit that adds `files` (path and content).
pub(crate) fn repository_with_change(path: &Path, files: &[(&str, &str)]) -> PathBuf {
    let repo = repository(path);
    git(&repo, &["branch", "base"]);
    for (file, content) in files {
        let target = repo.join(file);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "-m", "change"]);
    repo
}
