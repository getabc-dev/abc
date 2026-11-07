use std::fs;
use std::path::{Path, PathBuf};

use super::{APPLY_PUSH, Id, Push, Session, branches, sealed, short, slug};
use crate::testing::{git, repository, script};

/// A main clone with an `origin/main` and a linked worktree on branch `feature`; returns the main
/// clone and the worktree.
fn clone_and_worktree(dir: &Path) -> (PathBuf, PathBuf) {
    let main = repository(&dir.join("main"));
    git(
        &main,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/getabc-dev/abc.git",
        ],
    );
    git(&main, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&main, &["config", "user.name", "Maintainer \"M\""]);
    let worktree = dir.join("Feature Tree");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            worktree.to_str().unwrap(),
        ],
    );
    (main, worktree.canonicalize().unwrap())
}

fn open(dir: &Path) -> (PathBuf, Session) {
    let (main, worktree) = clone_and_worktree(dir);
    let session = Session::open(&worktree, &main, &dir.join("state")).unwrap();
    (main, session)
}

/// Commits in the worktree as the VM does: through the session repository.
fn commit_in_vm(session: &Session, message: &str) {
    let admin = session.git_dir().join("worktrees").join(&session.id);
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=agent",
            "-c",
            "user.email=agent@example.com",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            message,
        ])
        .env("GIT_DIR", &admin)
        .env("GIT_WORK_TREE", &session.worktree)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn session_git(session: &Session, args: &[&str]) -> String {
    let output = sealed(&session.git_dir()).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn names_the_session_after_the_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let (main, session) = open(dir.path());
    assert_eq!(session.name, "abc-feature-tree");
    assert_eq!(session.state, dir.path().join("state/feature-tree"));
    assert_eq!(session.git_dir(), dir.path().join("state/feature-tree/git"));
    assert_eq!(session.common, main.join(".git"));
    assert_eq!(session.admin, main.join(".git/worktrees").join(&session.id));
}

#[test]
fn finds_the_worktree_through_the_main_clone_never_its_dot_git_file() {
    let dir = tempfile::tempdir().unwrap();
    let (main, worktree) = clone_and_worktree(dir.path());
    let evil = repository(&dir.path().join("evil"));
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", evil.join(".git").display()),
    )
    .unwrap();
    let session = Session::open(&worktree, &main, &dir.path().join("state")).unwrap();
    assert_eq!(
        session.common,
        main.join(".git"),
        "the main clone's, not the file's"
    );
    session.prepare_git().unwrap();
    commit_in_vm(&session, "in the vm");
    assert!(session.sync().unwrap().starts_with("feature: "));
    assert_eq!(
        git(&main, &["log", "-1", "--format=%s", "feature"]).trim(),
        "in the vm",
        "the main clone's branch moved, through its own administrative directory"
    );
    assert_eq!(
        git(&evil, &["log", "--format=%s"]).trim(),
        "start",
        "the decoy is untouched"
    );
}

