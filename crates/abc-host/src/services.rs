//! The services abc runs as the user: the server stack's three and a worker's one. launchd
//! agents on macOS (`~/Library/LaunchAgents/dev.getabc.*.plist`), systemd user units on Linux
//! (`~/.config/systemd/user/abc-*.service`, with lingering so they run without a login).
//!
//! Each starts at login (or boot, with lingering) and is restarted when it fails. launchd has no
//! dependencies between agents: abc server waits for Postgres itself (`ABC_DATABASE_WAIT`), and
//! Serve's target needs no socket until a request comes. systemd orders abc server after the
//! other two and stops it with Postgres.
//!
//! On macOS each service writes its output to `<name>.log` in the log directory its
//! [`Services`] is given: the server home's `logs/` for the stack, the worker home's for the
//! worker, so neither needs the other's home. On Linux the journal keeps it.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use crate::home::write_private;
use crate::process::{Invocation, find_on_path};
use crate::{Host, HostError, Os};

/// A part of abc that runs as a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// abc's private Postgres.
    Postgres,
    /// abc's own tailscaled.
    Tailscaled,
    /// `abc server`.
    Server,
    /// `abc worker`, on a machine that does the work.
    Worker,
}

impl Part {
    /// The server stack's parts, in the order they start.
    pub const STACK: [Self; 3] = [Self::Postgres, Self::Tailscaled, Self::Server];

    /// The part's short name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Tailscaled => "tailscaled",
            Self::Server => "server",
            Self::Worker => "worker",
        }
    }

    /// What people call it.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Postgres => "abc's Postgres",
            Self::Tailscaled => "abc's tailscaled",
            Self::Server => "abc server",
            Self::Worker => "abc worker",
        }
    }

    /// The command that installs the part's service, and repairs it.
    #[must_use]
    pub const fn setup_command(self) -> &'static str {
        match self {
            Self::Postgres | Self::Tailscaled | Self::Server => "abc server setup",
            Self::Worker => "abc worker setup",
        }
    }

    /// The command that says how the part's service stands.
    #[must_use]
    pub const fn status_command(self) -> &'static str {
        match self {
            Self::Postgres | Self::Tailscaled | Self::Server => "abc server status",
            Self::Worker => "abc worker status",
        }
    }

    /// The launchd label or systemd unit name.
    #[must_use]
    pub fn service_name(self, os: Os) -> String {
        match os {
            Os::MacOs => format!("dev.getabc.{}", self.name()),
            Os::Linux => format!("abc-{}.service", self.name()),
        }
    }
}

/// What a service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Which part.
    pub part: Part,
    /// The program and its arguments.
    pub command: Vec<OsString>,
    /// Its environment, beyond the service manager's.
    pub env: Vec<(String, String)>,
}

/// How a service stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Its definition is not there.
    Missing,
    /// Defined, but not loaded or not enabled.
    Stopped,
    /// Loaded and running.
    Running,
}

/// The service manager of this host, for services whose output on macOS goes to `logs`.
#[derive(Debug)]
pub(crate) struct Services<'a> {
    host: &'a Host,
    logs: PathBuf,
}

impl<'a> Services<'a> {
    /// The manager, with each service's macOS log file `<logs>/<name>.log`.
    pub(crate) const fn new(host: &'a Host, logs: PathBuf) -> Self {
        Self { host, logs }
    }

    /// `part`'s log file on macOS.
    pub(crate) fn log_file(&self, part: Part) -> PathBuf {
        self.logs.join(format!("{}.log", part.name()))
    }

    /// Where `part`'s definition lives.
    pub(crate) fn definition(&self, part: Part) -> PathBuf {
        let name = part.service_name(self.host.os);
        match self.host.os {
            Os::MacOs => self.host.services_dir.join(format!("{name}.plist")),
            Os::Linux => self.host.services_dir.join(name),
        }
    }

