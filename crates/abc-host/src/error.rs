//! Why a command failed. Every message ends with what to do about it (P4).

use std::path::{Path, PathBuf};

use thiserror::Error;

/// A failure of `abc server setup` or a day-2 command, with its fix.
#[derive(Debug, Error)]
pub enum HostError {
    /// Neither `ABC_SERVER_HOME` nor `HOME` is set.
    #[error(
        "cannot find a home directory for abc's server: set HOME, or ABC_SERVER_HOME to the \
         directory abc should keep its server in"
    )]
    NoHomeDir,
    /// Someone else can reach a file or directory of abc's, another user owns it, or it is a
    /// link.
    #[error(
        "{} must belong to you, be private to you and not be a link: chmod 700 a directory, or \
         chmod 600 a file, and run the command again",
        path.display()
    )]
    Insecure {
        /// The path.
        path: PathBuf,
    },
    /// A socket would not fit the operating system's limit.
    #[error(
        "{} is {len} bytes long, but {os} binds socket paths of at most {limit} bytes: set \
         ABC_SERVER_HOME to a shorter directory, such as ~/.abc-server, and run abc server setup \
         again",
        path.display()
    )]
    SocketPathTooLong {
        /// The socket's path.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
        /// The most the system allows.
        limit: usize,
        /// The system's name.
        os: &'static str,
    },
    /// The home holds a character Postgres's settings cannot hold.
    #[error(
        "{} holds a double quote or a line break, which Postgres's socket setting cannot hold: \
         set ABC_SERVER_HOME to a directory without one",
        path.display()
    )]
    UnusablePath {
        /// The home.
        path: PathBuf,
    },
    /// A file or directory could not be used.
    #[error(
        "cannot use {}: check that it exists and that you may write to it, then run the command \
         again",
        path.display()
    )]
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// `config.toml` does not parse, or is not this abc's.
    #[error(
        "{} is not a configuration this abc can read ({reason}): correct it, or delete it and run \
         abc server setup again",
        path.display()
    )]
    Config {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// There is no server here yet.
    #[error("abc server is not set up here ({} is missing): run abc server setup", path.display())]
    NotSetUp {
        /// The missing configuration.
        path: PathBuf,
    },
    /// A program abc runs is not where abc looked.
    #[error(
        "{what} is not at {}: reinstall abc (brew reinstall getabc-dev/tap/abc), or set `{key}` \
         under [programs] in {}",
        path.display(),
        config.display()
    )]
    ProgramMissing {
        /// The program, as people know it.
        what: &'static str,
        /// Its key under `[programs]`.
        key: &'static str,
        /// Where abc looked.
        path: PathBuf,
        /// The configuration that can name another place.
        config: PathBuf,
    },
    /// The Postgres programs are not Postgres 18's.
    #[error(
        "{} is not Postgres 18 (it says {found:?}): brew install postgresql@18, or set `postgres` \
         under [programs] in {} to Postgres 18's bin directory",
        path.display(),
        config.display()
    )]
    PostgresVersion {
        /// The `postgres` program asked.
        path: PathBuf,
        /// What it said.
        found: String,
        /// The configuration that can name another place.
        config: PathBuf,
    },
    /// The cluster on disk is another major version's.
    #[error(
        "{} holds a Postgres {found} cluster, but abc runs Postgres 18: move the directory aside, \
         run abc server setup, and load a backup with abc server restore",
        path.display()
    )]
    ClusterVersion {
        /// The cluster's directory.
        path: PathBuf,
        /// Its `PG_VERSION`.
        found: String,
    },
    /// One of the system's own programs is not on the `PATH`.
    #[error("{program} is not on your PATH, and abc needs it {needed_for}: {fix}")]
    SystemProgramMissing {
        /// The program.
        program: &'static str,
        /// What for.
        needed_for: &'static str,
        /// How to get it.
        fix: &'static str,
    },
    /// A program could not be started.
    #[error("could not start `{command}`: {fix}")]
    Spawn {
        /// The command line.
        command: String,
        /// What to do.
        fix: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A program ran and failed.
    #[error("`{command}` failed ({status}): {fix}{}", shown_output(output))]
    Failed {
        /// The command line.
        command: String,
        /// How it ended.
        status: String,
        /// The end of what it printed.
        output: String,
        /// What to do.
        fix: String,
    },
    /// Something did not happen in time.
    #[error("{what} did not happen within {seconds} s: {fix}")]
    Timeout {
        /// What was awaited.
        what: String,
        /// How long abc waited.
        seconds: u64,
        /// What to do.
        fix: String,
    },
    /// abc's tailscaled answered with something abc cannot read.
    #[error(
        "abc's tailscaled answered `{command}` with something abc cannot read ({reason}): check \
         that [programs] in the configuration names Tailscale's own tailscale and tailscaled"
    )]
    TailscaleAnswer {
        /// The command line.
        command: String,
        /// What is wrong with the answer.
        reason: String,
    },
    /// The root step ran, but Serve does not point at abc.
    #[error(
        "Serve on abc's node does not point at {} after the root step: run `{command}` yourself, \
         then abc server setup again",
        socket.display()
    )]
    ServeNotSet {
        /// abc's socket.
        socket: PathBuf,
        /// The command that sets it.
        command: String,
    },
    /// `--purge` was asked to delete a directory that is not abc server's home.
    #[error(
        "{} holds no abc server configuration (config.toml), so it is not abc's to delete: check \
         ABC_SERVER_HOME, or delete the directory yourself if you mean to",
        path.display()
    )]
    NotAbcsHome {
        /// The directory.
        path: PathBuf,
    },
    /// A backup would overwrite a file.
    #[error("{} exists already: name another file, or move this one aside", path.display())]
    BackupExists {
        /// The file.
        path: PathBuf,
    },
}

