//! `abc server status`: each part of the stack, whether it is healthy, and the exact fix when it
//! is not.

use crate::config::Config;
use crate::ops::{self, Readiness};
use crate::postgres::Postgres;
use crate::services::{Part, ServiceState, Services};
use crate::setup::{lingering, loginctl};
use crate::tailscale::Tailscale;
use crate::{Host, HostError, Os};

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
    fn ok(part: &str, detail: String) -> Self {
        Self {
            part: part.to_owned(),
            healthy: true,
            detail,
            fix: None,
        }
    }

    fn failing(part: &str, detail: String, fix: String) -> Self {
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
pub fn status(host: &Host) -> Result<Report, HostError> {
    let config = Config::require(&host.home)?;
    let services = Services::new(host);
    let mut checks = vec![match config.programs.check(&host.home.config()) {
        Ok(()) => Check::ok("programs", "abc, its tailscaled and Postgres 18".to_owned()),
        Err(error) => Check::failing("programs", "missing".to_owned(), error.to_string()),
    }];
    checks.extend(Part::ALL.map(|part| service_check(host, &services, part)));
    checks.push(postgres_check(host, &config, &services));
    checks.extend(tailnet_checks(host, &config, &services));
    if host.os == Os::Linux {
        checks.push(linger_check(host));
    }
    checks.push(server_check(&config, &services));
    Ok(Report { checks })
}

fn service_check(host: &Host, services: &Services<'_>, part: Part) -> Check {
    let name = format!("{} service", part.name());
    let service = part.service_name(host.os);
    match services.state(part) {
        Ok(ServiceState::Running) => Check::ok(&name, format!("{service} is running")),
        Ok(ServiceState::Stopped) => Check::failing(
            &name,
            format!("{service} is installed but not running"),
            format!(
                "{SETUP} (and read why it stopped: {})",
                services.log_hint(part)
            ),
        ),
        Ok(ServiceState::Missing) => Check::failing(
            &name,
            format!("{service} is not installed"),
            SETUP.to_owned(),
        ),
        Err(error) => Check::failing(&name, "unknown".to_owned(), error.to_string()),
    }
}

fn postgres_check(host: &Host, config: &Config, services: &Services<'_>) -> Check {
    if Postgres::new(&host.home, &config.programs).ready() {
        Check::ok(
            "postgres",
            format!(
                "accepting connections on {}",
                host.home.postgres_socket().display()
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
fn tailnet_checks(host: &Host, config: &Config, services: &Services<'_>) -> [Check; 2] {
    let tailscale = Tailscale::new(&host.home, &config.programs);
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

fn linger_check(host: &Host) -> Check {
    match loginctl(host) {
        Ok(loginctl) if lingering(&loginctl, host.uid) => Check::ok(
            "lingering",
            "your services run while you are logged out".to_owned(),
        ),
        Ok(_) => Check::failing(
            "lingering",
            "off: your services stop when you log out".to_owned(),
            format!("{SETUP}, or sudo loginctl enable-linger {}", host.uid),
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