#[test]
fn refuses_what_is_not_a_worktree_of_the_main_clone() {
    let dir = tempfile::tempdir().unwrap();
    let (main, worktree) = clone_and_worktree(dir.path());
    let state = dir.path().join("state");
    let not_listed = format!("{:#}", Session::open(&main, &main, &state).unwrap_err());
    assert!(
        not_listed.contains("administrative directory"),
        "{not_listed}"
    );
    let other = repository(&dir.path().join("other"));
    let foreign = format!(
        "{:#}",
        Session::open(&worktree, &other, &state).unwrap_err()
    );
    assert!(foreign.contains("is not a worktree of"), "{foreign}");
    let missing = format!(
        "{:#}",
        Session::open(&dir.path().join("gone"), &main, &state).unwrap_err()
    );
    assert!(missing.contains("does not exist"), "{missing}");
    let admin = main.join(".git/worktrees").join(
        fs::read_dir(main.join(".git/worktrees"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name(),
    );
    fs::write(
        admin.join("commondir"),
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    let mismatch = format!("{:#}", Session::open(&worktree, &main, &state).unwrap_err());
    assert!(mismatch.contains("does not belong to"), "{mismatch}");
}

#[test]
fn slugs_and_short_names() {
    assert_eq!(slug("S0 Micro_VM."), "s0-micro-vm");
    assert_eq!(slug("--a--"), "a");
    assert_eq!(short("0123456789abcdef"), "0123456789ab");
    assert_eq!(short("abc"), "abc");
}

#[test]
fn the_session_repository_holds_the_branch_and_origin_main_only() {
    let dir = tempfile::tempdir().unwrap();
    let (main, session) = open(dir.path());
    git(&main, &["branch", "other"]);
    assert_eq!(session.prepare_git().unwrap(), None);
    assert_eq!(
        session_git(&session, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/feature\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\n",
        "no other branch"
    );
    let config = fs::read_to_string(session.git_dir().join("config")).unwrap();
    assert!(
        config.contains("url = \"https://github.com/getabc-dev/abc.git\"")
            && config.contains("name = \"Maintainer \\\"M\\\"\""),
        "{config}"
    );
    let admin = session.git_dir().join("worktrees").join(&session.id);
    assert_eq!(
        fs::read_to_string(admin.join("HEAD")).unwrap(),
        "ref: refs/heads/feature\n"
    );
    assert_eq!(
        fs::read_to_string(admin.join("commondir")).unwrap(),
        "../..\n"
    );
    assert_eq!(
        fs::read_to_string(admin.join("gitdir")).unwrap(),
        format!("{}/.git\n", session.worktree.display())
    );
    assert!(
        admin.join("index").exists(),
        "an index for the worktree's files"
    );
}

/// The seal copied the host's `remote.origin.url` into the VM's repository. A maintainer whose
/// origin is SSH gave the VM a remote it cannot use: no key there, egress is HTTPS only, and the
/// App's credential helper answers only for `https://github.com`. The VM always gets the HTTPS URL.
#[test]
fn the_vm_always_gets_the_https_remote() {
    let dir = tempfile::tempdir().unwrap();
    let (main, session) = open(dir.path());
    for origin in [
        "git@github.com:getabc-dev/abc.git",
        "ssh://git@github.com/getabc-dev/abc",
        "https://github.com/getabc-dev/abc",
    ] {
        git(&main, &["remote", "set-url", "origin", origin]);
        session.prepare_git().unwrap();
        let config = fs::read_to_string(session.git_dir().join("config")).unwrap();
        assert!(
            config.contains("url = \"https://github.com/getabc-dev/abc.git\""),
            "{origin}: {config}"
        );
    }
    git(
        &main,
        &["remote", "set-url", "origin", "/somewhere/else.git"],
    );
    let error = format!("{:#}", session.prepare_git().unwrap_err());
    assert!(
        error.contains("origin is not a github.com repository: /somewhere/else.git"),
        "{error}"
    );
}

#[test]
fn nothing_the_vm_left_in_the_session_repository_runs_on_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    session.prepare_git().unwrap();
    let repo = session.git_dir();
    // The VM commits first; what it plants afterwards must not run on the host.
    commit_in_vm(&session, "in the vm");
    let ran = dir.path().join("ran");
    let payload = dir.path().join("payload");
    script(&payload, &format!("echo \"$0 $*\" >> {}\n", ran.display()));
    // What a VM could plant: hooks, a hooks path, an fsmonitor, an include, alternates, and
    // per-worktree config.
    for hook in [
        "reference-transaction",
        "post-checkout",
        "pre-auto-gc",
        "post-index-change",
    ] {
        script(
            &repo.join("hooks").join(hook),
            &format!("{} {hook}\n", payload.display()),
        );
    }
    let planted = dir.path().join("planted");
    fs::create_dir_all(&planted).unwrap();
    script(
        &planted.join("reference-transaction"),
        &format!("{}\n", payload.display()),
    );
    fs::write(
        dir.path().join("included"),
        format!("[core]\n\tfsmonitor = {}\n", payload.display()),
    )
    .unwrap();
    let planted_config = format!(
        "{}[core]\n\thooksPath = {}\n\tfsmonitor = {}\n\tsshCommand = {}\n[include]\n\tpath = {}\n\
         [gc]\n\tauto = 1\n[extensions]\n\tworktreeConfig = true\n",
        fs::read_to_string(repo.join("config")).unwrap(),
        planted.display(),
        payload.display(),
        payload.display(),
        dir.path().join("included").display()
    );
    fs::write(repo.join("config"), planted_config).unwrap();
    fs::write(
        repo.join("objects/info/alternates"),
        "/nonexistent/objects\n",
    )
    .unwrap();
    let admin = repo.join("worktrees").join(&session.id);
    fs::write(
        admin.join("config.worktree"),
        format!("[core]\n\thooksPath = {}\n", planted.display()),
    )
    .unwrap();

    session.sync().unwrap();
    session.prepare_git().unwrap();
    session.sync().unwrap();
    assert!(!ran.exists(), "nothing ran: {:?}", fs::read_to_string(&ran));
    let sealed_config = fs::read_to_string(repo.join("config")).unwrap();
    assert!(
        !sealed_config.contains("hooksPath") && !sealed_config.contains("include"),
        "{sealed_config}"
    );
    assert_eq!(
        fs::read_dir(repo.join("hooks")).unwrap().count(),
        0,
        "hooks emptied"
    );
    assert!(!repo.join("objects/info/alternates").exists());
    assert!(!admin.join("config.worktree").exists());
}

#[test]
fn replaces_symlinks_the_vm_left_without_following_them() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    session.prepare_git().unwrap();
    let admin = session.git_dir().join("worktrees").join(&session.id);
    let victim = dir.path().join("victim");
    fs::write(&victim, "keep\n").unwrap();
    for file in ["HEAD", "commondir", "gitdir", "index"] {
        fs::remove_file(admin.join(file)).unwrap();
        std::os::unix::fs::symlink(&victim, admin.join(file)).unwrap();
    }
    fs::remove_file(session.git_dir().join("config")).unwrap();
    std::os::unix::fs::symlink(&victim, session.git_dir().join("config")).unwrap();
    session.prepare_git().unwrap();
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "keep\n",
        "never written through"
    );
    for file in ["HEAD", "commondir", "gitdir", "index", "../../config"] {
        let meta = fs::symlink_metadata(admin.join(file)).unwrap();
        assert!(meta.is_file(), "{file} is a plain file again");
    }
    // A whole administrative directory turned into a symlink.
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_dir_all(&admin).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &admin).unwrap();
    session.prepare_git().unwrap();
    assert!(fs::symlink_metadata(&admin).unwrap().is_dir());
    assert_eq!(
        fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing written there"
    );
}

