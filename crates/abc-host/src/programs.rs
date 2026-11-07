//! The programs the stack runs, and where abc finds them.
//!
//! abc's Homebrew formula (I1, `packaging/homebrew/abc.rb`) installs, beside `bin/abc`:
//!
//! ```text
//! <prefix>/opt/abc/bin/abc
//! <prefix>/opt/abc/libexec/tailscaled     abc's own, at the version the formula pins
//! <prefix>/opt/abc/libexec/tailscale      its CLI, the same version
//! <prefix>/opt/abc/libexec/postgresql  →  postgresql@18's opt bin directory (keg-only)
//! ```
//!
//! None of these is on the `PATH`, and abc never looks there for them: a `tailscale` on the
//! `PATH` may be the user's own, which abc must not touch. Paths go through `opt/abc` rather
//! than the versioned `Cellar` directory, so the services keep working across `brew upgrade`.
//! A path under `[programs]` in `config.toml` wins over all of this.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::HostError;
use crate::process::{Invocation, is_executable};

/// The Postgres major version abc runs.
pub const POSTGRES_MAJOR: &str = "18";

/// The programs the stack runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Programs {
    /// abc itself, which the server's service runs.
    pub abc: PathBuf,
    /// abc's own tailscaled.
    pub tailscaled: PathBuf,
    /// Its CLI.
    pub tailscale: PathBuf,
    /// Postgres 18's bin directory: `initdb`, `postgres`, `pg_isready`, `psql`, `pg_dump` and
    /// `pg_restore`.
    pub postgres: PathBuf,
}

/// The Postgres programs abc runs, by name.
const POSTGRES_PROGRAMS: [&str; 6] = [
    "initdb",
    "postgres",
    "pg_isready",
    "psql",
    "pg_dump",
    "pg_restore",
];

impl Programs {
    /// Where abc's install keeps them, for the abc at `exe` (the running binary).
    #[must_use]
    pub fn from_install(exe: &Path) -> Self {
        let root = install_root(exe);
        let libexec = root.join("libexec");
        Self {
            abc: root.join("bin/abc"),
            tailscaled: libexec.join("tailscaled"),
            tailscale: libexec.join("tailscale"),
            postgres: libexec.join("postgresql"),
        }
    }

    /// One of Postgres's programs.
    pub(crate) fn postgres_program(&self, name: &str) -> PathBuf {
        self.postgres.join(name)
    }

    /// Fails unless every program is there and executable, and the Postgres is 18. `config` is
    /// the file whose `[programs]` can name other paths.
    ///
    /// # Errors
    ///
    /// [`HostError::ProgramMissing`] for the first missing one, [`HostError::PostgresVersion`].
    pub fn check(&self, config: &Path) -> Result<(), HostError> {
        let missing = |what, key, path: &Path| HostError::ProgramMissing {
            what,
            key,
            path: path.to_owned(),
            config: config.to_owned(),
        };
        for (what, key, path) in [
            ("abc", "abc", &self.abc),
            ("abc's tailscaled", "tailscaled", &self.tailscaled),
            ("abc's tailscale CLI", "tailscale", &self.tailscale),
        ] {
            if !is_executable(path) {
                return Err(missing(what, key, path));
            }
        }
        for name in POSTGRES_PROGRAMS {
            let path = self.postgres_program(name);
            if !is_executable(&path) {
                return Err(missing("Postgres 18", "postgres", &path));
            }
        }
        let postgres = self.postgres_program("postgres");
        let said = Invocation::new(&postgres)
            .arg("--version")
            .run("set `postgres` under [programs] to Postgres 18's bin directory")?;
        if postgres_major(&said) == Some(POSTGRES_MAJOR) {
            Ok(())
        } else {
            Err(HostError::PostgresVersion {
                path: postgres,
                found: said.trim().to_owned(),
                config: config.to_owned(),
            })
        }
    }
}

/// The major version in `postgres --version`'s answer, such as `postgres (PostgreSQL) 18.1`.
fn postgres_major(said: &str) -> Option<&str> {
    let version = said.trim().rsplit(' ').next()?;
    version.split(['.', 'b', 'r']).next()
}

/// The directory abc is installed in: Homebrew's `opt/abc` for a binary in its `Cellar`, else the
/// directory above the binary's.
fn install_root(exe: &Path) -> PathBuf {
    let exe = exe
        .canonicalize()
        .unwrap_or_else(|_unresolved| exe.to_owned());
    let keg = exe
        .parent()
        .and_then(Path::parent)
        .map_or_else(PathBuf::new, Path::to_path_buf);
    homebrew_opt(&keg).filter(|opt| opt.exists()).unwrap_or(keg)
}

