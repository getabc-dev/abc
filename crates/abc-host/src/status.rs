//! `abc server status`: each part of the stack, whether it is healthy, and the exact fix when it
//! is not.

use crate::config::Config;
use crate::ops::{self, Readiness};
use crate::postgres::Postgres;
use crate::services::{Part, ServiceState, Services};
use crate::setup::{lingering, loginctl};
use crate::tailscale::Tailscale;
use crate::{HostError, Os, Server};

/// One part's health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The part.
    pub part: String,
    /// Whether it is healthy.
    pub healthy: bool,
    /// What was found.
    pub detail: String,
    /// What to run when it is not healthy.
    pub fix: Option<String>,
}

impl Check {
    pub(crate) fn ok(part: &str, detail: String) -> Self {
        Self {
            part: part.to_owned(),
            healthy: true,
            detail,
            fix: None,
        }
    }

    pub(crate) fn failing(part: &str, detail: String, fix: String) -> Self {
        Self {
            part: part.to_owned(),
            healthy: false,
            detail,
            fix: Some(fix),
        }
    }
}

/// Every part's health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// One check per part, in the order the stack starts.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether every part is healthy.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|check| check.healthy)
    }

    /// The report as the person reads it: one line a part, and its fix under it.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let width = self
            .checks
            .iter()
            .map(|check| check.part.len())
            .max()
            .unwrap_or_default();
        let mut lines = Vec::new();
        for check in &self.checks {
            let mark = if check.healthy { "ok  " } else { "FAIL" };
            lines.push(format!(
                "{mark}  {part:width$}  {detail}",
                part = check.part,
                detail = check.detail
            ));
            if let Some(fix) = &check.fix {
                lines.push(format!("      fix: {fix}"));
            }
        }
        lines
    }
}

/// The fix for a part setup puts right.
const SETUP: &str = "abc server setup";

/// Looks at every part of the stack.
///
/// # Errors
///
/// [`HostError::NotSetUp`] when there is no server here, and as [`Config::load`].
pub fn status(server: &Server) -> Result<Report, HostError> {
    let (host, home) = (&server.host, &server.home);
    let config = Config::require(home)?;
    let services = Services::new(host, home.logs());
    let mut checks = vec![match config.programs.check(&home.config()) {
        Ok(()) => Check::ok("programs", "abc, its tailscaled and Postgres 18".to_owned()),
        Err(error) => Check::failing("programs", "missing".to_owned(), error.to_string()),
    }];
    checks.extend(Part::STACK.map(|part| {
        service_check(
            part,
            host.os,
            services.state(part),
            &services.log_hint(part),
        )
    }));
    checks.push(postgres_check(server, &config, &services));
    checks.extend(tailnet_checks(server, &config, &services));
    if host.os == Os::Linux {
        checks.push(linger_check(
            loginctl(host).map(|loginctl| lingering(&loginctl, host.uid)),
            host.uid,
            SETUP,
        ));
    }
    checks.push(server_check(&config, &services));
    Ok(Report { checks })
}

/// `part`'s service, found in `state` on `os`, with the command that shows its output.
pub(crate) fn service_check(
    part: Part,
    os: Os,
    state: Result<ServiceState, HostError>,
    log_hint: &str,
) -> Check {
    let name = format!("{} service", part.name());
    let service = part.service_name(os);
    let setup = part.setup_command();
    match state {
        Ok(ServiceState::Running) => Check::ok(&name, format!("{service} is running")),
        Ok(ServiceState::Stopped) => Check::failing(
            &name,
            format!("{service} is installed but not running"),
            format!("{setup} (and read why it stopped: {log_hint})"),
        ),
        Ok(ServiceState::Missing) => Check::failing(
            &name,
            format!("{service} is not installed"),
            setup.to_owned(),
        ),
        Err(error) => Check::failing(&name, "unknown".to_owned(), error.to_string()),
    }
}

fn postgres_check(server: &Server, config: &Config, services: &Services<'_>) -> Check {
    if Postgres::new(&server.home, &config.programs).ready() {
        Check::ok(
            "postgres",
            format!(
                "accepting connections on {}",
                server.home.postgres_socket().display()
            ),
        )
    } else {
        Check::failing(
            "postgres",
            "not accepting connections".to_owned(),
            format!("read {}, then {SETUP}", services.log_hint(Part::Postgres)),
        )
    }
}