    /// `part`'s definition, as this host's service manager reads it.
    pub(crate) fn render(&self, service: &Service) -> String {
        match self.host.os {
            Os::MacOs => launchd_plist(
                &service.part.service_name(Os::MacOs),
                service,
                &self.log_file(service.part),
            ),
            Os::Linux => systemd_unit(service),
        }
    }

    fn manager(&self) -> Result<Invocation, HostError> {
        let (program, fix) = match self.host.os {
            Os::MacOs => ("launchctl", "it is part of macOS; check your PATH"),
            Os::Linux => (
                "systemctl",
                "abc runs as systemd user services on Linux; use a distribution with systemd",
            ),
        };
        let path =
            find_on_path(&self.host.path, program).ok_or(HostError::SystemProgramMissing {
                program,
                needed_for: "to run abc's services",
                fix,
            })?;
        Ok(match self.host.os {
            Os::MacOs => Invocation::new(path),
            Os::Linux => Invocation::new(path).arg("--user"),
        })
    }

    /// `gui/<uid>/<label>`, launchd's name for an agent.
    fn launchd_target(&self, part: Part) -> String {
        format!("gui/{}/{}", self.host.uid, part.service_name(Os::MacOs))
    }

    /// Writes `service`'s definition and has it running with it. An unchanged, running service
    /// is left alone; a changed one is restarted. Returns whether anything changed.
    pub(crate) fn install(&self, service: &Service) -> Result<bool, HostError> {
        let part = service.part;
        let path = self.definition(part);
        fs::create_dir_all(&self.host.services_dir)
            .map_err(|source| HostError::io(&self.host.services_dir, source))?;
        let changed = write_private(&path, &self.render(service))?;
        let fix = format!(
            "check {} with {}, and run {} again",
            part.title(),
            part.status_command(),
            part.setup_command()
        );
        let manager = self.manager()?;
        match self.host.os {
            Os::MacOs => {
                let loaded = self.launchd(part)?;
                if let (Some(running), false) = (loaded, changed) {
                    // Loaded with this definition: started again if it has stopped.
                    if !running {
                        manager
                            .arg("kickstart")
                            .arg(self.launchd_target(part))
                            .run(&fix)?;
                    }
                    return Ok(false);
                }
                if loaded.is_some() {
                    manager
                        .clone()
                        .arg("bootout")
                        .arg(self.launchd_target(part))
                        .run(&fix)?;
                }
                self.bootstrap(&manager, &path, &fix)?;
            }
            Os::Linux => {
                if changed {
                    manager.clone().arg("daemon-reload").run(&fix)?;
                }
                let name = part.service_name(Os::Linux);
                manager.clone().args(["enable", name.as_str()]).run(&fix)?;
                let verb = if changed { "restart" } else { "start" };
                manager.args([verb, name.as_str()]).run(&fix)?;
            }
        }
        Ok(changed)
    }

    /// `launchctl bootstrap`, retried while launchd finishes a `bootout` of the same label,
    /// which it does in the background. The agents run in the user's login session (`gui/<uid>`),
    /// which a Mac has only while someone is logged in at its desktop, so the failure says so.
    fn bootstrap(
        &self,
        manager: &Invocation,
        plist: &std::path::Path,
        fix: &str,
    ) -> Result<(), HostError> {
        let fix = format!(
            "{fix}; abc's services run in your login session, which a Mac has only while you are \
             logged in at its desktop (not over SSH alone): log in there, or turn on automatic \
             login so they start after a restart"
        );
        let fix = fix.as_str();
        let call = manager
            .clone()
            .arg("bootstrap")
            .arg(format!("gui/{}", self.host.uid))
            .arg(plist);
        let start = Instant::now();
        loop {
            match call.run(fix) {
                Ok(_printed) => return Ok(()),
                Err(error) if start.elapsed() >= self.host.timing.service => return Err(error),
                Err(_unloading) => std::thread::sleep(self.host.timing.poll),
            }
        }
    }

