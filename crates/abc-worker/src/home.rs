//! The worker's home: where its key lives, the lock that allows one worker process per home, and
//! the one record (`worker.toml`) with its atomic, durable save.
//!
//! ```text
//! <home>/             0700   ABC_WORKER_HOME, else
//!                            macOS: ~/Library/Application Support/abc/worker
//!                            Linux: $XDG_CONFIG_HOME/abc/worker, else ~/.config/abc/worker
//!   worker.lock       0600   locked by `Held` for the process's life; never renamed
//!   worker.toml       0600   the record: the enrolment in use (server, what it was made for, key, who
//!                            it is) and a pending key whose answer has not been seen
//! ```
//!
//! A lost record loses the key for good: enrol again with a new token, and the owner revokes the
//! old, now offline, worker. A copied record authenticates on both machines; the newer session
//! supersedes the older (close 4409), and the superseded worker stops and says why
//! ([`crate::Terminal::Superseded`]), so two machines never take turns as one worker.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use abc_core::credential::{EnrolmentTokenDigest, WorkerKey};
use abc_core::secret::{Secret, serialize_exposed};
use abc_protocol::url::PublicUrl;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Enrolled;

/// The record's file name.
const RECORD: &str = "worker.toml";
/// The lock's file name.
const LOCK: &str = "worker.lock";
/// Where a save writes before it renames over [`RECORD`].
const STAGED: &str = "worker.toml.new";
/// The record format this build reads and writes. There are no migrations (S3): a record of any
/// other version is refused, and the message says to enrol again.
const FORMAT: u32 = 1;
/// Permission bits only the owner may hold.
const PRIVATE: u32 = 0o077;

/// The directory holding the worker's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHome {
    dir: PathBuf,
}

impl WorkerHome {
    /// `ABC_WORKER_HOME`, else this operating system's place for it, read through `lookup` (the
    /// process environment in the binary).
    ///
    /// # Errors
    ///
    /// [`HomeError::NoHomeDir`] when neither `ABC_WORKER_HOME` nor `HOME` is set.
    pub fn discover(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, HomeError> {
        let set = |name: &str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        if let Some(dir) = set("ABC_WORKER_HOME") {
            return Ok(Self::at(dir));
        }
        let config = if cfg!(target_os = "macos") {
            set("HOME").map(|home| home.join("Library/Application Support"))
        } else {
            set("XDG_CONFIG_HOME").or_else(|| set("HOME").map(|home| home.join(".config")))
        };
        config
            .map(|config| Self::at(config.join("abc/worker")))
            .ok_or(HomeError::NoHomeDir)
    }

    /// An explicit directory.
    #[must_use]
    pub const fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the directory (0700) if it is missing, refuses one that others can reach or that
    /// another user owns, and takes the lock without waiting.
    ///
    /// # Errors
    ///
    /// [`HomeError::Busy`] when another worker process holds it, [`HomeError::Insecure`] for an
    /// open or foreign directory, [`HomeError::Io`] otherwise.
    pub(crate) fn lock(&self) -> Result<Held, HomeError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|source| HomeError::io(&self.dir, source))?;
        private(&self.dir)?;
        let path = self.dir.join(LOCK);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| HomeError::io(&path, source))?;
        match lock.try_lock() {
            Ok(()) => Ok(Held {
                _lock: lock,
                dir: self.dir.clone(),
            }),
            Err(TryLockError::WouldBlock) => Err(HomeError::Busy {
                dir: self.dir.clone(),
            }),
            Err(TryLockError::Error(source)) => Err(HomeError::io(&path, source)),
        }
    }
}

