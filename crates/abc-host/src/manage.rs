//! The day-2 commands: `abc server upgrade`, `backup`, `restore` and `uninstall`.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::home::private;
use crate::ops;
use crate::postgres::{Postgres, backup_name};
use crate::services::{Part, Services};
use crate::setup::definitions;
use crate::tailscale::Tailscale;
use crate::{Console, HostError, Server};

/// The fix when Postgres is not running for a command that needs it.
const POSTGRES_DOWN: &str = "check abc's Postgres with abc server status";

/// After `brew upgrade abc`: writes the services again for the programs now installed, restarts
/// all three, and waits until abc server, which migrates its database as it starts, is ready.
///
/// # Errors
///
/// [`HostError`], each naming its fix.
pub fn upgrade(server: &Server, console: &mut Console<'_>) -> Result<(), HostError> {
    let host = &server.host;
    let config = Config::require(&server.home)?;
    config.programs.check(&server.home.config())?;
    let services = Services::new(host, server.home.logs());
    let postgres = Postgres::new(&server.home, &config.programs);
    let [postgres_service, tailscaled_service, server_service] = definitions(server, &config)?;
    // abc server goes first: Postgres stops only once its sessions end (launchd's SIGTERM is
    // Postgres's smart shutdown), and the server's pool keeps them open.
    services.stop(Part::Server)?;
    for service in [postgres_service, tailscaled_service] {
        if !services.install(&service)? {
            services.restart(service.part)?;
        }
        console.line(&format!("Restarted {}.", service.part.title()));
    }
    postgres.wait_ready(
        &host.timing,
        &format!("read {}", services.log_hint(Part::Postgres)),
    )?;
    // Installing a stopped service starts it, with its definition rewritten if it changed.
    services.install(&server_service)?;
    console.line("Restarted abc server.");
    ops::wait_ready(
        config.ops_addr,
        &host.timing,
        &format!(
            "read {}; a failed migration says why there",
            services.log_hint(Part::Server)
        ),
    )?;
    console.line("abc server is ready; it migrated its database as it started.");
    Ok(())
}

/// A backup written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// The file.
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
}

/// Dumps abc's database to `file`, or to a new file in the home's `backups/`. The file is
/// private (0600), and an existing one is never overwritten.
///
/// # Errors
///
/// [`HostError::BackupExists`], and [`HostError`]s naming their fixes.
pub fn backup(
    server: &Server,
    file: Option<&Path>,
    now: time::OffsetDateTime,
) -> Result<Backup, HostError> {
    let config = Config::require(&server.home)?;
    let postgres = Postgres::new(&server.home, &config.programs);
    let path = file.map_or_else(
        || server.home.backups().join(backup_name(now)),
        Path::to_owned,
    );
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_created) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(HostError::BackupExists { path });
        }
        Err(source) => return Err(HostError::io(&path, source)),
    }
    if let Err(error) = postgres.dump(&path) {
        let _removed = fs::remove_file(&path);
        return Err(error);
    }
    let bytes = fs::metadata(&path)
        .map_err(|source| HostError::io(&path, source))?
        .len();
    Ok(Backup { path, bytes })
}

/// Loads `file`, a backup, in place of abc's database: abc server stops, the dump loads into a
/// fresh database, which then replaces abc's, and abc server starts again. A dump that does not
/// load leaves abc's database as it was. Whether a claimed server may be replaced is the caller's
/// question (`--force`): only the store knows whether it is claimed.
///
/// # Errors
///
/// [`HostError`], each naming its fix.
pub fn restore(server: &Server, file: &Path, console: &mut Console<'_>) -> Result<(), HostError> {
    let host = &server.host;
    let config = Config::require(&server.home)?;
    let postgres = Postgres::new(&server.home, &config.programs);
    postgres.check_dump(file)?;
    if !postgres.ready() {
        return Err(HostError::Failed {
            command: "pg_isready".to_owned(),
            status: "not accepting connections".to_owned(),
            output: String::new(),
            fix: POSTGRES_DOWN.to_owned(),
        });
    }
    let services = Services::new(host, server.home.logs());
    services.stop(Part::Server)?;
    console.line("Stopped abc server.");
    let restored = postgres.restore(file);
    let started = services.start(Part::Server);
    if let Err(load) = restored {
        // The load's failure is the one to fix; the server's, if any, is said beside it.
        if let Err(start) = started {
            console.line(&format!("abc server did not start again either: {start}"));
        }
        return Err(load);
    }
    started?;
    console.line(&format!("Loaded {} into abc's database.", file.display()));
    ops::wait_ready(
        config.ops_addr,
        &host.timing,
        &format!("read {}", services.log_hint(Part::Server)),
    )?;
    console.line("abc server is ready.");
    Ok(())
}

/// Removes the services and logs abc's node out of the tailnet. The data stays unless `purge`.
///
/// # Errors
///
/// [`HostError`], each naming its fix.
pub fn uninstall(server: &Server, purge: bool, console: &mut Console<'_>) -> Result<(), HostError> {
    let config = Config::load(&server.home)?;
    // A purge deletes the home whole, so it must be abc's: holding abc's configuration and
    // private to this user. Checked before anything changes.
    if purge && server.home.exists() {
        if config.is_none() {
            return Err(HostError::NotAbcsHome {
                path: server.home.dir().to_path_buf(),
            });
        }
        private(server.home.dir())?;
    }
    let services = Services::new(&server.host, server.home.logs());
    if let Some(config) = &config {
        let tailscale = Tailscale::new(&server.home, &config.programs);
        match tailscale.log_out() {
            Ok(()) => console.line("Logged abc's node out of your tailnet."),
            Err(error) => console.line(&format!(
                "Could not log abc's node out ({error}); remove it in the admin console below."
            )),
        }
    }
    for part in [Part::Server, Part::Tailscaled, Part::Postgres] {
        services.remove(part)?;
        console.line(&format!("Removed {}'s service.", part.title()));
    }
    console.line(
        "Delete the device `abc` from your tailnet at https://login.tailscale.com/admin/machines \
         (it is listed as logged out).",
    );
    let home = server.home.dir();
    if !server.home.exists() {
        return Ok(());
    }
    if purge {
        fs::remove_dir_all(home).map_err(|source| HostError::io(home, source))?;
        console.line(&format!(
            "Deleted {} and all of abc's data.",
            home.display()
        ));
    } else {
        console.line(&format!(
            "abc's data stays in {}: abc server setup runs it again, and --purge deletes it.",
            home.display()
        ));
    }
    Ok(())
}
