//! A session: one worktree of the main clone, its VM's name, and its own git repository.
//!
//! The VM never sees the main clone. Each session gets a bare repository of its own under the
//! state directory holding only the worktree's branch and `origin/main`, laid out as the common
//! directory of a linked worktree. The VM mounts it at [`GUEST_GIT`], and its entrypoint links the
//! main clone's git directory path there, where the worktree's `.git` file points, so git inside
//! works unchanged. Commits made in the VM come back with [`Session::sync`]; commits made on the
//! host go into a running VM with [`Session::push`].
//!
//! Everything the VM can write is hostile to the host: the worktree's files, its `.git` file, and
//! the session repository with its config, hooks and administrative files. So the host
//!
//! - finds the worktree through the main clone (`git worktree list --porcelain` and the main
//!   clone's own administrative directory), never through the worktree's `.git` file, and runs git
//!   on the worktree with an explicit `GIT_DIR` and `GIT_WORK_TREE`;
//! - runs git on the session repository only through [`sealed`], with no system or global config
//!   and every setting that runs a program pinned off, and rewrites that repository's config,
//!   empties its hooks and drops its alternates before it uses it;
//! - writes the session repository's files only while the VM is stopped, replacing whatever the VM
//!   left there, symlinks included, without following it. While it runs, the host only reads the
//!   repository; commits it pushes travel as a bundle the VM's own git fetches ([`APPLY_PUSH`]).

use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail, ensure};

/// Where the VM sees the session repository.
pub(crate) const GUEST_GIT: &str = "/abc/git";

/// Settings that make git run a program, pinned off for every command on the session repository,
/// on top of ignoring the system and global config. Command-line settings win over the
/// repository's own config, and git passes them on to the commands it starts, such as
/// upload-pack.
const SEALED: [&str; 17] = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.sshCommand=false",
    "core.gitProxy=",
    "core.askPass=",
    "core.pager=cat",
    "core.editor=false",
    "credential.helper=",
    "gc.auto=0",
    "maintenance.auto=false",
    "protocol.allow=never",
    "protocol.file.allow=always",
    "uploadpack.packObjectsHook=",
    "transfer.fsckObjects=true",
    "fetch.fsckObjects=true",
    "submodule.recurse=false",
    "fetch.recurseSubmodules=false",
];

/// What the VM runs, as the agent in the worktree, to take a [`Push`]: the bundle on stdin, the
/// branch as `$1`. Without `+` git moves the branch only forward, and refuses when the VM's branch
/// moved meanwhile. The files are already the host's, so the index follows the new commit.
pub(crate) const APPLY_PUSH: &str = "set -eu
bundle=$(mktemp)
trap 'rm -f \"$bundle\"' EXIT
cat > \"$bundle\"
git fetch --no-tags --update-head-ok \"$bundle\" \"refs/heads/$1:refs/heads/$1\"
git reset --quiet
";

/// Commits of the worktree's branch for a running VM, from [`Session::push`].
#[derive(Debug)]
pub(crate) enum Push {
    /// The VM's branch already has them; what to tell the user.
    Current(String),
    /// A bundle of the commits the VM's branch lacks, for [`APPLY_PUSH`].
    Bundle {
        branch: String,
        /// The VM's commit and the worktree's.
        from: String,
        to: String,
        bundle: Vec<u8>,
    },
}

/// git for a repository the VM can write: no system or global config, no inherited git
/// environment, and [`SEALED`] settings. The repository is `dir` itself (`GIT_DIR`), never one
/// git finds by searching up from it; a caller working on a worktree sets its own `GIT_DIR`.
pub(crate) fn sealed(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_DIR", dir)
        .arg("-C")
        .arg(dir);
    for setting in SEALED {
        command.args(["-c", setting]);
    }
    command
}

/// Runs `command` and returns what it prints, failing with git's message.
fn run(mut command: Command, what: &str) -> anyhow::Result<String> {
    let output = command.output().context("could not run git")?;
    ensure!(
        output.status.success(),
        "git {what} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git printed text that is not UTF-8")
}

/// What names a session: the slug of its worktree's directory name. Its VM's name, its volumes'
/// names and its state directory all derive from it here, and nowhere else.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Id(String);

/// What every session's VM name starts with.
const PREFIX: &str = "abc-";

