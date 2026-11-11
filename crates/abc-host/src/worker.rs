//! A worker's service: `abc worker` kept running on its machine as the user who set it up, from
//! login (or boot, with lingering on Linux) and after it fails, by the same service code as the
//! server's stack ([`crate::services`]).
//!
//! ```text
//! macOS  ~/Library/LaunchAgents/dev.getabc.worker.plist   output in <worker home>/logs/worker.log
//! Linux  ~/.config/systemd/user/abc-worker.service        output in the journal; lingering on
//! ```
//!
//! The service runs `<abc> worker --home <worker home>` with its environment stated in the
//! definition (`HOME`, `ABC_WORKER_HOME`, `ABC_LOG=info`), since a service manager passes next to
//! none. Exit status 3, the server telling the worker to stop, and 2, a home to fix, are not
//! restarted by systemd; launchd cannot single out an exit status, and what it does instead is
//! said at [`crate::services`]'s plist. None of this needs abc server's home: the worker's own
//! home holds its key and, on macOS, its log.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use crate::home::private;
use crate::root::RootStep;
use crate::services::{Part, Service, ServiceState, Services};
use crate::setup::{check_linger_took, lingering, loginctl, missing_linger, run_as_root};
use crate::status::{Report, linger_check, service_check};
use crate::{Console, Host, HostError, Os};

/// The command that installs the worker's service, which its fixes name.
const SETUP: &str = "abc worker setup";

/// What the worker's service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerService {
    /// The abc executable the service runs: the installed abc, or the running one resolved.
    pub abc: PathBuf,
    /// The worker's home, holding its key (`worker.toml`) and, on macOS, its log.
    pub home: PathBuf,
}

impl WorkerService {
    /// The service: `<abc> worker --home <home>`, with `HOME`, `ABC_WORKER_HOME` and
    /// `ABC_LOG=info` stated.
    fn service(&self, host: &Host) -> Result<Service, HostError> {
        let abc = service_path(&self.abc)?;
        let home = service_path(&self.home)?;
        let user_home = service_path(&host.user_home)?;
        Ok(Service {
            part: Part::Worker,
            command: vec![abc.into(), "worker".into(), "--home".into(), home.into()],
            env: vec![
                ("HOME".to_owned(), user_home.to_owned()),
                ("ABC_WORKER_HOME".to_owned(), home.to_owned()),
                ("ABC_LOG".to_owned(), "info".to_owned()),
            ],
        })
    }
}

/// `path` as a service definition holds it: absolute, since the service starts elsewhere, and
/// UTF-8, since launchd's plist and systemd's unit are text.
fn service_path(path: &Path) -> Result<&str, HostError> {
    let error = |reason| HostError::ServicePath {
        path: path.to_path_buf(),
        reason,
    };
    if !path.is_absolute() {
        return Err(error(
            "is relative, and a service starts in another directory than this command: name it \
             from /",
        ));
    }
    path.to_str().ok_or_else(|| {
        error("is not UTF-8, which a service definition cannot hold: choose a path that is")
    })
}

/// The worker's log directory on macOS.
fn logs(home: &Path) -> PathBuf {
    home.join("logs")
}

/// Installs the worker's service and has it running: writes its definition (0600), creates the
/// log directory (0700), loads and starts the service, or restarts it when the definition
/// changed, and on Linux enables lingering, through one `sudo` it explains on `console` first,
/// so the worker starts at boot and keeps running after logout. Running it again with the same
/// `spec` changes nothing. Returns whether anything changed.
///
/// # Errors
///
/// [`HostError::ServicePath`] for a path a service cannot be given, [`HostError::Insecure`] for
/// a log directory others can reach, and [`HostError`]s from the service manager, `loginctl`
/// and `sudo`, each naming its fix.
pub fn install_worker(
    host: &Host,
    spec: &WorkerService,
    console: &mut Console<'_>,
) -> Result<bool, HostError> {
    let service = spec.service(host)?;
    let logs = logs(&spec.home);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&logs)
        .map_err(|source| HostError::io(&logs, source))?;
    private(&logs)?;
    let mut changed = Services::new(host, logs).install(&service)?;
    if let Some(linger) = missing_linger(host)? {
        let step = RootStep::Linger(linger);
        let command = run_as_root(host, &step, SETUP, console)?;
        check_linger_took(&step, &command)?;
        changed = true;
    }
    Ok(changed)
}

/// How the worker's service stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
    /// Whether it is installed and running.
    pub state: ServiceState,
    /// On Linux, whether the user's services run without a login (at boot, after logout);
    /// `None` on macOS, where an agent runs in the login session.
    pub lingering: Option<bool>,
    /// Where its definition is.
    pub definition: PathBuf,
    /// The command that shows its recent output.
    pub log: String,
    os: Os,
    uid: u32,
}

