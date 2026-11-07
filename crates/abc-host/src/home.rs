//! abc server's home: one private directory holding everything the stack keeps.
//!
//! ```text
//! <home>/              0700  ABC_SERVER_HOME, else
//!                            macOS: ~/Library/Application Support/abc/server
//!                            Linux: $XDG_DATA_HOME/abc/server, else ~/.local/share/abc/server
//!   config.toml        0600  the public URL, the ops address, the programs' paths
//!   postgres/          0700  the Postgres 18 cluster (initdb's)
//!   tailscale/         0700  abc's tailscaled state: its node key and Serve's setting
//!   run/               0700  the sockets: .s.PGSQL.5432, tailscaled.sock, abc.sock
//!   logs/              0700  each service's output (macOS; Linux uses the journal)
//!   backups/           0700  abc server backup's dumps
//! ```
//!
//! The server's home sits beside the worker's (`abc/worker`), never above it, so
//! `abc server uninstall --purge` cannot reach a worker's key. Every socket path is checked
//! against the operating system's limit before anything binds one.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use crate::{HostError, Os};

/// Permission bits only the owner may hold.
const PRIVATE: u32 = 0o077;

/// The directory holding abc server's data, sockets and configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHome {
    dir: PathBuf,
}

impl ServerHome {
    /// `ABC_SERVER_HOME`, else this operating system's place for it, read through `set` (which
    /// answers only for variables that are set and not empty).
    ///
    /// # Errors
    ///
    /// [`HostError::NoHomeDir`] when neither `ABC_SERVER_HOME` nor `HOME` is set.
    pub fn discover(os: Os, set: impl Fn(&str) -> Option<OsString>) -> Result<Self, HostError> {
        if let Some(dir) = set("ABC_SERVER_HOME") {
            return Ok(Self::at(PathBuf::from(dir)));
        }
        let home = set("HOME").map(PathBuf::from);
        let data = match os {
            Os::MacOs => home.map(|home| home.join("Library/Application Support")),
            Os::Linux => set("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| home.map(|home| home.join(".local/share"))),
        };
        data.map(|data| Self::at(data.join("abc/server")))
            .ok_or(HostError::NoHomeDir)
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

    /// `config.toml`.
    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// The Postgres cluster's directory.
    #[must_use]
    pub fn postgres(&self) -> PathBuf {
        self.dir.join("postgres")
    }

    /// abc's tailscaled state directory.
    #[must_use]
    pub fn tailscale(&self) -> PathBuf {
        self.dir.join("tailscale")
    }

    /// The sockets' directory, which is also Postgres's `unix_socket_directories`.
    #[must_use]
    pub fn run(&self) -> PathBuf {
        self.dir.join("run")
    }

    /// The services' output on macOS.
    #[must_use]
    pub fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }

    /// Where `abc server backup` writes when given no file.
    #[must_use]
    pub fn backups(&self) -> PathBuf {
        self.dir.join("backups")
    }

    /// Postgres's socket, which it names after its port.
    #[must_use]
    pub fn postgres_socket(&self) -> PathBuf {
        self.run().join(".s.PGSQL.5432")
    }

    /// abc's tailscaled's socket, where its CLI talks to it.
    #[must_use]
    pub fn tailscaled_socket(&self) -> PathBuf {
        self.run().join("tailscaled.sock")
    }

    /// abc server's sidecar socket, the one Serve proxies to.
    #[must_use]
    pub fn abc_socket(&self) -> PathBuf {
        self.run().join("abc.sock")
    }

    /// Fails unless every socket path fits `os`'s limit, and the home can be written into
    /// Postgres's settings.
    ///
    /// # Errors
    ///
    /// [`HostError::SocketPathTooLong`] naming the longest path, or [`HostError::UnusablePath`].
    pub fn check_paths(&self, os: Os) -> Result<(), HostError> {
        let bytes = self.dir.as_os_str().as_bytes();
        if bytes
            .iter()
            .any(|byte| matches!(byte, b'"' | b'\n' | b'\r'))
        {
            return Err(HostError::UnusablePath {
                path: self.dir.clone(),
            });
        }
        // Postgres also binds a lock file beside its socket, `.s.PGSQL.5432.lock`, which is no
        // socket and has no such limit.
        for socket in [
            self.postgres_socket(),
            self.tailscaled_socket(),
            self.abc_socket(),
        ] {
            let len = socket.as_os_str().len();
            if len > os.socket_path_limit() {
                return Err(HostError::SocketPathTooLong {
                    path: socket,
                    len,
                    limit: os.socket_path_limit(),
                    os: os.name(),
                });
            }
        }
        Ok(())
    }

    /// Creates the home and its directories, each 0700, or checks the ones there: each must
    /// belong to this user, give no one else access and not be a link. The cluster's directory is
    /// initdb's to create.
    ///
    /// # Errors
    ///
    /// [`HostError::Insecure`] or [`HostError::Io`].
    pub fn create(&self) -> Result<(), HostError> {
        for dir in [
            self.dir.clone(),
            self.tailscale(),
            self.run(),
            self.logs(),
            self.backups(),
        ] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|source| HostError::io(&dir, source))?;
            private(&dir)?;
        }
        Ok(())
    }

    /// Whether the home exists.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.dir.is_dir()
    }
}

