//! Helpers for tests that need real git repositories.

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