impl Id {
    /// The session of a worktree whose directory is named `name`; none when the name has no
    /// letters or digits.
    pub(crate) fn of_worktree(name: &str) -> Option<Self> {
        Self::of_slug(&slug(name))
    }

    /// The session whose slug is `slug`, as its state directory is named; none unless `slug` is
    /// one: lowercase letters, digits and inner dashes.
    pub(crate) fn of_slug(slug: &str) -> Option<Self> {
        (!slug.is_empty() && self::slug(slug) == slug).then(|| Self(slug.to_owned()))
    }

    /// The session whose VM is named `name`; none unless it is one.
    pub(crate) fn of_name(name: &str) -> Option<Self> {
        name.strip_prefix(PREFIX).and_then(Self::of_slug)
    }

    /// Its VM's name, which also prefixes its volumes' names and its broker's files.
    pub(crate) fn name(&self) -> String {
        format!("{PREFIX}{}", self.0)
    }

    /// Its own directory under the state root `root`.
    pub(crate) fn state(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }
}

/// One agent session's names and places.
#[derive(Debug)]
pub(crate) struct Session {
    /// The worktree, canonical.
    pub(crate) worktree: PathBuf,
    /// The VM's name, and the prefix of its volumes' names.
    pub(crate) name: String,
    /// The session's own directory under the state root.
    pub(crate) state: PathBuf,
    /// The main clone's git directory, where the worktree's `.git` file leads.
    pub(crate) common: PathBuf,
    /// The worktree's administrative directory in the main clone.
    admin: PathBuf,
    /// The worktree's name in the main clone (`.git/worktrees/<id>`).
    id: String,
}

