//! A session: one worktree, its VM's name, and its own git repository.
//!
//! The VM never sees the main clone. Each session gets a bare repository of its own under the
//! state directory holding only the worktree's branch and `origin/main`, laid out as the common
//! directory of a linked worktree. The VM mounts it where the worktree's `.git` file points, so git
//! inside works unchanged, and other branches, other worktrees and the main clone's config and
//! hooks stay out. Commits made in the VM come back with [`Session::sync`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail, ensure};

use crate::git::git;

/// One agent session's names and places.
#[derive(Debug)]
pub(crate) struct Session {
    /// The worktree, canonical.
    pub(crate) worktree: PathBuf,
    /// The VM's name, and the prefix of its volumes' names.
    pub(crate) name: String,
    /// The session's own directory under the state root.
    pub(crate) state: PathBuf,
    /// The main clone's common git directory, where the worktree's `.git` file leads.
    pub(crate) common: PathBuf,
    /// The worktree's name in the main clone (`.git/worktrees/<id>`).
    id: String,
}

impl Session {
    /// The session for the linked worktree at `worktree`, keeping its state under `state_root`.
    pub(crate) fn open(worktree: &Path, state_root: &Path) -> anyhow::Result<Self> {
        let worktree = fs::canonicalize(worktree)
            .with_context(|| format!("{} does not exist", worktree.display()))?;
        let dot_git = worktree.join(".git");
        ensure!(
            dot_git.is_file(),
            "{} is not a linked worktree (its .git is not a file); agent sessions run in worktrees \
             made with `git worktree add`",
            worktree.display()
        );
        let pointer = fs::read_to_string(&dot_git)
            .with_context(|| format!("could not read {}", dot_git.display()))?;
        let gitdir = pointer
            .strip_prefix("gitdir: ")
            .map(str::trim_end)
            .with_context(|| format!("{} does not name a gitdir", dot_git.display()))?;
        let gitdir = Path::new(gitdir);
        let common = gitdir.join(
            fs::read_to_string(gitdir.join("commondir"))
                .with_context(|| format!("{} has no commondir", gitdir.display()))?
                .trim_end(),
        );
        let common = fs::canonicalize(&common)
            .with_context(|| format!("{} does not exist", common.display()))?;
        let id = name_of(gitdir)?;
        let slug = slug(&name_of(&worktree)?);
        ensure!(
            !slug.is_empty(),
            "{} has no letters or digits to name a VM after",
            worktree.display()
        );
        Ok(Self {
            name: format!("abc-{slug}"),
            state: state_root.join(&slug),
            worktree,
            common,
            id,
        })
    }

    /// The session's own git repository.
    pub(crate) fn git_dir(&self) -> PathBuf {
        self.state.join("git")
    }

    /// The worktree's administrative directory in the session's repository, where its `.git`
    /// file leads inside the VM.
    fn admin(&self) -> PathBuf {
        self.git_dir().join("worktrees").join(&self.id)
    }