    /// Restarts `part`, or starts it if it was stopped.
    pub(crate) fn restart(&self, part: Part) -> Result<(), HostError> {
        let fix = format!("run {} to reinstall {}", part.setup_command(), part.title());
        let manager = self.manager()?;
        match self.host.os {
            Os::MacOs => manager
                .args(["kickstart", "-k"])
                .arg(self.launchd_target(part))
                .run(&fix),
            Os::Linux => manager
                .args(["restart", part.service_name(Os::Linux).as_str()])
                .run(&fix),
        }
        .map(|_printed| ())
    }

    /// Stops `part` until the next restart or install.
    pub(crate) fn stop(&self, part: Part) -> Result<(), HostError> {
        let fix = format!("check {} with {}", part.title(), part.status_command());
        let manager = self.manager()?;
        match self.host.os {
            // An agent that is booted out stays out until it is bootstrapped again.
            Os::MacOs => {
                if self.launchd(part)?.is_none() {
                    return Ok(());
                }
                manager
                    .arg("bootout")
                    .arg(self.launchd_target(part))
                    .run(&fix)
            }
            Os::Linux => manager
                .args(["stop", part.service_name(Os::Linux).as_str()])
                .run(&fix),
        }
        .map(|_printed| ())
    }

    /// Starts `part` after [`Services::stop`].
    pub(crate) fn start(&self, part: Part) -> Result<(), HostError> {
        let fix = format!("run {} to reinstall {}", part.setup_command(), part.title());
        let manager = self.manager()?;
        match self.host.os {
            Os::MacOs => match self.launchd(part)? {
                Some(true) => Ok(()),
                Some(false) => manager
                    .arg("kickstart")
                    .arg(self.launchd_target(part))
                    .run(&fix)
                    .map(|_printed| ()),
                None => self.bootstrap(&manager, &self.definition(part), &fix),
            },
            Os::Linux => manager
                .args(["start", part.service_name(Os::Linux).as_str()])
                .run(&fix)
                .map(|_printed| ()),
        }
    }

    /// Stops `part` for good and deletes its definition.
    pub(crate) fn remove(&self, part: Part) -> Result<(), HostError> {
        let path = self.definition(part);
        if !path.exists() {
            return Ok(());
        }
        let fix = format!(
            "stop {} by hand, then delete {}",
            part.title(),
            path.display()
        );
        let manager = self.manager()?;
        match self.host.os {
            Os::MacOs => self.stop(part)?,
            Os::Linux => {
                manager
                    .clone()
                    .args(["disable", "--now", part.service_name(Os::Linux).as_str()])
                    .run(&fix)?;
            }
        }
        fs::remove_file(&path).map_err(|source| HostError::io(&path, source))?;
        if self.host.os == Os::Linux {
            manager.arg("daemon-reload").run(&fix)?;
        }
        Ok(())
    }

    /// How `part` stands.
    pub(crate) fn state(&self, part: Part) -> Result<ServiceState, HostError> {
        if !self.definition(part).exists() {
            return Ok(ServiceState::Missing);
        }
        let manager = self.manager()?;
        let running = match self.host.os {
            Os::MacOs => self.launchd(part)? == Some(true),
            Os::Linux => {
                let asked = manager
                    .args(["is-active", part.service_name(Os::Linux).as_str()])
                    .output("")?;
                String::from_utf8_lossy(&asked.stdout).trim() == "active"
            }
        };
        Ok(if running {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        })
    }

    /// Whether launchd has `part` loaded (`None` when not), and if so whether its process runs:
    /// `launchctl print` succeeds for a loaded agent, and says `state = running` while it runs.
    fn launchd(&self, part: Part) -> Result<Option<bool>, HostError> {
        let printed = self
            .manager()?
            .arg("print")
            .arg(self.launchd_target(part))
            .output("")?;
        if !printed.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&printed.stdout)
                .lines()
                .any(|line| line.trim() == "state = running"),
        ))
    }

    /// The command that shows `part`'s recent output.
    pub(crate) fn log_hint(&self, part: Part) -> String {
        match self.host.os {
            Os::MacOs => format!("tail -n 50 '{}'", self.log_file(part).display()),
            Os::Linux => format!(
                "journalctl --user -u {} -n 50",
                part.service_name(Os::Linux)
            ),
        }
    }
}