impl Session {
    /// The session for `worktree`, a linked worktree of the main clone `main`, keeping its state
    /// under `state_root`.
    pub(crate) fn open(worktree: &Path, main: &Path, state_root: &Path) -> anyhow::Result<Self> {
        let worktree = fs::canonicalize(worktree)
            .with_context(|| format!("{} does not exist", worktree.display()))?;
        let common = fs::canonicalize(main.join(".git"))
            .with_context(|| format!("{} has no .git directory", main.display()))?;
        ensure!(common.is_dir(), "{} is not a main clone", main.display());
        let listing = run(
            {
                let mut command = Command::new("git");
                command
                    .arg("-C")
                    .arg(main)
                    .args(["worktree", "list", "--porcelain"]);
                command
            },
            "worktree list",
        )?;
        ensure!(
            listing
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .any(|path| fs::canonicalize(path).is_ok_and(|path| path == worktree)),
            "{} is not a worktree of {}; agent sessions run in worktrees made with `git worktree \
             add` from the main clone",
            worktree.display(),
            main.display()
        );
        let pointer = worktree.join(".git");
        let admin = fs::read_dir(common.join("worktrees"))
            .with_context(|| format!("{} has no worktrees", main.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|admin| {
                fs::read_to_string(admin.join("gitdir"))
                    .is_ok_and(|gitdir| Path::new(gitdir.trim_end()) == pointer)
            })
            .with_context(|| {
                format!(
                    "the main clone has no administrative directory for {}",
                    worktree.display()
                )
            })?;
        let named = fs::read_to_string(admin.join("commondir"))
            .with_context(|| format!("{} has no commondir", admin.display()))?;
        ensure!(
            fs::canonicalize(admin.join(named.trim_end())).is_ok_and(|dir| dir == common),
            "{} does not belong to {}",
            admin.display(),
            common.display()
        );
        let id = name_of(&admin)?;
        let session = Id::of_worktree(&name_of(&worktree)?).with_context(|| {
            format!(
                "{} has no letters or digits to name a VM after",
                worktree.display()
            )
        })?;
        Ok(Self {
            name: session.name(),
            state: session.state(state_root),
            worktree,
            common,
            admin,
            id,
        })
    }

    /// The session's own git repository.
    pub(crate) fn git_dir(&self) -> PathBuf {
        self.state.join("git")
    }

    /// git on the worktree, through the main clone's administrative directory: never through the
    /// worktree's `.git` file, which the VM can rewrite.
    fn worktree_git(&self) -> Command {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_DIR", &self.admin)
            .env("GIT_WORK_TREE", &self.worktree)
            .current_dir(&self.worktree);
        command
    }

    /// The branch checked out in the worktree, or none when its HEAD is detached, and its commit.
    fn head(&self) -> anyhow::Result<(Option<String>, String)> {
        let mut symbolic = self.worktree_git();
        symbolic.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
        let output = symbolic.output().context("could not run git")?;
        let branch = output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        let mut parse = self.worktree_git();
        parse.args(["rev-parse", "HEAD"]);
        Ok((branch, run(parse, "rev-parse")?.trim().to_owned()))
    }

    /// Creates or refreshes the session's repository from the worktree, while the VM is stopped:
    /// its config, hooks, alternates and the worktree's administrative files are put back to what
    /// the host wrote, and its branch moves forward to the worktree's commit when it can, never
    /// back, so commits made in the VM are kept. Returns a note for the user when the VM's branch
    /// has commits the worktree lacks.
    pub(crate) fn prepare_git(&self) -> anyhow::Result<Option<String>> {
        let repo = self.git_dir();
        plain_dir(&self.state)?;
        let fresh = !repo.exists();
        if fresh {
            plain_dir(&repo)?;
            let mut init = sealed(&repo);
            init.args(["init", "--quiet", "--bare"]);
            run(init, "init")?;
        }
        plain_dir(&repo)?;
        self.seal(&repo)?;
        let worktree = self.worktree.as_os_str();
        if fresh {
            let mut fetch = sealed(&repo);
            fetch
                .args(["fetch", "--quiet", "--no-tags"])
                .arg(&self.common)
                .arg("+refs/remotes/origin/main:refs/remotes/origin/main");
            run(fetch, "fetch")?;
            let mut origin_head = sealed(&repo);
            origin_head.args([
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ]);
            run(origin_head, "symbolic-ref")?;
        }
        let admin = repo.join("worktrees").join(&self.id);
        plain_dir(&repo.join("worktrees"))?;
        plain_dir(&admin)?;
        write_plain(&admin.join("commondir"), "../..\n")?;
        let mut gitdir = worktree.to_owned();
        gitdir.push("/.git\n");
        write_plain(&admin.join("gitdir"), &gitdir.to_string_lossy())?;
        let (branch, head) = self.head()?;
        let mut note = None;
        if let Some(branch) = &branch {
            // Without `+`, git refuses to move the branch back: a fast-forward or nothing.
            let mut fetch = sealed(&repo);
            fetch
                .args(["fetch", "--quiet", "--update-head-ok", "--no-tags"])
                .arg(&self.admin)
                .arg(format!("{head}:refs/heads/{branch}"));
            if run(fetch, "fetch").is_err() {
                note = Some(format!(
                    "the VM's {branch} has commits the worktree's lacks; `cargo xtask vm sync` \
                     takes them"
                ));
            }
            write_plain(&admin.join("HEAD"), &format!("ref: refs/heads/{branch}\n"))?;
        } else {
            let mut fetch = sealed(&repo);
            fetch
                .args(["fetch", "--quiet", "--no-tags"])
                .arg(&self.admin)
                .arg(&head);
            run(fetch, "fetch")?;
            write_plain(&admin.join("HEAD"), &format!("{head}\n"))?;
        }
        remove_any(&admin.join("index"))?;
        let mut read_tree = sealed(&repo);
        read_tree
            .env("GIT_DIR", &admin)
            .env("GIT_WORK_TREE", &self.worktree)
            .args(["read-tree", "HEAD"]);
        run(read_tree, "read-tree")?;
        Ok(note)
    }

    /// Puts the session repository's config back to the host's, empties its hooks and drops its
    /// alternates and any per-worktree config, so nothing the VM wrote there runs on the host.
    fn seal(&self, repo: &Path) -> anyhow::Result<()> {
        let origin = git_main(&self.common, &["config", "remote.origin.url"]).unwrap_or_default();
        let mut config = format!(
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\
             [remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            quoted(origin.trim())
        );
        let identity: Vec<(&str, String)> = ["name", "email"]
            .into_iter()
            .filter_map(|key| {
                git_main(&self.common, &["config", &format!("user.{key}")])
                    .ok()
                    .map(|value| (key, value.trim().to_owned()))
            })
            .collect();
        if !identity.is_empty() {
            config.push_str("[user]\n");
            for (key, value) in identity {
                config.push('\t');
                config.push_str(key);
                config.push_str(" = ");
                config.push_str(&quoted(&value));
                config.push('\n');
            }
        }
        write_plain(&repo.join("config"), &config)?;
        let hooks = repo.join("hooks");
        remove_any(&hooks)?;
        fs::create_dir(&hooks).with_context(|| format!("could not create {}", hooks.display()))?;
        for leftover in [
            repo.join("objects/info/alternates"),
            repo.join("objects/info/http-alternates"),
            repo.join("info/attributes"),
            repo.join("worktrees")
                .join(&self.id)
                .join("config.worktree"),
            repo.join("config.worktree"),
        ] {
            remove_any(&leftover)?;
        }
        Ok(())
    }

    /// Moves the worktree's branch to the VM's commit on it, when that only adds commits. The
    /// files are already the VM's, so the index follows and the files stay as they are. Returns
    /// what changed, for the user.
    pub(crate) fn sync(&self) -> anyhow::Result<String> {
        let repo = self.git_dir();
        let (branch, ours) = self.head()?;
        let Some(branch) = branch else {
            bail!(
                "{} is on no branch; check one out first",
                self.worktree.display()
            );
        };
        let mut parse = sealed(&repo);
        parse.args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ]);
        let theirs = run(parse, "rev-parse")?.trim().to_owned();
        if theirs == ours {
            return Ok(format!("{branch} is already at {}", short(&ours)));
        }
        let mut fetch = sealed(&self.worktree);
        fetch
            .env("GIT_DIR", &self.admin)
            .env("GIT_WORK_TREE", &self.worktree)
            .args(["fetch", "--quiet", "--no-tags"])
            .arg(&repo)
            .arg(format!("refs/heads/{branch}"));
        run(fetch, "fetch")?;
        let mut ancestor = self.worktree_git();
        ancestor.args(["merge-base", "--is-ancestor", &ours, &theirs]);
        ensure!(
            ancestor.status().context("could not run git")?.success(),
            "the worktree's {branch} has commits the VM's lacks; merge them by hand"
        );
        let mut reset = self.worktree_git();
        reset.args(["reset", "--quiet", "--mixed", &theirs]);
        run(reset, "reset")?;
        Ok(format!("{branch}: {} -> {}", short(&ours), short(&theirs)))
    }