#[test]
fn commits_made_in_the_vm_come_back_and_are_never_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    let worktree = session.worktree.clone();
    session.prepare_git().unwrap();
    assert!(
        session.sync().unwrap().contains("already at"),
        "nothing to take yet"
    );
    commit_in_vm(&session, "in the vm");
    let vm_head = session_git(&session, &["rev-parse", "refs/heads/feature"]);
    let note = session.prepare_git().unwrap().unwrap();
    assert!(note.contains("cargo xtask vm sync"), "{note}");
    assert_eq!(
        session_git(&session, &["rev-parse", "refs/heads/feature"]),
        vm_head,
        "a restart keeps the VM's commit"
    );
    let moved = session.sync().unwrap();
    assert!(moved.starts_with("feature: "), "{moved}");
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), vm_head);
    assert_eq!(
        git(&worktree, &["status", "--porcelain"]),
        "",
        "index and files agree"
    );
    git(
        &worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "on the host"],
    );
    assert_eq!(
        session.prepare_git().unwrap(),
        None,
        "the host's commit goes forward"
    );
    assert_eq!(
        session_git(&session, &["rev-parse", "refs/heads/feature"]),
        git(&worktree, &["rev-parse", "HEAD"])
    );
}

#[test]
fn diverged_branches_are_left_to_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    session.prepare_git().unwrap();
    commit_in_vm(&session, "in the vm");
    git(
        &session.worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "on the host"],
    );
    let error = format!("{:#}", session.sync().unwrap_err());
    assert!(error.contains("merge them by hand"), "{error}");
}

