//! Each active run's supervisor is a user service of the host, so that a reboot does not end it:
//! a launchd agent on macOS, which launchd starts again at login (`RunAtLoad`), and a systemd user
//! unit on Linux, enabled for the user's default target. Both restart a supervisor that fails and
//! leave one that exits 0, which a supervisor does only once its run has ended.
//!
//! The service runs a copy of the xtask that installed it, kept by its sha256 in the runs
//! directory, so rebuilding or cleaning the main clone never breaks a run. The copy still knows
//! the main clone, where it was built, for msb and the egress list.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, ensure};
use sha2::{Digest as _, Sha256};

use crate::vm::Os;

/// The prefix of every run's launchd label.
const LABEL: &str = "dev.getabc.run.";

/// The services of runs, on one host.
pub(crate) trait Services {
    /// Installs and starts run `run`'s supervisor.
    fn install(&self, run: &str) -> anyhow::Result<()>;
    /// Removes run `run`'s service so that nothing starts it again, and stops its supervisor.
    fn remove(&self, run: &str) -> anyhow::Result<()>;
    /// Removes run `run`'s service from within its own supervisor, which then exits 0: the
    /// service manager does not restart it, and nothing starts it again.
    fn retire(&self, run: &str) -> anyhow::Result<()>;
}

/// The host's service manager.
#[derive(Debug)]
pub(crate) struct Manager {
    pub(crate) os: Os,
    /// Where unit files go: `~/Library/LaunchAgents` or `~/.config/systemd/user`.
    pub(crate) units: PathBuf,
    /// `launchctl` or `systemctl`.
    pub(crate) program: PathBuf,
    /// The xtask to copy, which installs the service: this process's executable.
    pub(crate) exe: PathBuf,
    /// Where the copies go.
    pub(crate) bin: PathBuf,
    /// The environment the supervisor gets: `HOME`, `PATH` and the XDG directories that are set.
    pub(crate) env: Vec<(String, String)>,
    /// The directory of the runs, where each supervisor's own output goes.
    pub(crate) runs: PathBuf,
}

impl Manager {
    /// The service manager of `os` for the user whose home is `home`.
    pub(crate) fn of(
        os: Os,
        home: &Path,
        runs: &Path,
        env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    ) -> anyhow::Result<Self> {
        let config = env("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from);
        let (units, program) = match os {
            Os::MacOs => (home.join("Library/LaunchAgents"), "launchctl"),
            Os::Linux => (config.join("systemd/user"), "systemctl"),
        };
        let mut passed = vec![("HOME".to_owned(), home.display().to_string())];
        for name in ["PATH", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME"] {
            if let Some(value) = env(name).filter(|value| !value.is_empty()) {
                let value = value
                    .into_string()
                    .map_err(|value| anyhow::anyhow!("{name} is not UTF-8: {}", value.display()))?;
                passed.push((name.to_owned(), value));
            }
        }
        Ok(Self {
            os,
            units,
            program: PathBuf::from(program),
            exe: std::env::current_exe().context("could not find this xtask's executable")?,
            bin: runs.join("bin"),
            env: passed,
            runs: runs.to_path_buf(),
        })
    }

    fn unit(&self, run: &str) -> PathBuf {
        match self.os {
            Os::MacOs => self.units.join(format!("{LABEL}{run}.plist")),
            Os::Linux => self.units.join(format!("abc-run-{run}.service")),
        }
    }

    /// The launchd domain of this user's GUI session, `gui/<uid>`.
    fn domain() -> anyhow::Result<String> {
        let output = Command::new("id")
            .arg("-u")
            .stdin(Stdio::null())
            .output()
            .context("could not run id")?;
        ensure!(output.status.success(), "id -u failed");
        Ok(format!(
            "gui/{}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }

    fn manage(&self, args: &[&str]) -> anyhow::Result<()> {
        let program = self.program.display();
        let output = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("could not run {program}"))?;
        ensure!(
            output.status.success(),
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }

    /// Copies this xtask into the runs directory under its sha256, once, and returns the copy.
    fn copy_exe(&self) -> anyhow::Result<PathBuf> {
        let mut file = std::fs::File::open(&self.exe)
            .with_context(|| format!("could not read {}", self.exe.display()))?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = file.read(&mut buffer)?;
            let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
                break;
            };
            hash.update(chunk);
        }
        let digest = hash.finalize();
        let mut hex = String::new();
        for byte in digest.iter().take(16) {
            write!(hex, "{byte:02x}")?;
        }
        let copy = self.bin.join(format!("xtask-{hex}"));
        if !copy.is_file() {
            std::fs::create_dir_all(&self.bin)
                .with_context(|| format!("could not create {}", self.bin.display()))?;
            let partial = self
                .bin
                .join(format!(".xtask-{hex}.{}", std::process::id()));
            std::fs::copy(&self.exe, &partial)
                .with_context(|| format!("could not copy xtask to {}", partial.display()))?;
            std::fs::rename(&partial, &copy)
                .with_context(|| format!("could not move {} into place", copy.display()))?;
        }
        Ok(copy)
    }

    /// Removes the copies of xtask that no unit file runs, but `keep`, the one just installed.
    fn prune(&self, keep: &Path) -> anyhow::Result<()> {
        let mut units = String::new();
        if let Ok(entries) = std::fs::read_dir(&self.units) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(LABEL) || name.starts_with("abc-run-") {
                    units.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
                }
            }
        }
        for entry in std::fs::read_dir(&self.bin)
            .with_context(|| format!("could not read {}", self.bin.display()))?
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path != keep && !units.contains(&path.display().to_string()) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("could not remove {}", path.display()))?;
            }
        }
        Ok(())
    }

    /// The unit file of run `run`'s supervisor, running `exe`.
    pub(crate) fn render(&self, run: &str, exe: &Path) -> String {
        let args = [
            exe.display().to_string(),
            "vm".to_owned(),
            "supervise".to_owned(),
            run.to_owned(),
        ];
        let output = self.runs.join(run).join("supervisor.log");
        match self.os {
            Os::MacOs => {
                let mut plist = String::from(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \
                     \"-//Apple//DTD PLIST 1.0//EN\" \
                     \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist \
                     version=\"1.0\">\n<dict>\n",
                );
                let mut key = |name: &str, value: &str| {
                    plist.push_str("  <key>");
                    plist.push_str(name);
                    plist.push_str("</key>");
                    plist.push_str(value);
                    plist.push('\n');
                };
                key("Label", &string(&format!("{LABEL}{run}")));
                key(
                    "ProgramArguments",
                    &format!(
                        "<array>{}</array>",
                        args.iter().map(|arg| string(arg)).collect::<String>()
                    ),
                );
                key(
                    "EnvironmentVariables",
                    &format!(
                        "<dict>{}</dict>",
                        self.env
                            .iter()
                            .fold(String::new(), |mut all, (name, value)| {
                                all.push_str("<key>");
                                all.push_str(&xml(name));
                                all.push_str("</key>");
                                all.push_str(&string(value));
                                all
                            })
                    ),
                );
                key("RunAtLoad", "<true/>");
                // launchd kills the job's process group when it exits, and the token broker the
                // supervisor starts is in it; the broker serves the VM, which runs on.
                key("AbandonProcessGroup", "<true/>");
                key(
                    "KeepAlive",
                    "<dict><key>SuccessfulExit</key><false/></dict>",
                );
                key("ThrottleInterval", "<integer>60</integer>");
                key("ProcessType", "<string>Background</string>");
                key("StandardOutPath", &string(&output.display().to_string()));
                key("StandardErrorPath", &string(&output.display().to_string()));
                plist.push_str("</dict>\n</plist>\n");
                plist
            }
            Os::Linux => {
                let mut unit = format!(
                    "[Unit]\nDescription=abc agent run {run}\n\n[Service]\nExecStart={}\n",
                    args.iter()
                        .map(|arg| quoted(arg))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                for (name, value) in &self.env {
                    unit.push_str("Environment=");
                    unit.push_str(&quoted(&format!("{name}={value}")));
                    unit.push('\n');
                }
                let output = output.display().to_string();
                for line in [
                    format!("StandardOutput=append:{output}"),
                    format!("StandardError=append:{output}"),
                    // A restarted supervisor leaves the session's token broker running, as
                    // launchd does; the VM it serves runs on.
                    "KillMode=process".to_owned(),
                    "Restart=on-failure".to_owned(),
                    "RestartSec=60".to_owned(),
                    String::new(),
                    "[Install]".to_owned(),
                    "WantedBy=default.target".to_owned(),
                ] {
                    unit.push_str(&line);
                    unit.push('\n');
                }
                unit
            }
        }
    }
}