    /// The commits the worktree's branch has and the VM's lacks, as a bundle for a running VM,
    /// when its branch is behind the worktree's. Refuses, saying why, when the VM's branch has
    /// commits the worktree's lacks. Reads the session repository and writes nothing to it.
    pub(crate) fn push(&self) -> anyhow::Result<Push> {
        let repo = self.git_dir();
        let (branch, ours) = self.head()?;
        let Some(branch) = branch else {
            bail!(
                "{} is on no branch; check one out first",
                self.worktree.display()
            );
        };
        ensure!(
            repo.is_dir(),
            "{} has no session repository; `cargo xtask vm start` creates it",
            self.worktree.display()
        );
        let mut parse = sealed(&repo);
        parse.args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ]);
        let theirs = run(parse, "rev-parse")
            .with_context(|| format!("the session repository has no {branch}"))?
            .trim()
            .to_owned();
        if theirs == ours {
            return Ok(Push::Current(format!(
                "the VM's {branch} is already at {}",
                short(&ours)
            )));
        }
        let mut known = self.worktree_git();
        known.args(["cat-file", "-e", &format!("{theirs}^{{commit}}")]);
        ensure!(
            known
                .output()
                .context("could not run git")?
                .status
                .success(),
            "the VM's {branch} has commits the worktree's lacks; `cargo xtask vm sync` takes \
             them, or says the branches diverged"
        );
        let mut ancestor = self.worktree_git();
        ancestor.args(["merge-base", "--is-ancestor", &theirs, &ours]);
        ensure!(
            ancestor.status().context("could not run git")?.success(),
            "the VM's {branch} ({}) is not behind the worktree's ({}): the branches diverged; \
             merge them by hand",
            short(&theirs),
            short(&ours)
        );
        let mut bundle = self.worktree_git();
        bundle.args([
            "bundle",
            "create",
            "--quiet",
            "-",
            &format!("refs/heads/{branch}"),
            &format!("^{theirs}"),
        ]);
        let output = bundle.output().context("could not run git")?;
        ensure!(
            output.status.success(),
            "git bundle failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Push::Bundle {
            branch,
            from: theirs,
            to: ours,
            bundle: output.stdout,
        })
    }
}

/// git config of the main clone, whose git directory is `common`: trusted, the host's own.
fn git_main(common: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut command = Command::new("git");
    command.env("GIT_DIR", common).args(args);
    run(command, args.first().copied().unwrap_or("config"))
}