/// Runs [`APPLY_PUSH`] on `bundle` as the VM does, in the worktree through the session
/// repository; returns whether git took it and what it said.
fn apply_in_vm(session: &Session, branch: &str, bundle: &[u8]) -> (bool, String) {
    use std::io::Write as _;
    let admin = session.git_dir().join("worktrees").join(&session.id);
    let mut child = std::process::Command::new("sh")
        .args(["-c", APPLY_PUSH, "sh", branch])
        .env("GIT_DIR", &admin)
        .env("GIT_WORK_TREE", &session.worktree)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(&session.worktree)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bundle).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The bundle a push must produce, with the commits it moves between.
fn bundle(push: Push) -> (String, String, String, Vec<u8>) {
    match push {
        Push::Bundle {
            branch,
            from,
            to,
            bundle,
        } => (branch, from, to, bundle),
        Push::Current(note) => panic!("nothing to push: {note}"),
    }
}

#[test]
fn commits_made_on_the_host_go_into_the_vm_only_forward() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    let worktree = session.worktree.clone();
    let error = format!("{:#}", session.push().unwrap_err());
    assert!(
        error.contains("`cargo xtask vm start` creates it"),
        "{error}"
    );
    session.prepare_git().unwrap();
    let vm_head = session_git(&session, &["rev-parse", "refs/heads/feature"]);
    match session.push().unwrap() {
        Push::Current(note) => assert_eq!(
            note,
            format!("the VM's feature is already at {}", short(vm_head.trim()))
        ),
        Push::Bundle { .. } => panic!("nothing is new yet"),
    }
    fs::write(worktree.join("host.txt"), "from the host\n").unwrap();
    git(&worktree, &["add", "host.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "on the host"]);
    let host_head = git(&worktree, &["rev-parse", "HEAD"]);
    let (branch, from, to, bytes) = bundle(session.push().unwrap());
    assert_eq!(
        (branch.as_str(), from.as_str(), to.as_str()),
        ("feature", vm_head.trim(), host_head.trim())
    );
    assert_eq!(
        session_git(&session, &["rev-parse", "refs/heads/feature"]),
        vm_head,
        "building the push writes nothing to the session repository"
    );
    let (took, said) = apply_in_vm(&session, &branch, &bytes);
    assert!(took, "{said}");
    assert_eq!(
        session_git(&session, &["rev-parse", "refs/heads/feature"]),
        host_head
    );
    let admin = session.git_dir().join("worktrees").join(&session.id);
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .env("GIT_DIR", &admin)
        .env("GIT_WORK_TREE", &worktree)
        .current_dir(&worktree)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "",
        "the VM's index follows the new commit"
    );
    assert!(matches!(session.push().unwrap(), Push::Current(_)));

    git(
        &worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "second"],
    );
    let (_, _, _, stale) = bundle(session.push().unwrap());
    commit_in_vm(&session, "meanwhile in the vm");
    let (refused, why) = apply_in_vm(&session, "feature", &stale);
    assert!(!refused, "the VM's git moves its branch only forward");
    assert!(why.contains("non-fast-forward"), "{why}");
    let ahead = format!("{:#}", session.push().unwrap_err());
    assert!(
        ahead.contains("has commits the worktree's lacks; `cargo xtask vm sync`"),
        "{ahead}"
    );
}