/// Fails unless `path` belongs to this process's user, gives others no access and is not a link.
pub(crate) fn private(path: &Path) -> Result<(), HostError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| HostError::io(path, source))?;
    let me = rustix::process::getuid().as_raw();
    if metadata.uid() != me || metadata.mode() & PRIVATE != 0 || metadata.file_type().is_symlink() {
        return Err(HostError::Insecure {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// Writes `text` to `path` as a private file (0600), atomically: a fresh file beside it is
/// written, synced and renamed over it. Returns whether the content changed.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<bool, HostError> {
    match fs::read_to_string(path) {
        Ok(old) if old == text => {
            private(path)?;
            return Ok(false);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(HostError::io(path, source)),
    }
    let mut staged = path.as_os_str().to_owned();
    staged.push(".new");
    let staged = PathBuf::from(staged);
    // A staged file is left only by a crash mid-write: it is overwritten, and made private
    // whatever mode it had.
    let write = || -> io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staged)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|source| HostError::io(&staged, source))?;
    fs::rename(&staged, path).map_err(|source| HostError::io(path, source))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{ServerHome, write_private};
    use crate::{HostError, Os};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn the_home_is_each_systems_data_place_beside_the_worker() {
        let mac = ServerHome::discover(Os::MacOs, env(&[("HOME", "/Users/ada")])).unwrap();
        assert_eq!(
            mac.dir(),
            Path::new("/Users/ada/Library/Application Support/abc/server"),
            "macOS"
        );
        let linux = ServerHome::discover(Os::Linux, env(&[("HOME", "/home/ada")])).unwrap();
        assert_eq!(
            linux.dir(),
            Path::new("/home/ada/.local/share/abc/server"),
            "Linux"
        );
        let xdg = ServerHome::discover(
            Os::Linux,
            env(&[("HOME", "/home/ada"), ("XDG_DATA_HOME", "/data")]),
        )
        .unwrap();
        assert_eq!(xdg.dir(), Path::new("/data/abc/server"), "XDG_DATA_HOME");
        let set = ServerHome::discover(
            Os::MacOs,
            env(&[("HOME", "/Users/ada"), ("ABC_SERVER_HOME", "/srv/abc")]),
        )
        .unwrap();
        assert_eq!(set.dir(), Path::new("/srv/abc"), "ABC_SERVER_HOME wins");
        assert!(
            matches!(
                ServerHome::discover(Os::Linux, env(&[])),
                Err(HostError::NoHomeDir)
            ),
            "no HOME"
        );
    }

    #[test]
    fn the_paths_are_named_inside_the_home() {
        let home = ServerHome::at(PathBuf::from("/h"));
        assert_eq!(home.config(), Path::new("/h/config.toml"), "config");
        assert_eq!(home.postgres(), Path::new("/h/postgres"), "cluster");
        assert_eq!(home.tailscale(), Path::new("/h/tailscale"), "state");
        assert_eq!(home.logs(), Path::new("/h/logs"), "logs");
        assert_eq!(home.backups(), Path::new("/h/backups"), "backups");
        assert_eq!(
            home.postgres_socket(),
            Path::new("/h/run/.s.PGSQL.5432"),
            "Postgres names its socket after its port"
        );
        assert_eq!(
            home.tailscaled_socket(),
            Path::new("/h/run/tailscaled.sock"),
            "tailscaled"
        );
        assert_eq!(home.abc_socket(), Path::new("/h/run/abc.sock"), "abc");
    }

    #[test]
    fn a_socket_path_one_byte_over_the_limit_is_refused_with_the_fix() {
        // The longest socket is tailscaled's: "/run/tailscaled.sock" is 20 bytes after the home.
        let fits = format!("/{}", "a".repeat(103 - 20 - 1));
        let home = ServerHome::at(PathBuf::from(&fits));
        assert_eq!(
            home.tailscaled_socket().as_os_str().len(),
            103,
            "at the limit"
        );
        home.check_paths(Os::MacOs).unwrap();
        let over = ServerHome::at(PathBuf::from(format!("{fits}a")));
        let error = over.check_paths(Os::MacOs).unwrap_err();
        assert!(
            matches!(
                &error,
                HostError::SocketPathTooLong {
                    len: 104,
                    limit: 103,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("set ABC_SERVER_HOME to a shorter directory"),
            "names the fix: {error}"
        );
        over.check_paths(Os::Linux).unwrap();
        let linux_over = ServerHome::at(PathBuf::from(format!("/{}", "a".repeat(108 - 20 - 1))));
        assert!(
            matches!(
                linux_over.check_paths(Os::Linux),
                Err(HostError::SocketPathTooLong {
                    len: 108,
                    limit: 107,
                    ..
                })
            ),
            "Linux allows four bytes more, and no more"
        );
    }

    #[test]
    fn a_home_postgres_cannot_quote_is_refused() {
        for bad in ["/a\"b", "/a\nb", "/a\rb"] {
            let error = ServerHome::at(PathBuf::from(bad))
                .check_paths(Os::Linux)
                .unwrap_err();
            assert!(
                matches!(error, HostError::UnusablePath { .. }),
                "{bad:?}: {error:?}"
            );
            assert!(
                error.to_string().contains("set ABC_SERVER_HOME"),
                "names the fix: {error}"
            );
        }
        ServerHome::at(PathBuf::from(
            "/Users/ada/Library/Application Support/abc/server",
        ))
        .check_paths(Os::MacOs)
        .unwrap();
    }

    #[test]
    fn create_makes_private_directories_and_refuses_open_ones() {
        let scratch = tempfile::tempdir().unwrap();
        let home = ServerHome::at(scratch.path().join("abc/server"));
        assert!(!home.exists(), "not yet");
        home.create().unwrap();
        assert!(home.exists(), "created");
        for dir in [
            home.dir().to_owned(),
            home.tailscale(),
            home.run(),
            home.logs(),
            home.backups(),
        ] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        home.create().unwrap();
        std::fs::set_permissions(home.run(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = home.create().unwrap_err();
        assert!(
            matches!(&error, HostError::Insecure { path } if *path == home.run()),
            "{error:?}"
        );
        assert!(error.to_string().contains("chmod 700"), "{error}");
    }

    #[test]
    fn write_private_replaces_atomically_and_says_whether_it_changed() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("f");
        assert!(write_private(&path, "one").unwrap(), "new");
        assert!(!write_private(&path, "one").unwrap(), "unchanged");
        assert!(write_private(&path, "two").unwrap(), "changed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two", "content");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "private");
        assert!(
            !scratch.path().join("f.new").exists(),
            "nothing staged is left"
        );
        // A crash's leftover, open to others, is overwritten and made private.
        std::fs::write(scratch.path().join("f.new"), "stale and longer").unwrap();
        std::fs::set_permissions(
            scratch.path().join("f.new"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(write_private(&path, "three").unwrap(), "written past it");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "three", "whole");
        let replaced = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(replaced, 0o600, "private again");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(
                write_private(&path, "three"),
                Err(HostError::Insecure { .. })
            ),
            "an unchanged file others can read is refused"
        );
        let dir = scratch.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        assert!(
            matches!(write_private(&dir, "x"), Err(HostError::Io { path: failed, .. }) if failed == dir),
            "what cannot be read is an error, never a new file"
        );
    }
}