/// A config value, quoted for a git config file.
fn quoted(value: &str) -> String {
    let escaped: String = value
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Makes sure `dir` is a directory and not a symlink, replacing whatever else is there.
fn plain_dir(dir: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => remove_any(dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect {}", dir.display()));
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))
}

/// Removes whatever is at `path`, a symlink itself rather than what it points to.
fn remove_any(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    }
    .with_context(|| format!("could not remove {}", path.display()))
}

/// Writes `contents` to a new regular file at `path`, replacing whatever was there without
/// following a symlink: the new file is created exclusively.
fn write_plain(path: &Path, contents: &str) -> anyhow::Result<()> {
    remove_any(path)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .with_context(|| format!("could not write {}", path.display()))
}

/// The last component of `path`, as text.
fn name_of(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
        .with_context(|| format!("{} has no name", path.display()))
}

/// A name fit for a VM: lowercase letters, digits and dashes.
fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// A worktree of the main clone, as `vm gc` sees it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Worktree {
    /// Its session.
    pub(crate) session: Id,
    /// The branch checked out in it, without `refs/heads/`; none when its HEAD is detached.
    pub(crate) branch: Option<String>,
    /// The commit checked out in it.
    pub(crate) commit: String,
}

/// The linked worktrees of the main clone `main`, from `git worktree list --porcelain`: the host's
/// own record, never a file a VM can write.
pub(crate) fn worktrees(main: &Path) -> anyhow::Result<Vec<Worktree>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(main)
        .args(["worktree", "list", "--porcelain"]);
    let listing = run(command, "worktree list")?;
    let mut worktrees = Vec::new();
    // The first entry is the main clone itself.
    for entry in listing.split("\n\n").skip(1) {
        let mut session = None;
        let mut branch = None;
        let mut commit = None;
        for line in entry.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                session = Path::new(path)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .and_then(Id::of_worktree);
            } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
                branch = Some(name.to_owned());
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                commit = Some(head.to_owned());
            }
        }
        if let (Some(session), Some(commit)) = (session, commit) {
            worktrees.push(Worktree {
                session,
                branch,
                commit,
            });
        }
    }
    Ok(worktrees)
}

/// The branches of the session repository under the session directory `state`, each with its
/// commit; none when there is no repository. Read only through [`sealed`] git.
pub(crate) fn branches(state: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let repo = state.join("git");
    if !repo.is_dir() {
        return Ok(Vec::new());
    }
    let mut list = sealed(&repo);
    list.args([
        "for-each-ref",
        "--format=%(objectname) %(refname:lstrip=2)",
        "refs/heads/",
    ]);
    run(list, "for-each-ref")?
        .lines()
        .map(|line| {
            line.split_once(' ')
                .map(|(commit, branch)| (branch.to_owned(), commit.to_owned()))
                .with_context(|| format!("git for-each-ref printed {line:?}"))
        })
        .collect()
}

/// The commits of the session repository under `state` that only it holds: reachable from its
/// branches, but neither from a branch of `origin` it knows nor from any of `elsewhere`, commits
/// known to be kept (a merged pull request's head). Each is its commit's first twelve characters
/// and its subject. Commits of `elsewhere` the repository lacks are skipped.
pub(crate) fn unshared(state: &Path, elsewhere: &[String]) -> anyhow::Result<Vec<String>> {
    let tips: Vec<String> = branches(state)?
        .into_iter()
        .map(|(_, commit)| commit)
        .collect();
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let repo = state.join("git");
    let mut known = Vec::new();
    for commit in elsewhere {
        let mut exists = sealed(&repo);
        exists.args([
            "cat-file",
            "-e",
            "--end-of-options",
            &format!("{commit}^{{commit}}"),
        ]);
        if exists
            .output()
            .context("could not run git")?
            .status
            .success()
        {
            known.push(commit);
        }
    }
    let mut list = sealed(&repo);
    list.args(["rev-list", "--no-commit-header", "--format=%H %s"])
        .args(&tips)
        .args(["--not", "--remotes=origin"])
        .args(known);
    Ok(run(list, "rev-list")?
        .lines()
        .map(|line| {
            let (commit, subject) = line.split_once(' ').unwrap_or((line, ""));
            format!("{} {subject}", short(commit))
        })
        .collect())
}

/// The first twelve characters of a commit's name.
pub(crate) fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[cfg(test)]
mod tests;