#[test]
fn a_push_refuses_branches_that_diverged() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    let worktree = session.worktree.clone();
    session.prepare_git().unwrap();
    git(
        &worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "on the host"],
    );
    let (branch, _, _, bytes) = bundle(session.push().unwrap());
    assert!(apply_in_vm(&session, &branch, &bytes).0);
    git(
        &worktree,
        &[
            "commit",
            "--quiet",
            "--amend",
            "--allow-empty",
            "-m",
            "rewritten",
        ],
    );
    let diverged = format!("{:#}", session.push().unwrap_err());
    assert!(
        diverged.contains("is not behind the worktree's")
            && diverged.contains("merge them by hand"),
        "{diverged}"
    );
    git(&worktree, &["checkout", "--quiet", "--detach"]);
    let detached = format!("{:#}", session.push().unwrap_err());
    assert!(detached.contains("on no branch"), "{detached}");
}

#[test]
fn a_detached_worktree_gets_its_commit_but_no_sync() {
    let dir = tempfile::tempdir().unwrap();
    let (_, session) = open(dir.path());
    git(&session.worktree, &["checkout", "--quiet", "--detach"]);
    session.prepare_git().unwrap();
    let head = git(&session.worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        fs::read_to_string(
            session
                .git_dir()
                .join("worktrees")
                .join(&session.id)
                .join("HEAD")
        )
        .unwrap(),
        head
    );
    let error = format!("{:#}", session.sync().unwrap_err());
    assert!(error.contains("on no branch"), "{error}");
}

#[test]
fn what_cannot_be_inspected_is_an_error_not_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    // Beneath a regular file, a path is not missing but cannot be looked at.
    let beneath = file.join("sub");
    let inspect = format!("{:#}", super::plain_dir(&beneath).unwrap_err());
    assert!(inspect.starts_with("could not inspect "), "{inspect}");
    let remove = format!("{:#}", super::remove_any(&beneath).unwrap_err());
    assert!(remove.starts_with("could not remove "), "{remove}");
    super::remove_any(&dir.path().join("missing")).unwrap();
}

/// A session's name and state come from its slug alone. Stripping every leading `abc-` from a
/// name gave the session of worktree `abc-cli` the state of worktree `cli`'s, and the session of
/// worktree `abc` the state root itself.
#[test]
fn a_sessions_name_and_state_come_from_its_slug() {
    let root = Path::new("/state");
    for (worktree, name, state) in [
        ("abc", "abc-abc", "/state/abc"),
        ("abc-cli", "abc-abc-cli", "/state/abc-cli"),
        ("My Change!", "abc-my-change", "/state/my-change"),
    ] {
        let id = Id::of_worktree(worktree).unwrap();
        assert_eq!(id.name(), name, "{worktree}");
        assert_eq!(id.state(root), Path::new(state), "{worktree}");
        assert_eq!(Id::of_name(name), Some(id.clone()), "{name}");
        let slug = state.strip_prefix("/state/").unwrap();
        assert_eq!(Id::of_slug(slug), Some(id), "{slug}");
    }
    assert_eq!(Id::of_worktree("--"), None);
    for name in ["abc-", "cli", "abc-Cli", "abc--x", "abc-x-", "abc-a/b"] {
        assert_eq!(Id::of_name(name), None, "{name}");
    }
    for slug in ["", ".", "..", "-x", "x-", "A", "a b"] {
        assert_eq!(Id::of_slug(slug), None, "{slug:?}");
    }
}

/// A session directory whose `git` is not a repository must not read the repository around it:
/// git searched up from the directory it was given.
#[test]
fn sealed_git_reads_only_the_repository_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let around = repository(&dir.path().join("around"));
    let state = around.join("state/s");
    fs::create_dir_all(state.join("git")).unwrap();
    let error = format!("{:#}", branches(&state).unwrap_err());
    assert!(error.contains("not a git repository"), "{error}");
}