/// `<prefix>/opt/abc` for a keg `<prefix>/Cellar/abc/<version>`.
fn homebrew_opt(keg: &Path) -> Option<PathBuf> {
    let formula = keg.parent()?;
    let cellar = formula.parent()?;
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|file| file == name);
    (named(formula, "abc") && named(cellar, "Cellar"))
        .then(|| cellar.parent().map(|prefix| prefix.join("opt/abc")))
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{Programs, homebrew_opt, install_root, postgres_major};
    use crate::HostError;

    fn executable(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_homebrew_keg_is_reached_through_its_opt_link() {
        assert_eq!(
            homebrew_opt(Path::new("/opt/homebrew/Cellar/abc/0.2.0")),
            Some(PathBuf::from("/opt/homebrew/opt/abc")),
            "Apple silicon"
        );
        assert_eq!(
            homebrew_opt(Path::new("/home/linuxbrew/.linuxbrew/Cellar/abc/1.0")),
            Some(PathBuf::from("/home/linuxbrew/.linuxbrew/opt/abc")),
            "Linux"
        );
        assert_eq!(homebrew_opt(Path::new("/usr/local/abc")), None, "no Cellar");
        assert_eq!(
            homebrew_opt(Path::new("/x/Cellar/other/1.0")),
            None,
            "another formula"
        );
        assert_eq!(homebrew_opt(Path::new("/")), None, "the root");
    }

    #[test]
    fn programs_come_from_the_install_beside_the_binary() {
        let scratch = tempfile::tempdir().unwrap();
        let prefix = scratch.path().canonicalize().unwrap();
        let keg = prefix.join("Cellar/abc/0.2.0");
        executable(&keg.join("bin/abc"), "");
        // Without the opt link, the keg itself.
        assert_eq!(install_root(&keg.join("bin/abc")), keg, "no opt link yet");
        std::fs::create_dir_all(prefix.join("opt")).unwrap();
        std::os::unix::fs::symlink(&keg, prefix.join("opt/abc")).unwrap();
        // Homebrew's bin link resolves into the Cellar, and the services get the opt path.
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::os::unix::fs::symlink(keg.join("bin/abc"), prefix.join("bin/abc")).unwrap();
        let programs = Programs::from_install(&prefix.join("bin/abc"));
        let opt = prefix.join("opt/abc");
        assert_eq!(
            programs,
            Programs {
                abc: opt.join("bin/abc"),
                tailscaled: opt.join("libexec/tailscaled"),
                tailscale: opt.join("libexec/tailscale"),
                postgres: opt.join("libexec/postgresql"),
            },
            "through opt, which follows upgrades"
        );
        let elsewhere = Programs::from_install(Path::new("/nonexistent/build/bin/abc"));
        assert_eq!(
            elsewhere.tailscale,
            Path::new("/nonexistent/build/libexec/tailscale"),
            "a binary outside Homebrew looks beside itself"
        );
    }

    #[test]
    fn postgres_versions_are_read_from_its_answer() {
        assert_eq!(
            postgres_major("postgres (PostgreSQL) 18.1\n"),
            Some("18"),
            "release"
        );
        assert_eq!(
            postgres_major("postgres (PostgreSQL) 18beta2"),
            Some("18"),
            "beta"
        );
        assert_eq!(
            postgres_major("postgres (PostgreSQL) 17.6"),
            Some("17"),
            "older"
        );
        assert_eq!(
            postgres_major("postgres (PostgreSQL) 19rc1"),
            Some("19"),
            "rc"
        );
    }

    #[test]
    fn check_names_the_first_missing_program_and_where_to_set_it() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        let config = root.join("config.toml");
        let programs = Programs {
            abc: root.join("abc"),
            tailscaled: root.join("tailscaled"),
            tailscale: root.join("tailscale"),
            postgres: root.join("pg"),
        };
        let first = programs.check(&config).unwrap_err();
        assert!(
            matches!(&first, HostError::ProgramMissing { key: "abc", .. }),
            "{first:?}"
        );
        executable(&programs.abc, "");
        executable(&programs.tailscaled, "");
        let cli = programs.check(&config).unwrap_err();
        assert!(
            cli.to_string()
                .contains("set `tailscale` under [programs] in"),
            "{cli}"
        );
        executable(&programs.tailscale, "");
        let postgres = programs.check(&config).unwrap_err();
        assert!(
            matches!(&postgres, HostError::ProgramMissing { key: "postgres", path, .. } if path.ends_with("pg/initdb")),
            "{postgres:?}"
        );
        for name in ["initdb", "pg_isready", "psql", "pg_dump", "pg_restore"] {
            executable(&root.join("pg").join(name), "");
        }
        executable(
            &root.join("pg/postgres"),
            "#!/bin/sh\necho 'postgres (PostgreSQL) 17.6'\n",
        );
        let old = programs.check(&config).unwrap_err();
        assert!(
            matches!(&old, HostError::PostgresVersion { found, .. } if found == "postgres (PostgreSQL) 17.6"),
            "{old:?}"
        );
        assert!(
            old.to_string().contains("brew install postgresql@18"),
            "{old}"
        );
        executable(
            &root.join("pg/postgres"),
            "#!/bin/sh\necho 'postgres (PostgreSQL) 18.1'\n",
        );
        programs.check(&config).unwrap();
    }
}