/// `text` escaped for XML.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `text` as a plist string.
fn string(text: &str) -> String {
    format!("<string>{}</string>", xml(text))
}

/// `text` as one word of a systemd unit: double-quoted, with backslashes, quotes and `%`
/// escaped.
fn quoted(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

impl Services for Manager {
    fn install(&self, run: &str) -> anyhow::Result<()> {
        let exe = self.copy_exe()?;
        std::fs::create_dir_all(&self.units)
            .with_context(|| format!("could not create {}", self.units.display()))?;
        let unit = self.unit(run);
        std::fs::write(&unit, self.render(run, &exe))
            .with_context(|| format!("could not write {}", unit.display()))?;
        let shown = unit.display().to_string();
        let loaded = match self.os {
            Os::MacOs => {
                Self::domain().and_then(|domain| self.manage(&["bootstrap", &domain, &shown]))
            }
            Os::Linux => self.manage(&["--user", "daemon-reload"]).and_then(|()| {
                self.manage(&[
                    "--user",
                    "enable",
                    "--now",
                    &format!("abc-run-{run}.service"),
                ])
            }),
        };
        if let Err(error) = loaded {
            // A unit that did not load is never left for the service manager to start later.
            let _removed = std::fs::remove_file(&unit);
            return Err(error);
        }
        self.prune(&exe)
    }

    fn remove(&self, run: &str) -> anyhow::Result<()> {
        if self.unit(run).is_file() {
            match self.os {
                Os::MacOs => {
                    self.manage(&["bootout", &format!("{}/{LABEL}{run}", Self::domain()?)])?;
                }
                Os::Linux => {
                    self.manage(&["--user", "stop", &format!("abc-run-{run}.service")])?;
                }
            }
        }
        self.retire(run)
    }

    fn retire(&self, run: &str) -> anyhow::Result<()> {
        let unit = self.unit(run);
        if !unit.is_file() {
            return Ok(());
        }
        if self.os == Os::Linux {
            // Without stopping it: the supervisor that retires itself exits on its own.
            self.manage(&["--user", "disable", &format!("abc-run-{run}.service")])?;
        }
        std::fs::remove_file(&unit)
            .with_context(|| format!("could not remove {}", unit.display()))?;
        match self.os {
            // launchd starts nothing at the next login once the file is gone, and a job that
            // exited 0 under `KeepAlive {SuccessfulExit: false}` is not started again before.
            Os::MacOs => Ok(()),
            Os::Linux => self.manage(&["--user", "daemon-reload"]),
        }
    }
}