/// Fails unless `path` belongs to this process's user and gives others no access.
fn private(path: &Path) -> Result<(), HomeError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| HomeError::io(path, source))?;
    let me = rustix::process::getuid().as_raw();
    if metadata.uid() != me || metadata.mode() & PRIVATE != 0 || metadata.file_type().is_symlink() {
        return Err(HomeError::Insecure {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Proof that this process holds the home's lock. Reading or writing the record takes one, so one
/// worker per home and one writer of the record are the same rule. The lock is released on drop,
/// and by the kernel on any exit, `kill -9` included.
#[derive(Debug)]
pub(crate) struct Held {
    _lock: File,
    dir: PathBuf,
}

/// What a key was made to enrol with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Basis {
    /// An enrolment token, by its digest. The same token again is a retry; another token is a new
    /// enrolment. A digest of a 256-bit token reveals nothing.
    Token(EnrolmentTokenDigest),
    /// The owner's approval in the browser. Running setup again while the request waits is a
    /// retry: the server answers the same request. One that ended (denied or expired) discards the
    /// key, so the next run asks afresh.
    Approval,
}

/// A key made for one enrolment at one server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Slot {
    /// The server the key was made for. Enrolment and every dial use this URL and no other.
    pub(crate) server: PublicUrl,
    /// What the key was sent with.
    pub(crate) basis: Basis,
    /// The worker's key. It leaves its `Secret` only here and in the `Authorization` header.
    #[serde(serialize_with = "serialize_exposed")]
    pub(crate) key: Secret<WorkerKey>,
}

impl Slot {
    /// Whether this key was made for `basis` at `server`: sending it again is a retry.
    pub(crate) fn is_for(&self, server: &PublicUrl, basis: &Basis) -> bool {
        self.server == *server && self.basis == *basis
    }
}

/// The enrolment in use: its key, and who the server said it is.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Current {
    /// The key the worker dials with.
    pub(crate) key: Slot,
    /// Who the server says this machine is.
    pub(crate) worker: Enrolled,
}

/// `worker.toml`: the enrolment in use, and the one under way. A new key is written to `pending`
/// the moment it exists, before any request names it, so a crash at any point leaves either
/// nothing or a key this machine can retry with. It becomes `current` only once the server has
/// answered, so a failed `--replace` leaves the enrolment in use working.
#[derive(Debug, Default)]
pub(crate) struct Record {
    /// The enrolment the worker runs with.
    pub(crate) current: Option<Current>,
    /// A key sent, or about to be sent, whose answer has not been seen.
    pub(crate) pending: Option<Slot>,
}

/// The record as stored: owned when loaded, borrowed when saved.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored<C, P> {
    /// [`FORMAT`].
    format: u32,
    current: Option<C>,
    pending: Option<P>,
}

impl Held {
    fn record_path(&self) -> PathBuf {
        self.dir.join(RECORD)
    }

    /// The record, or `None` when there is none. A record others can read, owned by another user,
    /// unparsable, or of another format is an error, never "absent": enrolling again over a
    /// damaged key would leave a worker behind on the server.
    pub(crate) fn load(&self) -> Result<Option<Record>, HomeError> {
        let path = self.record_path();
        match fs::symlink_metadata(&path) {
            Ok(_) => private(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(HomeError::io(&path, source)),
        }
        let text = zeroize::Zeroizing::new(
            fs::read_to_string(&path).map_err(|source| HomeError::io(&path, source))?,
        );
        // The parser's message can quote the file, and the file holds the key: it is dropped.
        let stored: Stored<Current, Slot> = toml_edit::de::from_str(&text)
            .ok()
            .ok_or_else(|| HomeError::Unreadable { path: path.clone() })?;
        if stored.format != FORMAT {
            return Err(HomeError::Unreadable { path });
        }
        Ok(Some(Record {
            current: stored.current,
            pending: stored.pending,
        }))
    }

    /// Replaces the record, atomically and durably: the new text goes to a fresh 0600 file, which
    /// is synced, renamed over the record, and the directory synced. A crash leaves the old record
    /// or the new one, never part of either.
    pub(crate) fn save(
        &self,
        current: Option<&Current>,
        pending: Option<&Slot>,
    ) -> Result<(), HomeError> {
        let staged = self.dir.join(STAGED);
        let stored = Stored {
            format: FORMAT,
            current,
            pending,
        };
        let text = zeroize::Zeroizing::new(
            toml_edit::ser::to_string_pretty(&stored)
                .ok()
                .ok_or(HomeError::Encode)?,
        );
        // A staged file is left only by a crash mid-save; this process holds the lock, so it is
        // stale.
        match fs::remove_file(&staged) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(HomeError::io(&staged, source)),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)
            .map_err(|source| HomeError::io(&staged, source))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| HomeError::io(&staged, source))?;
        drop(file);
        let path = self.record_path();
        fs::rename(&staged, &path).map_err(|source| HomeError::io(&path, source))?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| HomeError::io(&self.dir, source))
    }
}