/// `text` escaped for XML.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A launchd agent: started at login, kept alive, private files, output to `log`.
///
/// The stack's agents are kept alive whatever ends them. The worker's is restarted only when it
/// fails (`KeepAlive` with `SuccessfulExit` false), so a worker that exits 0 stays down. launchd
/// cannot tell one failing exit status from another: unlike systemd's
/// `RestartPreventExitStatus=2 3`, it restarts a worker that exited 2 (its home needs fixing) or
/// 3 (the server told it to stop), at most once every ten seconds (launchd's default
/// `ThrottleInterval`). Each such start finds the same refusal and exits again without working,
/// and `abc worker status` and the log say why. The closest correct alternative, no `KeepAlive`,
/// would leave a worker down after any crash, which is worse.
fn launchd_plist(label: &str, service: &Service, log: &std::path::Path) -> String {
    let mut plist = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let line = |into: &mut String, text: &str| {
        into.push_str(text);
        into.push('\n');
    };
    line(&mut plist, "\t<key>Label</key>");
    line(&mut plist, &format!("\t<string>{}</string>", xml(label)));
    line(&mut plist, "\t<key>ProgramArguments</key>");
    line(&mut plist, "\t<array>");
    for arg in &service.command {
        line(
            &mut plist,
            &format!("\t\t<string>{}</string>", xml(&arg.to_string_lossy())),
        );
    }
    line(&mut plist, "\t</array>");
    if !service.env.is_empty() {
        line(&mut plist, "\t<key>EnvironmentVariables</key>");
        line(&mut plist, "\t<dict>");
        for (name, value) in &service.env {
            line(&mut plist, &format!("\t\t<key>{}</key>", xml(name)));
            line(&mut plist, &format!("\t\t<string>{}</string>", xml(value)));
        }
        line(&mut plist, "\t</dict>");
    }
    let log = xml(&log.to_string_lossy());
    line(&mut plist, "\t<key>RunAtLoad</key>");
    line(&mut plist, "\t<true/>");
    line(&mut plist, "\t<key>KeepAlive</key>");
    match service.part {
        Part::Postgres | Part::Tailscaled | Part::Server => line(&mut plist, "\t<true/>"),
        Part::Worker => {
            for text in [
                "\t<dict>",
                "\t\t<key>SuccessfulExit</key>",
                "\t\t<false/>",
                "\t</dict>",
            ] {
                line(&mut plist, text);
            }
        }
    }
    for text in [
        // 077: whatever a service creates is private to the user.
        "\t<key>Umask</key>",
        "\t<integer>63</integer>",
        "\t<key>StandardOutPath</key>",
        &format!("\t<string>{log}</string>"),
        "\t<key>StandardErrorPath</key>",
        &format!("\t<string>{log}</string>"),
        "</dict>",
        "</plist>",
    ] {
        line(&mut plist, text);
    }
    plist
}