impl HostError {
    /// Whether the person fixes it in how abc is set up or configured, rather than in what
    /// happened while it ran: the binary exits 2 for these, which a service manager does not
    /// retry. Every variant is decided here, so a new one is decided too.
    #[must_use]
    pub const fn is_setup_problem(&self) -> bool {
        match self {
            Self::NoHomeDir
            | Self::Insecure { .. }
            | Self::SocketPathTooLong { .. }
            | Self::UnusablePath { .. }
            | Self::Config { .. }
            | Self::NotSetUp { .. }
            | Self::ProgramMissing { .. }
            | Self::PostgresVersion { .. }
            | Self::ClusterVersion { .. }
            | Self::SystemProgramMissing { .. }
            | Self::NotAbcsHome { .. }
            | Self::BackupExists { .. } => true,
            Self::Io { .. }
            | Self::Spawn { .. }
            | Self::Failed { .. }
            | Self::Timeout { .. }
            | Self::TailscaleAnswer { .. }
            | Self::ServeNotSet { .. } => false,
        }
    }

    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A program's output, indented under the message, or nothing when it printed nothing.
fn shown_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        let indented: Vec<String> = trimmed.lines().map(|line| format!("  {line}")).collect();
        format!("\n{}", indented.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::HostError;

    #[test]
    fn mistakes_in_the_setup_are_told_from_failures_while_running() {
        let path = || std::path::PathBuf::from("/x");
        for setup in [
            HostError::NoHomeDir,
            HostError::NotAbcsHome { path: path() },
            HostError::NotSetUp { path: path() },
            HostError::BackupExists { path: path() },
        ] {
            assert!(setup.is_setup_problem(), "{setup:?}");
        }
        for running in [
            HostError::Timeout {
                what: "x".to_owned(),
                seconds: 1,
                fix: "y".to_owned(),
            },
            HostError::ServeNotSet {
                socket: path(),
                command: "z".to_owned(),
            },
            HostError::io(&path(), std::io::Error::other("gone")),
        ] {
            assert!(!running.is_setup_problem(), "{running:?}");
        }
    }

    #[test]
    fn a_failed_program_shows_its_output_indented_under_the_fix() {
        let failed = HostError::Failed {
            command: "initdb -D x".to_owned(),
            status: "exit status: 1".to_owned(),
            output: "\nfirst\nsecond\n".to_owned(),
            fix: "read the output".to_owned(),
        };
        assert_eq!(
            failed.to_string(),
            "`initdb -D x` failed (exit status: 1): read the output\n  first\n  second",
            "the output follows the fix"
        );
        let quiet = HostError::Failed {
            command: "x".to_owned(),
            status: "exit status: 2".to_owned(),
            output: " \n".to_owned(),
            fix: "f".to_owned(),
        };
        assert_eq!(
            quiet.to_string(),
            "`x` failed (exit status: 2): f",
            "nothing printed, nothing shown"
        );
    }
}
