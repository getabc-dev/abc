//! The scratch machine `abc server setup`'s tests run the real `abc` binary on: the fake service
//! managers and browser of `crates/abc-host/tests/fakes`, which run what each service definition
//! says, the pinned Postgres 18, and Tailscale's programs, fakes or real.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sqlx::PgPool;

/// Postgres 18's bin directory: the pinned `postgresql` tool's, which `cargo xtask check`,
/// `coverage` and `mutants` give the tests. Never one from `PATH`.
pub(crate) fn postgres_bin() -> PathBuf {
    std::env::var_os("ABC_TEST_POSTGRES_BIN")
        .map(PathBuf::from)
        .filter(|bin| bin.join("initdb").is_file())
        .unwrap_or_else(|| {
            panic!(
                "these tests run abc's private Postgres for real: run them through `cargo \
                 xtask check`, which gives them the pinned Postgres 18 (`cargo xtask setup \
                 --for-tests` installs it), or set ABC_TEST_POSTGRES_BIN to a Postgres 18 bin \
                 directory"
            )
        })
}

pub(crate) fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Where a scratch machine's tailscaled and tailscale come from.
#[derive(Debug, Clone)]
pub(crate) enum Tailscale {
    /// The fakes in `crates/abc-host/tests/fakes`, with fake sudo and loginctl.
    Fakes,
    /// Tailscale's own programs in this directory, with the machine's own sudo.
    Real(PathBuf),
}

/// A scratch machine: the fake service managers and browser, the user's home, abc's server home
/// and the configuration naming the real abc, the pinned Postgres and `tailscale`'s programs.
pub(crate) struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    pub(crate) fn new(tailscale: &Tailscale) -> Self {
        // Under /tmp, whatever TMPDIR says: abc's sockets live in the home, so its path is kept
        // short, as a user's is.
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let root = dir.path();
        let fakes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../abc-host/tests/fakes");
        copy_tree(&fakes.join("bin"), &root.join("bin"));
        copy_tree(&fakes.join("programs"), &root.join("programs"));
        let tailscale_bin = match tailscale {
            Tailscale::Fakes => root.join("programs"),
            Tailscale::Real(bin) => {
                // The machine's own sudo runs Serve's root step, and nothing stands in the way
                // of a tailscale that is not abc's.
                for real in ["sudo", "tailscale"] {
                    std::fs::remove_file(root.join("bin").join(real)).unwrap();
                }
                bin.clone()
            }
        };
        for made in ["state", "services", "user/Library", "user/.config/systemd"] {
            std::fs::create_dir_all(root.join(made)).unwrap();
        }
        // Both service directories are the one the fakes read.
        std::os::unix::fs::symlink(
            root.join("services"),
            root.join("user/Library/LaunchAgents"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            root.join("services"),
            root.join("user/.config/systemd/user"),
        )
        .unwrap();
        let home = root.join("h");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ops = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "format = 1\nops_addr = \"{ops}\"\n\n[programs]\nabc = \"{abc}\"\n\
                 tailscaled = \"{tailscale}/tailscaled\"\n\
                 tailscale = \"{tailscale}/tailscale\"\npostgres = \"{postgres}\"\n",
                abc = env!("CARGO_BIN_EXE_abc"),
                tailscale = tailscale_bin.display(),
                postgres = postgres_bin().display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            home.join("config.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        Self { dir }
    }

    pub(crate) fn root(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.root().join("h")
    }

    /// `abc server <args>` as the person runs it here.
    pub(crate) fn abc(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abc"));
        command
            .arg("server")
            .args(args)
            .env_clear()
            .env("HOME", self.root().join("user"))
            .env("ABC_SERVER_HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root().join("bin").display()),
            );
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command.output().unwrap()
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.root().join("state/calls")).unwrap_or_default();
        std::fs::write(self.root().join("state/calls"), "").unwrap();
        calls.lines().map(str::to_owned).collect()
    }

    /// abc's database, through the URL setup gave abc server.
    pub(crate) async fn pool(&self) -> PgPool {
        let url = format!(
            "postgres://abc@localhost/abc?host={}&port=5432",
            self.home()
                .join("run")
                .display()
                .to_string()
                .replace('/', "%2F")
        );
        PgPool::connect(&url).await.unwrap()
    }
}

impl Drop for Machine {
    /// Stops whatever the fake service managers left running, Postgres last and gently.
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(self.root().join("state")) else {
            return;
        };
        let mut pids: Vec<(bool, String)> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pid"))
            .filter_map(|entry| {
                let postgres = entry.file_name().to_string_lossy().contains("postgres");
                std::fs::read_to_string(entry.path())
                    .ok()
                    .map(|pid| (postgres, pid.trim().to_owned()))
            })
            .collect();
        pids.sort();
        for (postgres, pid) in pids {
            let signal = if postgres { "-INT" } else { "-KILL" };
            let _signalled = Command::new("kill").args([signal, &pid]).status();
        }
    }
}