/// `word` quoted for systemd's command lines and assignments: specifiers (`%`) and variables
/// (`$`) doubled, backslashes and quotes escaped.
fn systemd_word(word: &str) -> String {
    let escaped = word
        .replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// A systemd user unit: restarted on failure, wanted by the user's default target.
fn systemd_unit(service: &Service) -> String {
    let other = |part: Part| part.service_name(Os::Linux);
    let mut lines = vec![
        "[Unit]".to_owned(),
        format!("Description={}", service.part.title()),
    ];
    match service.part {
        Part::Postgres | Part::Tailscaled | Part::Worker => {}
        Part::Server => {
            lines.push(format!("Requires={}", other(Part::Postgres)));
            lines.push(format!(
                "After={} {}",
                other(Part::Postgres),
                other(Part::Tailscaled)
            ));
        }
    }
    let command: Vec<String> = service
        .command
        .iter()
        .map(|arg| systemd_word(&arg.to_string_lossy()))
        .collect();
    lines.extend([
        String::new(),
        "[Service]".to_owned(),
        "Type=simple".to_owned(),
        format!("ExecStart={}", command.join(" ")),
    ]);
    lines.extend(
        service.env.iter().map(|(name, value)| {
            format!("Environment={}", systemd_word(&format!("{name}={value}")))
        }),
    );
    lines.extend(["Restart=on-failure", "RestartSec=2", "UMask=0077"].map(str::to_owned));
    match service.part {
        // Postgres's fast shutdown: open sessions are ended rather than waited for.
        Part::Postgres => {
            lines.extend(["KillSignal=SIGINT", "KillMode=mixed"].map(str::to_owned));
        }
        Part::Tailscaled => {}
        // A configuration error (exit 2) or the server's own refusal (3) does not heal itself:
        // for the worker, 3 is the server telling it to stop (revoked, superseded, no common
        // protocol), which a person has to act on.
        Part::Server | Part::Worker => lines.push("RestartPreventExitStatus=2 3".to_owned()),
    }
    lines.extend(["", "[Install]", "WantedBy=default.target", ""].map(str::to_owned));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Part, Service, launchd_plist, systemd_unit, systemd_word, xml};
    use crate::Os;

    fn server() -> Service {
        Service {
            part: Part::Server,
            command: vec![
                "/opt/abc/bin/abc".into(),
                "server".into(),
                "--public-url".into(),
                "https://abc.t.ts.net".into(),
            ],
            env: vec![(
                "ABC_DATABASE_URL".to_owned(),
                "postgres://abc@localhost/abc?host=%2Fh%20x%2Frun".to_owned(),
            )],
        }
    }

    #[test]
    fn services_are_named_for_each_manager() {
        assert_eq!(
            Part::STACK.map(|part| part.service_name(Os::MacOs)),
            [
                "dev.getabc.postgres",
                "dev.getabc.tailscaled",
                "dev.getabc.server"
            ],
            "launchd labels"
        );
        assert_eq!(
            Part::STACK.map(|part| part.service_name(Os::Linux)),
            [
                "abc-postgres.service",
                "abc-tailscaled.service",
                "abc-server.service"
            ],
            "systemd units"
        );
        assert_eq!(
            Part::STACK.map(Part::title),
            ["abc's Postgres", "abc's tailscaled", "abc server"],
            "titles"
        );
        assert_eq!(
            [
                Part::Worker.service_name(Os::MacOs),
                Part::Worker.service_name(Os::Linux),
            ],
            ["dev.getabc.worker", "abc-worker.service"],
            "the worker's"
        );
        assert_eq!(
            Part::STACK.map(|part| (part.setup_command(), part.status_command())),
            [("abc server setup", "abc server status"); 3],
            "the stack is the server's"
        );
        assert_eq!(
            (Part::Worker.setup_command(), Part::Worker.status_command()),
            ("abc worker setup", "abc worker status"),
            "the worker is its own"
        );
    }

    #[test]
    fn a_launchd_agent_runs_at_load_is_kept_alive_and_escapes_its_text() {
        let plist = launchd_plist(
            "dev.getabc.server",
            &server(),
            Path::new("/h/logs/server.log"),
        );
        assert_eq!(
            plist,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>dev.getabc.server</string>
\t<key>ProgramArguments</key>
\t<array>
\t\t<string>/opt/abc/bin/abc</string>
\t\t<string>server</string>
\t\t<string>--public-url</string>
\t\t<string>https://abc.t.ts.net</string>
\t</array>
\t<key>EnvironmentVariables</key>
\t<dict>
\t\t<key>ABC_DATABASE_URL</key>
\t\t<string>postgres://abc@localhost/abc?host=%2Fh%20x%2Frun</string>
\t</dict>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<true/>
\t<key>Umask</key>
\t<integer>63</integer>
\t<key>StandardOutPath</key>
\t<string>/h/logs/server.log</string>
\t<key>StandardErrorPath</key>
\t<string>/h/logs/server.log</string>
</dict>
</plist>
",
            "the whole agent"
        );
        let bare = Service {
            part: Part::Tailscaled,
            command: vec!["/a&b/<c>".into()],
            env: Vec::new(),
        };
        let escaped = launchd_plist("dev.getabc.tailscaled", &bare, Path::new("/l"));
        assert!(
            escaped.contains("<string>/a&amp;b/&lt;c&gt;</string>"),
            "escaped: {escaped}"
        );
        assert!(
            !escaped.contains("EnvironmentVariables"),
            "no empty environment"
        );
        assert_eq!(xml("\"'"), "&quot;&apos;", "quotes");
    }

    #[test]
    fn a_systemd_unit_orders_the_server_after_its_database_and_quotes_every_word() {
        assert_eq!(
            systemd_unit(&server()),
            "[Unit]
Description=abc server
Requires=abc-postgres.service
After=abc-postgres.service abc-tailscaled.service

[Service]
Type=simple
ExecStart=\"/opt/abc/bin/abc\" \"server\" \"--public-url\" \"https://abc.t.ts.net\"
Environment=\"ABC_DATABASE_URL=postgres://abc@localhost/abc?host=%%2Fh%%20x%%2Frun\"
Restart=on-failure
RestartSec=2
UMask=0077
RestartPreventExitStatus=2 3

[Install]
WantedBy=default.target
",
            "the whole unit"
        );
        let postgres = systemd_unit(&Service {
            part: Part::Postgres,
            command: vec!["/pg/postgres".into()],
            env: Vec::new(),
        });
        assert!(
            postgres.contains("KillSignal=SIGINT\nKillMode=mixed\n"),
            "fast shutdown: {postgres}"
        );
        assert!(!postgres.contains("Requires="), "Postgres needs nothing");
        let tailscaled = systemd_unit(&Service {
            part: Part::Tailscaled,
            command: vec!["/t".into()],
            env: Vec::new(),
        });
        assert!(
            !tailscaled.contains("KillSignal") && !tailscaled.contains("RestartPrevent"),
            "{tailscaled}"
        );
        assert_eq!(
            systemd_word(r#"a\b"c%d$e f"#),
            r#""a\\b\"c%%d$$e f""#,
            "every special character"
        );
    }

    #[test]
    fn definitions_live_where_each_manager_reads_them() {
        let mut host = crate::Host {
            os: Os::MacOs,
            user_home: PathBuf::from("/Users/ada"),
            services_dir: PathBuf::from("/Users/ada/Library/LaunchAgents"),
            uid: 501,
            path: std::ffi::OsString::new(),
            timing: crate::Timing::production(),
        };
        let services = super::Services::new(&host, PathBuf::from("/h/logs"));
        assert_eq!(
            services.definition(Part::Postgres),
            Path::new("/Users/ada/Library/LaunchAgents/dev.getabc.postgres.plist"),
            "launchd"
        );
        assert_eq!(
            services.launchd_target(Part::Server),
            "gui/501/dev.getabc.server",
            "domain"
        );
        assert_eq!(
            services.log_hint(Part::Server),
            "tail -n 50 '/h/logs/server.log'",
            "the log file"
        );
        host.os = Os::Linux;
        host.services_dir = PathBuf::from("/home/ada/.config/systemd/user");
        let systemd = super::Services::new(&host, PathBuf::from("/h/logs"));
        assert_eq!(
            systemd.definition(Part::Server),
            Path::new("/home/ada/.config/systemd/user/abc-server.service"),
            "systemd"
        );
        assert_eq!(
            systemd.log_hint(Part::Tailscaled),
            "journalctl --user -u abc-tailscaled.service -n 50",
            "the journal"
        );
    }
}