/// The node, then Serve, which can only be read once the node runs.
fn tailnet_checks(server: &Server, config: &Config, services: &Services<'_>) -> [Check; 2] {
    let tailscale = Tailscale::new(&server.home, &config.programs);
    let node = match tailscale.status() {
        Ok(node) => node,
        Err(error) => {
            return [
                Check::failing(
                    "tailnet node",
                    format!("abc's tailscaled does not answer: {error}"),
                    format!("read {}, then {SETUP}", services.log_hint(Part::Tailscaled)),
                ),
                Check::failing(
                    "serve",
                    "unknown until abc's node answers".to_owned(),
                    SETUP.to_owned(),
                ),
            ];
        }
    };
    let (true, Some(name), Some(url)) = (node.running(), node.dns_name(), node.public_url()) else {
        return [
            Check::failing(
                "tailnet node",
                format!("not logged in ({})", node.backend_state),
                format!("{SETUP}, which opens the login link"),
            ),
            Check::failing(
                "serve",
                "unknown until abc's node is logged in".to_owned(),
                SETUP.to_owned(),
            ),
        ];
    };
    let node_check = if config.public_url.as_ref() == Some(&url) {
        Check::ok("tailnet node", format!("logged in as {url}"))
    } else {
        Check::failing(
            "tailnet node",
            format!("logged in as {url}, which is not the configured URL"),
            SETUP.to_owned(),
        )
    };
    let wanted = tailscale.wanted_target();
    let serve_check = match tailscale.serve_target(name) {
        Ok(Some(target)) if target == wanted => {
            Check::ok("serve", format!("https://{name}/ goes to {target}"))
        }
        Ok(target) => Check::failing(
            "serve",
            format!(
                "https://{name}/ goes to {}, not {wanted}",
                target.unwrap_or_else(|| "nothing".to_owned())
            ),
            format!("{SETUP}, which asks for your password once to set it"),
        ),
        Err(error) => Check::failing("serve", "unknown".to_owned(), error.to_string()),
    };
    [node_check, serve_check]
}

/// Lingering for user `uid`, found in `lingering`, which `setup` turns on.
pub(crate) fn linger_check(lingering: Result<bool, HostError>, uid: u32, setup: &str) -> Check {
    match lingering {
        Ok(true) => Check::ok(
            "lingering",
            "your services run while you are logged out".to_owned(),
        ),
        Ok(false) => Check::failing(
            "lingering",
            "off: your services stop when you log out".to_owned(),
            format!("{setup}, or sudo loginctl enable-linger {uid}"),
        ),
        Err(error) => Check::failing("lingering", "unknown".to_owned(), error.to_string()),
    }
}

fn server_check(config: &Config, services: &Services<'_>) -> Check {
    let fix = format!("read {}, then {SETUP}", services.log_hint(Part::Server));
    match ops::probe(config.ops_addr) {
        Readiness::Ready => Check::ok("abc server", format!("ready (ops on {})", config.ops_addr)),
        Readiness::NotReady(said) => {
            Check::failing("abc server", format!("not ready: {said}"), fix)
        }
        Readiness::Unreachable(error) => Check::failing(
            "abc server",
            format!("nothing answers on {}: {error}", config.ops_addr),
            fix,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Check, Report};

    #[test]
    fn the_report_aligns_parts_and_puts_each_fix_under_its_failure() {
        let report = Report {
            checks: vec![
                Check::ok("postgres", "accepting".to_owned()),
                Check::failing("abc server", "not ready".to_owned(), "do this".to_owned()),
            ],
        };
        assert!(!report.healthy(), "one fails");
        assert_eq!(
            report.lines(),
            [
                "ok    postgres    accepting",
                "FAIL  abc server  not ready",
                "      fix: do this",
            ],
            "aligned, with the fix"
        );
        let healthy = Report {
            checks: vec![Check::ok("a", "b".to_owned())],
        };
        assert!(healthy.healthy(), "all ok");
    }
}