impl WorkerStatus {
    /// Whether the worker runs and will run again after a reboot.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.report().healthy()
    }

    /// What to run to make it healthy, or `None` when it is.
    #[must_use]
    pub fn fix(&self) -> Option<String> {
        self.report().checks.into_iter().find_map(|check| check.fix)
    }

    /// The status as the person reads it: the service, lingering on Linux, each with its fix
    /// under it, then where the definition and the log are.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = self.report().lines();
        lines.push(format!("      definition: {}", self.definition.display()));
        lines.push(format!("      log: {}", self.log));
        lines
    }

    fn report(&self) -> Report {
        let mut checks = vec![service_check(
            Part::Worker,
            self.os,
            Ok(self.state),
            &self.log,
        )];
        checks.extend(
            self.lingering
                .map(|on| linger_check(Ok(on), self.uid, SETUP)),
        );
        Report { checks }
    }
}

/// How the worker's service on `host` stands; `home` is the worker's home, where its log is on
/// macOS.
///
/// # Errors
///
/// [`HostError`]s from the service manager and `loginctl`, each naming its fix.
pub fn worker_status(host: &Host, home: &Path) -> Result<WorkerStatus, HostError> {
    let services = Services::new(host, logs(home));
    let state = services.state(Part::Worker)?;
    let lingering = match host.os {
        Os::MacOs => None,
        Os::Linux => Some(lingering(&loginctl(host)?, host.uid)),
    };
    Ok(WorkerStatus {
        state,
        lingering,
        definition: services.definition(Part::Worker),
        log: services.log_hint(Part::Worker),
        os: host.os,
        uid: host.uid,
    })
}

/// Stops the worker's service and deletes its definition. The worker's home, which holds its key
/// and logs, stays unless `purge`, which deletes it whole once it is found to be a directory of
/// its own: not a link, not `/`, neither the user's home nor a directory holding it, and private
/// to the user. That is checked before anything changes. What became of each is said on
/// `console`. Lingering stays on: other services of the user's may need it.
///
/// # Errors
///
/// [`HostError::UnsafePurge`], [`HostError::ServicePath`] for a relative home and
/// [`HostError::Insecure`] when a purge must not delete the home, and [`HostError`]s from the
/// service manager and the file system, each naming its fix.
pub fn uninstall_worker(
    host: &Host,
    home: &Path,
    purge: bool,
    console: &mut Console<'_>,
) -> Result<(), HostError> {
    let present = if purge {
        purgeable(host, home)?
    } else {
        home.is_dir()
    };
    let services = Services::new(host, logs(home));
    if services.definition(Part::Worker).exists() {
        services.remove(Part::Worker)?;
        console.line("Removed abc worker's service.");
    } else {
        console.line("abc worker's service was not installed.");
    }
    if !present {
        return Ok(());
    }
    if purge {
        fs::remove_dir_all(home).map_err(|source| HostError::io(home, source))?;
        console.line(&format!(
            "Deleted {} with the worker's key: enrol this machine again to work here.",
            home.display()
        ));
    } else {
        console.line(&format!(
            "The worker's key and logs stay in {}: {SETUP} runs it again, and --purge deletes \
             them.",
            home.display()
        ));
    }
    Ok(())
}

/// Whether there is a worker home at `home` to purge (`false` when nothing is there), failing
/// unless deleting it whole deletes only the worker's own directory.
fn purgeable(host: &Host, home: &Path) -> Result<bool, HostError> {
    service_path(home)?;
    let refuse = |reason| HostError::UnsafePurge {
        path: home.to_path_buf(),
        reason,
    };
    let metadata = match fs::symlink_metadata(home) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(HostError::io(home, source)),
    };
    if metadata.file_type().is_symlink() {
        return Err(refuse("is a symbolic link"));
    }
    if !metadata.is_dir() {
        return Err(refuse("is not a directory"));
    }
    let real = fs::canonicalize(home).map_err(|source| HostError::io(home, source))?;
    if real.parent().is_none() {
        return Err(refuse("is the root directory"));
    }
    let user_home = fs::canonicalize(&host.user_home).unwrap_or_else(|_| host.user_home.clone());
    if user_home.starts_with(&real) {
        return Err(refuse("is your home directory, or holds it"));
    }
    if !WORKER_HOME_FILES
        .iter()
        .any(|name| home.join(name).is_file())
    {
        return Err(refuse(
            "holds no worker.toml or worker.lock, so it is not an abc worker's home",
        ));
    }
    private(home)?;
    Ok(true)
}

