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

/// Serves one HTTP request on a local port with `status` (such as `201 Created`) and `body`, and
/// returns the server's base URL and a handle that yields the request it received.
pub(crate) fn serve_once(
    status: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<String>) {
    let (url, server) = serve(vec![(status, body)]);
    (
        url,
        std::thread::spawn(move || server.join().unwrap().remove(0)),
    )
}

/// Serves one HTTP request per response in `responses`, in order, on a local port, and returns
/// the server's base URL and a handle that yields the requests it received.
pub(crate) fn serve(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        // A request that never comes fails the test instead of hanging it.
        listener.set_nonblocking(true).unwrap();
        let started = std::time::Instant::now();
        let mut requests = Vec::new();
        for (status, body) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < std::time::Duration::from_secs(10) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("no request arrived: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (url, server)
}

/// Writes an executable shell script at `path`.
pub(crate) fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// A clone at `temp/work` of a bare `temp/origin.git`, whose one commit is `origin/main`, the
/// default branch.
pub(crate) fn clone_with_origin(temp: &Path) -> PathBuf {
    let work = repository(&temp.join("work"));
    let bare = temp.join("origin.git");
    git(temp, &["init", "--quiet", "--bare", bare.to_str().unwrap()]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    git(&work, &["fetch", "--quiet", "origin"]);
    git(&work, &["remote", "set-head", "origin", "main"]);
    work
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

/// The checkout the tests read around xtask: the one xtask was built from, or, when xtask's tests
/// run in a copy of `xtask/` alone, as `cargo xtask mutants` runs them, the checkout that names
/// at run time in [`crate::mutants::TEST_REPOSITORY`]. Only tests read it; the shipped binary
/// finds its checkout from where it was built alone.
pub(crate) fn checkout() -> PathBuf {
    checkout_named(std::env::var_os(crate::mutants::TEST_REPOSITORY))
        .unwrap_or_else(|wrong| panic!("{wrong}"))
}

/// The checkout the variable's `value` names, which must be an absolute path: a test runs from
/// its crate's directory, so a relative one would name another place than the one meant. Without a
/// value, the checkout xtask was built from.
fn checkout_named(value: Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    let Some(value) = value else {
        return crate::repo_root().map_err(|error| format!("{error:#}"));
    };
    let path = PathBuf::from(value);
    if path.is_relative() {
        return Err(format!(
            "{} is `{}`, a relative path, and the tests run from another directory: give an \
             absolute one",
            crate::mutants::TEST_REPOSITORY,
            path.display()
        ));
    }
    Ok(path)
}

/// The file at `path` in this repository, for a test that checks it. Read at run time from
/// [`checkout`], never with `include_str!`, so the tests build in a copy of xtask/ alone.
pub(crate) fn read_in_repository(path: &str) -> String {
    let file = checkout().join(path);
    fs::read_to_string(&file).unwrap_or_else(|error| panic!("{}: {error}", file.display()))
}

/// The pinned tool `name` of this repository, for a test that runs it. It must be marked
/// `for-tests` in xtask/tools.toml, or CI's test jobs would not install it.
pub(crate) fn pinned_for_tests(name: &str) -> PathBuf {
    crate::tools::for_tests(&checkout(), name).unwrap_or_else(|error| panic!("{error:#}"))
}

/// A fake node and pnpm in `dir`.
///
/// Models: node and pnpm as `web` runs them. Environment: pnpm needs node's directory first on
/// `PATH`, as the real one does, and checks what `cargo xtask e2e` gives Playwright. Streams: it
/// prints nothing and logs its arguments to a file. Exit codes: 0, or 1 for a call whose arguments
/// hold `fail`. Its tests are those of every gate that uses it.
///
/// pnpm logs each call's arguments to `dir/pnpm.log`, fails a call whose arguments contain `fail`
/// (when it is not empty), refuses to run unless node's directory comes first on `PATH`, writes
/// `web/dist/index.html` for the build and a Vitest lcov for the coverage run, and checks the
/// environment `cargo xtask e2e` gives Playwright.
pub(crate) fn fake_web(dir: &Path, fail: &str) -> crate::web::Toolchain {
    fs::create_dir_all(dir).unwrap();
    script(&dir.join("node"), "exit 0\n");
    script(
        &dir.join("pnpm"),
        &format!(
            "echo \"$*\" >> {log}\n\
             case \"$PATH\" in {dir}:*) ;; *) echo 'node is not first on PATH' >&2; exit 90 ;; esac\n\
             if [ -n '{fail}' ]; then case \"$*\" in *'{fail}'*) exit 1 ;; esac; fi\n\
             case \"$*\" in\n\
               'run --silent build') mkdir -p dist && echo '<!doctype html>' > dist/index.html ;;\n\
               'run --silent coverage') mkdir -p coverage && printf 'SF:src/a.ts\\nDA:1,1\\nDA:2,0\\nend_of_record\\n' > coverage/lcov.info ;;\n\
               'exec playwright test') [ -n \"$ABC_E2E_SOCKET\" ] || exit 91; [ \"$ABC_E2E_ORIGIN\" = https://localhost:4173 ] || exit 92;\n\
                 head -n 1 \"$ABC_E2E_TLS_CERT\" | grep -qx -- '-----BEGIN CERTIFICATE-----' || exit 93;\n\
                 head -n 1 \"$ABC_E2E_TLS_KEY\" | grep -qx -- '-----BEGIN PRIVATE KEY-----' || exit 94 ;;\n\
             esac\n",
            log = dir.join("pnpm.log").display(),
            dir = dir.display(),
        ),
    );
    crate::web::Toolchain::pinned(&|name| Ok(dir.join(name))).unwrap()
}

/// What the fake pnpm of [`fake_web`] in `dir` was asked, a call per line.
pub(crate) fn pnpm_calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("pnpm.log")).unwrap_or_default()
}

/// A tree at `root` with a web app (`web/package.json`) and a Cargo package named `abc` whose
/// binary stands in for `abc server`: it answers every request on `ABC_OPS_ADDR` with a 200. Its one
/// test passes, so `cargo xtask check` has a test to run.
pub(crate) fn abc_tree(root: &Path) {
    fs::create_dir_all(root.join("web")).unwrap();
    fs::write(root.join("web/package.json"), "{}\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"abc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual \
         editing.\nversion = 4\n\n[[package]]\nname = \"abc\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::copy(
        checkout().join("rust-toolchain.toml"),
        root.join("rust-toolchain.toml"),
    )
    .unwrap();
    fs::create_dir_all(root.join(".config")).unwrap();
    fs::write(
        root.join(crate::check::NEXTEST_CONFIG),
        read_in_repository(crate::check::NEXTEST_CONFIG),
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        "use std::io::{Read as _, Write as _};\n\nfn main() {\n    let ops = std::env::var(\"ABC_OPS_ADDR\").unwrap_or_default();\n    let Ok(listener) = std::net::TcpListener::bind(&ops) else {\n        return;\n    };\n    for mut stream in listener.incoming().flatten() {\n        let mut request = [0_u8; 1024];\n        let _read = stream.read(&mut request);\n        let _written = stream.write_all(b\"HTTP/1.1 200 OK\\r\\nContent-Length: 0\\r\\n\\r\\n\");\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn builds() {}\n}\n",
    )
    .unwrap();
}

/// A temporary directory that is removed even when `spec::Workspace::create` made a read-only
/// Quint home in it, and even when the test fails: a plain `TempDir` cannot remove one.
pub(crate) struct Scratch(tempfile::TempDir);

impl Scratch {
    pub(crate) fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let removed = crate::spec::remove_tree(&crate::web::target_dir(self.0.path()).join("spec"));
        // A second panic while a failing test unwinds would abort the run and hide its message.
        if !std::thread::panicking() {
            removed.unwrap();
        }
    }
}

#[test]
fn the_checkout_variable_must_be_an_absolute_path_and_a_relative_one_is_named() {
    let error = checkout_named(Some("../abc".into())).unwrap_err();
    assert!(
        error.contains("XTASK_TEST_REPOSITORY is `../abc`, a relative path"),
        "{error}"
    );
    assert_eq!(
        checkout_named(Some("/work/abc".into())).unwrap(),
        PathBuf::from("/work/abc")
    );
    assert_eq!(checkout_named(None).unwrap(), crate::repo_root().unwrap());
}