/// The home could not be used. Each message says what to fix and never quotes the record.
#[derive(Debug, Error)]
pub enum HomeError {
    /// Another `abc worker` process holds this home.
    #[error(
        "another abc worker is already using {}; stop it first (one worker per home)",
        dir.display()
    )]
    Busy {
        /// The home.
        dir: PathBuf,
    },
    /// Neither `ABC_WORKER_HOME` nor `HOME` is set.
    #[error("cannot find a home directory for the worker: set ABC_WORKER_HOME")]
    NoHomeDir,
    /// Others can reach the file or directory, another user owns it, or it is a symbolic link.
    #[error(
        "{} must be yours and private, not a link (chmod 700 the directory, 600 the file)",
        path.display()
    )]
    Insecure {
        /// The path.
        path: PathBuf,
    },
    /// The record does not parse, or has a format this build does not know.
    #[error(
        "{} is damaged or from another abc version; move it aside and enrol again",
        path.display()
    )]
    Unreadable {
        /// The record's path.
        path: PathBuf,
    },
    /// The record could not be written as TOML; a bug.
    #[error("could not encode the worker's record")]
    Encode,
    /// Any other filesystem failure.
    #[error("cannot use {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

impl HomeError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use abc_core::credential::EnrolmentToken;
    use abc_core::id::FromUuid as _;
    use abc_core::identity::DisplayName;

    use super::*;
    use crate::testing::key;

    fn token_digest() -> EnrolmentTokenDigest {
        EnrolmentToken::parse(&format!("abc_enr_{}", "1".repeat(52)))
            .unwrap()
            .expose_secret()
            .digest()
    }

    fn url() -> PublicUrl {
        PublicUrl::parse("https://abc.test.ts.net").unwrap()
    }

    fn home() -> (tempfile::TempDir, WorkerHome) {
        let temp = tempfile::tempdir().unwrap();
        let home = WorkerHome::at(temp.path().join("abc/worker"));
        (temp, home)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn discovers_the_home_per_operating_system() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            WorkerHome::discover(env(&[("ABC_WORKER_HOME", "/w"), ("HOME", "/h")])).unwrap(),
            WorkerHome::at(PathBuf::from("/w")),
            "the override wins"
        );
        let expected = if cfg!(target_os = "macos") {
            "/h/Library/Application Support/abc/worker"
        } else {
            "/x/abc/worker"
        };
        assert_eq!(
            WorkerHome::discover(env(&[
                ("ABC_WORKER_HOME", ""),
                ("XDG_CONFIG_HOME", "/x"),
                ("HOME", "/h")
            ]))
            .unwrap()
            .dir(),
            Path::new(expected),
            "an empty override is unset"
        );
        let fallback = if cfg!(target_os = "macos") {
            "/h/Library/Application Support/abc/worker"
        } else {
            "/h/.config/abc/worker"
        };
        assert_eq!(
            WorkerHome::discover(env(&[("HOME", "/h")])).unwrap().dir(),
            Path::new(fallback),
            "the user's config directory"
        );
        assert!(
            matches!(WorkerHome::discover(env(&[])), Err(HomeError::NoHomeDir)),
            "no home at all"
        );
    }

    #[test]
    fn the_lock_makes_one_worker_per_home() {
        let (_temp, home) = home();
        let held = home.lock().unwrap();
        assert_eq!(mode(home.dir()), 0o700, "the home is created private");
        assert_eq!(mode(&home.dir().join(LOCK)), 0o600, "and so is the lock");
        let busy = home.lock().unwrap_err();
        assert!(
            matches!(&busy, HomeError::Busy { dir } if dir == home.dir()),
            "a second holder is refused: {busy:?}"
        );
        assert!(
            busy.to_string()
                .starts_with("another abc worker is already using "),
            "{busy}"
        );
        drop(held);
        home.lock().expect("dropping the lock frees the home");
    }

    #[test]
    fn an_open_or_linked_home_is_refused() {
        let (temp, home) = home();
        fs::create_dir_all(home.dir()).unwrap();
        fs::set_permissions(home.dir(), fs::Permissions::from_mode(0o750)).unwrap();
        let error = home.lock().unwrap_err();
        assert!(
            matches!(&error, HomeError::Insecure { path } if path == home.dir()),
            "a group-readable home: {error:?}"
        );
        assert!(
            error
                .to_string()
                .ends_with("(chmod 700 the directory, 600 the file)"),
            "{error}"
        );
        fs::set_permissions(home.dir(), fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(home.dir(), &link).unwrap();
        assert!(
            matches!(WorkerHome::at(link).lock(), Err(HomeError::Insecure { .. })),
            "a link to a home"
        );
    }

    fn slot(seed: u8) -> Slot {
        Slot {
            server: url(),
            basis: Basis::Token(token_digest()),
            key: key(seed),
        }
    }

    #[test]
    fn a_slot_is_for_one_token_at_one_server() {
        let slot = slot(1);
        assert!(
            slot.is_for(&url(), &Basis::Token(token_digest())),
            "its own"
        );
        assert!(!slot.is_for(&url(), &Basis::Approval), "not an approval");
        let other = PublicUrl::parse("https://other.test.ts.net").unwrap();
        assert!(
            !slot.is_for(&other, &Basis::Token(token_digest())),
            "another server"
        );
        let token = EnrolmentToken::parse(&format!("abc_enr_{}", "2".repeat(52)))
            .unwrap()
            .expose_secret()
            .digest();
        assert!(!slot.is_for(&url(), &Basis::Token(token)), "another token");
    }

    #[test]
    fn a_record_survives_a_save_and_a_load() {
        let (_temp, home) = home();
        let held = home.lock().unwrap();
        assert!(held.load().unwrap().is_none(), "no record yet");
        let text = key(1).expose_secret().as_str().to_owned();
        held.save(None, Some(&slot(1))).unwrap();
        let path = home.dir().join(RECORD);
        assert_eq!(mode(&path), 0o600, "the record is private");
        assert!(!home.dir().join(STAGED).exists(), "nothing staged is left");
        let loaded = held.load().unwrap().unwrap();
        assert!(loaded.current.is_none(), "nothing in use");
        let pending = loaded.pending.unwrap();
        assert_eq!(pending.server, url(), "the server");
        assert_eq!(
            pending.basis,
            Basis::Token(token_digest()),
            "the token's digest"
        );
        assert_eq!(pending.key.expose_secret().as_str(), text, "the key");
        let enrolled = Enrolled {
            worker: abc_core::id::WorkerId::from_uuid(uuid::Uuid::from_u128(7)),
            workspace: abc_core::id::WorkspaceId::from_uuid(uuid::Uuid::from_u128(8)),
            name: DisplayName::parse("mbp").unwrap(),
        };
        let current = Current {
            key: slot(1),
            worker: enrolled.clone(),
        };
        fs::write(home.dir().join(STAGED), "left by a crash").unwrap();
        held.save(Some(&current), Some(&slot(2))).unwrap();
        let saved_both = held.load().unwrap().unwrap();
        let loaded_current = saved_both.current.unwrap();
        assert_eq!(
            loaded_current.worker, enrolled,
            "an enrolment in use; a stale staged file does not stop the save"
        );
        assert_eq!(
            loaded_current.key.key.expose_secret().as_str(),
            text,
            "with its key"
        );
        assert_eq!(
            saved_both.pending.unwrap().key.expose_secret().as_str(),
            key(2).expose_secret().as_str(),
            "beside a pending one"
        );
        let saved = fs::read_to_string(&path).unwrap();
        assert!(
            saved.starts_with(
                "format = 1\n\n[current.key]\nserver = \"https://abc.test.ts.net\"\nkey = \""
            ) && saved.contains("\n[current.key.basis]\ntoken = \""),
            "readable TOML: {saved}"
        );
    }

    #[test]
    fn a_damaged_or_open_record_is_an_error_not_an_absence() {
        let (_temp, home) = home();
        let held = home.lock().unwrap();
        let path = home.dir().join(RECORD);
        held.save(None, Some(&slot(2))).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(held.load(), Err(HomeError::Insecure { .. })),
            "a world-readable key"
        );
        let secret = key(3);
        for damaged in [
            "not toml at all".to_owned(),
            format!(
                "format = 2\n\n[pending]\nserver = \"https://abc.test.ts.net\"\nbasis = {{ token = \"{}\" }}\nkey = \"{}\"\n",
                "0".repeat(64),
                secret.expose_secret().as_str()
            ),
            format!(
                "format = 1\n\n[pending]\nserver = \"https://abc.test.ts.net\"\nbasis = {{ token = \"{}\" }}\nkey = \"{}x\"\n",
                "0".repeat(64),
                secret.expose_secret().as_str()
            ),
            format!(
                "format = 1\n\n[pending]\nserver = \"http://abc\"\nbasis = {{ token = \"{}\" }}\nkey = \"{}\"\n",
                "0".repeat(64),
                secret.expose_secret().as_str()
            ),
            format!(
                "format = 1\n\n[pending]\nserver = \"https://abc.test.ts.net\"\nbasis = {{ token = \"{}\" }}\nkey = \"{}\"\nextra = 1\n",
                "0".repeat(64),
                secret.expose_secret().as_str()
            ),
        ] {
            fs::write(&path, &damaged).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let error = held.load().unwrap_err();
            assert!(
                matches!(&error, HomeError::Unreadable { path: got } if *got == path),
                "{damaged}: {error:?}"
            );
            assert!(
                !format!("{error:?} {error}").contains(secret.expose_secret().as_str()),
                "the error never quotes the key"
            );
        }
    }

    #[test]
    fn only_a_missing_record_is_absent_and_only_a_missing_staged_file_is_skipped() {
        let (temp, home) = home();
        let held = home.lock().unwrap();
        // A home whose path is a file: looking up the record fails, but not as "not found".
        let file = temp.path().join("not-a-directory");
        fs::write(&file, "").unwrap();
        let broken = Held {
            _lock: File::open(&file).unwrap(),
            dir: file.clone(),
        };
        let error = broken.load().unwrap_err();
        assert!(
            matches!(&error, HomeError::Io { path, source }
                if *path == file.join(RECORD) && source.kind() != std::io::ErrorKind::NotFound),
            "an unreadable record is an error, never an absence: {error:?}"
        );
        // A directory where the staged file goes cannot be removed; the save stops there.
        let staged = home.dir().join(STAGED);
        fs::create_dir(&staged).unwrap();
        let blocked = held.save(None, Some(&slot(4))).unwrap_err();
        assert!(
            matches!(&blocked, HomeError::Io { path, source }
                if *path == staged && source.kind() != std::io::ErrorKind::AlreadyExists),
            "the stale staged path could not be cleared: {blocked:?}"
        );
    }

    #[test]
    fn failures_name_the_path() {
        let (temp, home) = home();
        fs::write(
            temp.path().join("abc"),
            "a file where the home's parent goes",
        )
        .unwrap();
        let error = home.lock().unwrap_err();
        assert!(
            matches!(&error, HomeError::Io { path, .. } if path == home.dir()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("cannot use {}", home.dir().display()),
            "the path, with the cause as its source"
        );
    }
}