/// The files a worker's home holds once the worker has run in it, one of which a directory must
/// have to be deleted as that home.
const WORKER_HOME_FILES: [&str; 2] = ["worker.toml", "worker.lock"];

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{WorkerService, WorkerStatus, service_path};
    use crate::services::{ServiceState, Services};
    use crate::{Host, HostError, Os, Timing};

    fn host(os: Os) -> Host {
        Host {
            os,
            user_home: PathBuf::from("/Users/ada"),
            services_dir: PathBuf::from("/Users/ada/Library/LaunchAgents"),
            uid: 501,
            path: OsString::new(),
            timing: Timing::production(),
        }
    }

    fn spec() -> WorkerService {
        WorkerService {
            abc: PathBuf::from("/opt/homebrew/bin/abc"),
            home: PathBuf::from("/Users/ada/Library/Application Support/abc/worker"),
        }
    }

    fn rendered(os: Os) -> String {
        let host = host(os);
        let home = Path::new("/Users/ada/Library/Application Support/abc/worker");
        Services::new(&host, home.join("logs")).render(&spec().service(&host).unwrap())
    }

    #[test]
    fn the_launchd_agent_runs_at_login_restarts_failures_and_logs_in_the_workers_home() {
        assert_eq!(
            rendered(Os::MacOs),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>dev.getabc.worker</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/opt/homebrew/bin/abc</string>
\t\t<string>worker</string>
\t\t<string>--home</string>
\t\t<string>/Users/ada/Library/Application Support/abc/worker</string>
\t</array>
\t<key>EnvironmentVariables</key>
\t<dict>
\t\t<key>HOME</key>
\t\t<string>/Users/ada</string>
\t\t<key>ABC_WORKER_HOME</key>
\t\t<string>/Users/ada/Library/Application Support/abc/worker</string>
\t\t<key>ABC_LOG</key>
\t\t<string>info</string>
\t</dict>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
\t<key>Umask</key>
\t<integer>63</integer>
\t<key>StandardOutPath</key>
\t<string>/Users/ada/Library/Application Support/abc/worker/logs/worker.log</string>
\t<key>StandardErrorPath</key>
\t<string>/Users/ada/Library/Application Support/abc/worker/logs/worker.log</string>
</dict>
</plist>
",
            "the whole agent"
        );
    }

    #[test]
    fn the_systemd_unit_starts_at_boot_and_never_restarts_a_worker_told_to_stop() {
        assert_eq!(
            rendered(Os::Linux),
            "[Unit]
Description=abc worker

[Service]
Type=simple
ExecStart=\"/opt/homebrew/bin/abc\" \"worker\" \"--home\" \"/Users/ada/Library/Application Support/abc/worker\"
Environment=\"HOME=/Users/ada\"
Environment=\"ABC_WORKER_HOME=/Users/ada/Library/Application Support/abc/worker\"
Environment=\"ABC_LOG=info\"
Restart=on-failure
RestartSec=2
UMask=0077
RestartPreventExitStatus=2 3

[Install]
WantedBy=default.target
",
            "the whole unit"
        );
    }

    #[test]
    fn a_service_is_given_only_absolute_utf8_paths() {
        let relative = WorkerService {
            abc: PathBuf::from("abc"),
            home: PathBuf::from("/w"),
        };
        assert!(
            matches!(
                relative.service(&host(Os::Linux)),
                Err(HostError::ServicePath { path, reason }) if path == Path::new("abc")
                    && reason.starts_with("is relative")
            ),
            "relative abc"
        );
        let mut no_home = host(Os::Linux);
        no_home.user_home = PathBuf::from("ada");
        assert!(
            matches!(spec().service(&no_home), Err(HostError::ServicePath { .. })),
            "relative HOME"
        );
        let bytes = <OsString as std::os::unix::ffi::OsStringExt>::from_vec(b"/w\xff".to_vec());
        assert!(
            matches!(
                service_path(Path::new(&bytes)),
                Err(HostError::ServicePath { reason, .. }) if reason.starts_with("is not UTF-8")
            ),
            "not UTF-8"
        );
    }

    fn status(os: Os, state: ServiceState, lingering: Option<bool>) -> WorkerStatus {
        WorkerStatus {
            state,
            lingering,
            definition: PathBuf::from("/d/abc-worker.service"),
            log: "journalctl --user -u abc-worker.service -n 50".to_owned(),
            os,
            uid: 1000,
        }
    }

    #[test]
    fn the_status_names_the_fix_for_each_state() {
        let running = status(Os::Linux, ServiceState::Running, Some(true));
        assert!(running.healthy(), "running, lingering");
        assert_eq!(running.fix(), None, "nothing to fix");
        assert_eq!(
            running.lines(),
            [
                "ok    worker service  abc-worker.service is running",
                "ok    lingering       your services run while you are logged out",
                "      definition: /d/abc-worker.service",
                "      log: journalctl --user -u abc-worker.service -n 50",
            ],
            "every line"
        );
        let stopped = status(Os::Linux, ServiceState::Stopped, Some(true));
        assert!(!stopped.healthy(), "stopped");
        assert_eq!(
            stopped.fix().as_deref(),
            Some(
                "abc worker setup (and read why it stopped: journalctl --user -u \
                 abc-worker.service -n 50)"
            ),
            "read the log, set up again"
        );
        let missing = status(Os::MacOs, ServiceState::Missing, None);
        assert_eq!(
            missing.fix().as_deref(),
            Some("abc worker setup"),
            "install it"
        );
        assert_eq!(
            missing.lines()[0],
            "FAIL  worker service  dev.getabc.worker is not installed",
            "launchd's name"
        );
        assert_eq!(missing.lines().len(), 4, "no lingering on macOS");
        let alone = status(Os::Linux, ServiceState::Running, Some(false));
        assert!(!alone.healthy(), "it would not survive a logout");
        assert_eq!(
            alone.fix().as_deref(),
            Some("abc worker setup, or sudo loginctl enable-linger 1000"),
            "lingering's fix"
        );
    }
}