    /// Creates or refreshes the session's repository from the worktree: its branch moves forward
    /// to the worktree's commit when it can, and never back, so commits made in the VM are kept.
    /// Returns a note for the user when the VM's branch has commits the worktree lacks.
    pub(crate) fn prepare_git(&self) -> anyhow::Result<Option<String>> {
        let repo = self.git_dir();
        let worktree = self
            .worktree
            .to_str()
            .context("the worktree's path is not UTF-8")?;
        let admin = self.admin();
        if !repo.exists() {
            fs::create_dir_all(&repo)
                .with_context(|| format!("could not create {}", repo.display()))?;
            git(&repo, &["init", "--quiet", "--bare"])?;
            let origin = git(&self.worktree, &["config", "remote.origin.url"])?;
            git(&repo, &["config", "remote.origin.url", origin.trim()])?;
            git(
                &repo,
                &[
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
            )?;
            for key in ["user.name", "user.email"] {
                if let Ok(value) = git(&self.worktree, &["config", key]) {
                    git(&repo, &["config", key, value.trim()])?;
                }
            }
            git(
                &repo,
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    worktree,
                    "+refs/remotes/origin/main:refs/remotes/origin/main",
                ],
            )?;
            git(
                &repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            )?;
            fs::create_dir_all(&admin)
                .with_context(|| format!("could not create {}", admin.display()))?;
            fs::write(admin.join("commondir"), "../..\n")?;
            fs::write(admin.join("gitdir"), format!("{worktree}/.git\n"))?;
        }
        let head = git(&self.worktree, &["rev-parse", "HEAD"])?;
        let head = head.trim();
        let mut note = None;
        if let Some(branch) = branch(&self.worktree)? {
            {
                // Without `+`, git refuses to move the branch back: a fast-forward or nothing.
                let refspec = format!("{head}:refs/heads/{branch}");
                let fetched = git(
                    &repo,
                    &[
                        "fetch",
                        "--quiet",
                        "--update-head-ok",
                        "--no-tags",
                        worktree,
                        &refspec,
                    ],
                );
                if fetched.is_err() {
                    note = Some(format!(
                        "the VM's {branch} has commits the worktree's lacks; `cargo xtask vm sync` \
                         takes them"
                    ));
                }
                fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
            }
        } else {
            git(&repo, &["fetch", "--quiet", "--no-tags", worktree, head])?;
            fs::write(admin.join("HEAD"), format!("{head}\n"))?;
        }
        if !admin.join("index").exists() {
            let output = std::process::Command::new("git")
                .args(["read-tree", "HEAD"])
                .env("GIT_DIR", &admin)
                .env("GIT_WORK_TREE", &self.worktree)
                .output()
                .context("could not run git")?;
            ensure!(
                output.status.success(),
                "git read-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(note)
    }

    /// Moves the worktree's branch to the VM's commit on it, when that only adds commits. The
    /// files are already the VM's, so the index follows and the files stay as they are. Returns
    /// what changed, for the user.
    pub(crate) fn sync(&self) -> anyhow::Result<String> {
        let repo = self.git_dir();
        let Some(branch) = branch(&self.worktree)? else {
            bail!(
                "{} is on no branch; check one out first",
                self.worktree.display()
            );
        };
        let theirs = git(&repo, &["rev-parse", &format!("refs/heads/{branch}")])?;
        let theirs = theirs.trim();
        let ours = git(&self.worktree, &["rev-parse", "HEAD"])?;
        let ours = ours.trim();
        if theirs == ours {
            return Ok(format!("{branch} is already at {}", short(ours)));
        }
        ensure!(
            git(&repo, &["merge-base", "--is-ancestor", ours, theirs]).is_ok(),
            "the worktree's {branch} has commits the VM's lacks; merge them by hand"
        );
        let repo = repo
            .to_str()
            .context("the state directory's path is not UTF-8")?;
        git(
            &self.worktree,
            &["fetch", "--quiet", repo, &format!("refs/heads/{branch}")],
        )?;
        git(&self.worktree, &["reset", "--quiet", "--mixed", theirs])?;
        Ok(format!("{branch}: {} -> {}", short(ours), short(theirs)))
    }
}

/// The branch checked out in `worktree`, or none when its HEAD is detached.
fn branch(worktree: &Path) -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(worktree)
        .output()
        .context("could not run git")?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

/// The last component of `path`, as text.
fn name_of(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
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

/// The first twelve characters of a commit's name.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{Session, short, slug};
    use crate::testing::{git, repository};

    /// A main clone with an `origin/main` and a linked worktree on branch `feature`; returns the
    /// worktree's path.
    fn worktree(dir: &Path) -> std::path::PathBuf {
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
        git(&main, &["config", "user.name", "Maintainer"]);
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
        worktree.canonicalize().unwrap()
    }

    #[test]
    fn names_the_session_after_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = worktree(dir.path());
        let session = Session::open(&worktree, Path::new("/state")).unwrap();
        assert_eq!(session.name, "abc-feature-tree");
        assert_eq!(session.state, Path::new("/state/feature-tree"));
        assert_eq!(session.git_dir(), Path::new("/state/feature-tree/git"));
        assert_eq!(
            session.common,
            dir.path().join("main/.git").canonicalize().unwrap()
        );
        assert_eq!(session.worktree, worktree);
    }

    #[test]
    fn refuses_what_is_not_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let main = repository(&dir.path().join("main"));
        let error = format!("{:#}", Session::open(&main, dir.path()).unwrap_err());
        assert!(error.contains("not a linked worktree"), "{error}");
        let missing = format!(
            "{:#}",
            Session::open(&dir.path().join("gone"), dir.path()).unwrap_err()
        );
        assert!(missing.contains("does not exist"), "{missing}");
        let odd = dir.path().join("odd");
        fs::create_dir_all(&odd).unwrap();
        fs::write(odd.join(".git"), "not a pointer\n").unwrap();
        let pointer = format!("{:#}", Session::open(&odd, dir.path()).unwrap_err());
        assert!(pointer.contains("does not name a gitdir"), "{pointer}");
        let symbols = dir.path().join("---");
        fs::create_dir_all(&symbols).unwrap();
        fs::write(
            symbols.join(".git"),
            format!("gitdir: {}\n", main.join(".git").display()),
        )
        .unwrap();
        fs::write(main.join(".git/commondir"), ".\n").unwrap();
        let unnamed = format!("{:#}", Session::open(&symbols, dir.path()).unwrap_err());
        assert!(unnamed.contains("no letters or digits"), "{unnamed}");
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
        let worktree = worktree(dir.path());
        let main = dir.path().join("main");
        git(&main, &["branch", "other"]);
        let session = Session::open(&worktree, &dir.path().join("state")).unwrap();
        assert_eq!(session.prepare_git().unwrap(), None);
        let repo = session.git_dir();
        let refs = git(&repo, &["for-each-ref", "--format=%(refname)"]);
        assert_eq!(
            refs, "refs/heads/feature\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\n",
            "no other branch"
        );
        assert_eq!(
            git(&repo, &["config", "remote.origin.url"]).trim(),
            "https://github.com/getabc-dev/abc.git"
        );
        let config = fs::read_to_string(repo.join("config")).unwrap();
        assert!(config.contains("name = Maintainer"), "{config}");
        let admin = session.admin();
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
            format!("{}/.git\n", worktree.display())
        );
        assert!(
            admin.join("index").exists(),
            "an index for the worktree's files"
        );
    }

    /// Runs git in `worktree` as the VM does: with the session repository in the main clone's
    /// place.
    fn commit_in_vm(session: &Session, message: &str) {
        let admin = session.admin();
        let status = std::process::Command::new("git")
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
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn commits_made_in_the_vm_come_back_and_are_never_lost() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = worktree(dir.path());
        let session = Session::open(&worktree, &dir.path().join("state")).unwrap();
        session.prepare_git().unwrap();
        assert!(
            session.sync().unwrap().contains("already at"),
            "nothing to take yet"
        );
        commit_in_vm(&session, "in the vm");
        let vm_head = git(&session.git_dir(), &["rev-parse", "refs/heads/feature"]);
        let note = session.prepare_git().unwrap().unwrap();
        assert!(note.contains("cargo xtask vm sync"), "{note}");
        assert_eq!(
            git(&session.git_dir(), &["rev-parse", "refs/heads/feature"]),
            vm_head,
            "a restart keeps the VM's commit"
        );
        let moved = session.sync().unwrap();
        assert!(moved.starts_with("feature: "), "{moved}");
        assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), vm_head);
        assert_eq!(
            git(&worktree, &["log", "-1", "--format=%s"]).trim(),
            "in the vm"
        );
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
            git(&session.git_dir(), &["rev-parse", "refs/heads/feature"]),
            git(&worktree, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn diverged_branches_are_left_to_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = worktree(dir.path());
        let session = Session::open(&worktree, &dir.path().join("state")).unwrap();
        session.prepare_git().unwrap();
        commit_in_vm(&session, "in the vm");
        git(
            &worktree,
            &["commit", "--quiet", "--allow-empty", "-m", "on the host"],
        );
        let error = format!("{:#}", session.sync().unwrap_err());
        assert!(error.contains("merge them by hand"), "{error}");
    }

    #[test]
    fn a_detached_worktree_gets_its_commit_but_no_sync() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = worktree(dir.path());
        git(&worktree, &["checkout", "--quiet", "--detach"]);
        let session = Session::open(&worktree, &dir.path().join("state")).unwrap();
        session.prepare_git().unwrap();
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        assert_eq!(
            fs::read_to_string(session.admin().join("HEAD")).unwrap(),
            head
        );
        let error = format!("{:#}", session.sync().unwrap_err());
        assert!(error.contains("on no branch"), "{error}");
    }
}
