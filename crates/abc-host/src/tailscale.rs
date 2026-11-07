//! abc's own tailscaled: a node of its own on the user's tailnet, `abc`, run unprivileged in
//! userspace with its own state and socket, beside whatever Tailscale the user runs.
//!
//! Every CLI call names abc's socket (`--socket=`), so none can reach the user's own daemon, and
//! the daemon listens on no port of its own (`--port=0`).

use std::ffi::OsString;
use std::path::Path;
use std::process::Child;
use std::time::Instant;

use abc_protocol::url::PublicUrl;
use serde::Deserialize;

use crate::process::Invocation;
use crate::programs::Programs;
use crate::{Console, HostError, ServerHome, Timing};

/// The name abc's node asks for. The tailnet may add a suffix if it is taken.
pub(crate) const HOSTNAME: &str = "abc";

/// The fix when abc's tailscaled does not answer.
pub(crate) const NOT_ANSWERING: &str = "check abc's tailscaled with abc server status, and its log";

/// What `tailscale status --json` says about the node, in the parts abc reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct NodeStatus {
    /// `NoState`, `NeedsLogin`, `NeedsMachineAuth`, `Stopped`, `Starting` or `Running`.
    pub(crate) backend_state: String,
    /// The login link, while a login is under way.
    #[serde(rename = "AuthURL", default)]
    pub(crate) auth_url: String,
    /// The node itself.
    #[serde(rename = "Self", default)]
    pub(crate) node: Option<Node>,
}

/// The node's own entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Node {
    /// Its `MagicDNS` name, such as `abc.tail1234.ts.net.`, once it is on the tailnet.
    #[serde(rename = "DNSName", default)]
    pub(crate) dns_name: String,
}

impl NodeStatus {
    pub(crate) fn running(&self) -> bool {
        self.backend_state == "Running"
    }

    /// The node's DNS name without its trailing dot.
    pub(crate) fn dns_name(&self) -> Option<&str> {
        let name = self.node.as_ref()?.dns_name.trim_end_matches('.');
        (!name.is_empty()).then_some(name)
    }

    /// `https://<dns name>`, the URL Serve answers on.
    pub(crate) fn public_url(&self) -> Option<PublicUrl> {
        PublicUrl::parse(&format!("https://{}", self.dns_name()?)).ok()
    }
}

/// abc's tailscaled, through its CLI.
#[derive(Debug)]
pub(crate) struct Tailscale<'a> {
    home: &'a ServerHome,
    programs: &'a Programs,
}

impl<'a> Tailscale<'a> {
    pub(crate) const fn new(home: &'a ServerHome, programs: &'a Programs) -> Self {
        Self { home, programs }
    }

    /// `--socket=<abc's socket>`, which every CLI call starts with.
    fn socket_flag(&self) -> OsString {
        let mut flag = OsString::from("--socket=");
        flag.push(self.home.tailscaled_socket());
        flag
    }

    /// The service's command line.
    pub(crate) fn daemon_command(&self) -> Vec<OsString> {
        let mut state = OsString::from("--statedir=");
        state.push(self.home.tailscale());
        vec![
            self.programs.tailscaled.clone().into_os_string(),
            "--tun=userspace-networking".into(),
            state,
            self.socket_flag(),
            "--port=0".into(),
        ]
    }

    /// abc's CLI, on abc's socket.
    pub(crate) fn cli(&self) -> Invocation {
        Invocation::new(&self.programs.tailscale).arg(self.socket_flag())
    }

    /// What the node says about itself.
    pub(crate) fn status(&self) -> Result<NodeStatus, HostError> {
        let call = self.cli().args(["status", "--json"]);
        let printed = call.run(NOT_ANSWERING)?;
        serde_json::from_str(&printed).map_err(|error| HostError::TailscaleAnswer {
            command: call.shown(),
            reason: error.to_string(),
        })
    }

    /// Waits until the daemon answers on its socket.
    pub(crate) fn wait_answering(&self, timing: &Timing) -> Result<NodeStatus, HostError> {
        let start = Instant::now();
        loop {
            match self.status() {
                Ok(status) => return Ok(status),
                Err(error @ HostError::TailscaleAnswer { .. }) => return Err(error),
                Err(error) if start.elapsed() >= timing.tailscaled => {
                    return Err(HostError::Timeout {
                        what: format!("abc's tailscaled answering ({error})"),
                        seconds: timing.tailscaled.as_secs(),
                        fix: NOT_ANSWERING.to_owned(),
                    });
                }
                Err(_not_yet) => std::thread::sleep(timing.poll),
            }
        }
    }

    /// `tailscale up`, which logs the node in, with an auth key from `auth_key` if given.
    pub(crate) fn up_command(&self, auth_key: Option<&Path>) -> Invocation {
        let up = self.cli().arg("up").arg(format!("--hostname={HOSTNAME}"));
        match auth_key {
            Some(file) => {
                let mut flag = OsString::from("--auth-key=file:");
                flag.push(file);
                up.arg(flag)
            }
            None => up,
        }
    }

    /// Brings the node up and waits until it runs. When it needs a login, `open` is given the
    /// link once, and the console says where it is. Whether the running node has a name is the
    /// caller's question.
    pub(crate) fn log_in(
        &self,
        console: &mut Console<'_>,
        timing: &Timing,
        auth_key: Option<&Path>,
        open: &mut dyn FnMut(&mut Console<'_>, &str),
    ) -> Result<NodeStatus, HostError> {
        let up = self.up_command(auth_key);
        let mut child = up.spawn(NOT_ANSWERING)?;
        let start = Instant::now();
        let mut opened = false;
        let mut approval = false;
        loop {
            let status = self.status()?;
            if status.running() {
                reap(&mut child);
                return Ok(status);
            }
            if !status.auth_url.is_empty() && !opened {
                opened = true;
                console.line("Log abc's node in to your tailnet in the browser:");
                open(console, &status.auth_url);
            }
            if status.backend_state == "NeedsMachineAuth" && !approval {
                approval = true;
                console.line(
                    "Your tailnet approves new devices: approve `abc` at \
                     https://login.tailscale.com/admin/machines",
                );
            }
            if let Some(ended) = child
                .try_wait()
                .map_err(|source| HostError::io(up.program(), source))?
                && !ended.success()
            {
                let output = child
                    .wait_with_output()
                    .map_err(|source| HostError::io(up.program(), source))?;
                return Err(up.failed(
                    ended,
                    &output,
                    "read Tailscale's message above, then run abc server setup again",
                ));
            }
            if start.elapsed() >= timing.login {
                reap(&mut child);
                return Err(HostError::Timeout {
                    what: "abc's node logging in to your tailnet".to_owned(),
                    seconds: timing.login.as_secs(),
                    fix: "run abc server setup again for a new login link".to_owned(),
                });
            }
            std::thread::sleep(timing.poll);
        }
    }

    /// Where Serve sends `https://<dns name>/`, if anywhere.
    pub(crate) fn serve_target(&self, dns_name: &str) -> Result<Option<String>, HostError> {
        let call = self.cli().args(["serve", "status", "--json"]);
        let printed = call.run(NOT_ANSWERING)?;
        parse_serve_target(&printed, dns_name).map_err(|reason| HostError::TailscaleAnswer {
            command: call.shown(),
            reason,
        })
    }

    /// The target Serve must have: abc's socket.
    pub(crate) fn wanted_target(&self) -> String {
        format!("unix:{}", self.home.abc_socket().display())
    }

    /// The command that points Serve at abc's socket, which only root may run.
    pub(crate) fn serve_command(&self) -> Invocation {
        self.cli().args(["serve", "--bg"]).arg(self.wanted_target())
    }

    /// Logs the node out of the tailnet.
    pub(crate) fn log_out(&self) -> Result<(), HostError> {
        self.cli()
            .arg("logout")
            .run(NOT_ANSWERING)
            .map(|_printed| ())
    }
}

/// Stops `tailscale up` if it still runs, and waits for it.
fn reap(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _gone = child.kill();
    }
    let _ended = child.wait();
}

/// Serve's `/` proxy for `<dns_name>:443` in `tailscale serve status --json`'s answer.
fn parse_serve_target(printed: &str, dns_name: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ServeConfig {
        #[serde(default)]
        web: std::collections::BTreeMap<String, WebServer>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct WebServer {
        #[serde(default)]
        handlers: std::collections::BTreeMap<String, Handler>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Handler {
        #[serde(default)]
        proxy: String,
    }
    let config: ServeConfig = serde_json::from_str(printed).map_err(|error| error.to_string())?;
    Ok(config
        .web
        .get(&format!("{dns_name}:443"))
        .and_then(|server| server.handlers.get("/"))
        .map(|handler| handler.proxy.clone())
        .filter(|proxy| !proxy.is_empty()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{NodeStatus, Tailscale, parse_serve_target};
    use crate::ServerHome;
    use crate::programs::Programs;

    fn programs() -> Programs {
        Programs {
            abc: PathBuf::from("/x/abc"),
            tailscaled: PathBuf::from("/opt/abc/libexec/tailscaled"),
            tailscale: PathBuf::from("/opt/abc/libexec/tailscale"),
            postgres: PathBuf::from("/x/pg"),
        }
    }

    #[test]
    fn the_daemon_is_unprivileged_userspace_with_its_own_state_and_socket() {
        let home = ServerHome::at(PathBuf::from("/h"));
        let programs = programs();
        let tailscale = Tailscale::new(&home, &programs);
        let command: Vec<String> = tailscale
            .daemon_command()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            command,
            [
                "/opt/abc/libexec/tailscaled",
                "--tun=userspace-networking",
                "--statedir=/h/tailscale",
                "--socket=/h/run/tailscaled.sock",
                "--port=0",
            ],
            "the flags proved on 7 Nov"
        );
    }

    #[test]
    fn every_cli_call_names_abcs_socket() {
        let home = ServerHome::at(PathBuf::from("/h"));
        let programs = programs();
        let tailscale = Tailscale::new(&home, &programs);
        assert_eq!(
            tailscale.up_command(None).shown(),
            "/opt/abc/libexec/tailscale --socket=/h/run/tailscaled.sock up --hostname=abc",
            "up"
        );
        assert_eq!(
            tailscale.up_command(Some(Path::new("/k/key"))).shown(),
            "/opt/abc/libexec/tailscale --socket=/h/run/tailscaled.sock up --hostname=abc \
             --auth-key=file:/k/key",
            "an auth key from a file (decision 53)"
        );
        assert_eq!(
            tailscale.serve_command().shown(),
            "/opt/abc/libexec/tailscale --socket=/h/run/tailscaled.sock serve --bg \
             unix:/h/run/abc.sock",
            "Serve to abc's socket"
        );
    }

    #[test]
    fn status_is_read_for_state_link_and_name() {
        let needs: NodeStatus = serde_json::from_str(
            r#"{"Version":"1.90.6","BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/x","Self":{"DNSName":"","HostName":"abc"},"Peer":null}"#,
        )
        .unwrap();
        assert!(!needs.running(), "not yet");
        assert_eq!(
            needs.auth_url, "https://login.tailscale.com/a/x",
            "the link"
        );
        assert_eq!(needs.dns_name(), None, "no name before login");
        assert_eq!(needs.public_url(), None, "no URL before login");
        let running: NodeStatus = serde_json::from_str(
            r#"{"BackendState":"Running","Self":{"DNSName":"abc.tail1234.ts.net."}}"#,
        )
        .unwrap();
        assert!(running.running(), "running");
        assert_eq!(running.dns_name(), Some("abc.tail1234.ts.net"), "no dot");
        assert_eq!(
            running.public_url().unwrap().origin(),
            "https://abc.tail1234.ts.net",
            "the public URL"
        );
        let bare: NodeStatus = serde_json::from_str(r#"{"BackendState":"NoState"}"#).unwrap();
        assert_eq!(bare.dns_name(), None, "no Self");
    }

    #[test]
    fn serve_status_is_read_for_the_nodes_root_proxy() {
        let set = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"abc.tail1.ts.net:443":{"Handlers":{"/":{"Proxy":"unix:/h/run/abc.sock"}}}}}"#;
        assert_eq!(
            parse_serve_target(set, "abc.tail1.ts.net").unwrap(),
            Some("unix:/h/run/abc.sock".to_owned()),
            "set"
        );
        assert_eq!(
            parse_serve_target(set, "other.tail1.ts.net").unwrap(),
            None,
            "another name"
        );
        assert_eq!(
            parse_serve_target("{}", "abc.tail1.ts.net").unwrap(),
            None,
            "unset"
        );
        let path = r#"{"Web":{"abc.tail1.ts.net:443":{"Handlers":{"/x":{"Proxy":"http://127.0.0.1:1"}}}}}"#;
        assert_eq!(
            parse_serve_target(path, "abc.tail1.ts.net").unwrap(),
            None,
            "only / counts"
        );
        let text = r#"{"Web":{"abc.tail1.ts.net:443":{"Handlers":{"/":{"Text":"hi"}}}}}"#;
        assert_eq!(
            parse_serve_target(text, "abc.tail1.ts.net").unwrap(),
            None,
            "a handler that proxies nothing"
        );
        assert!(parse_serve_target("not json", "x").is_err(), "unreadable");
    }
}
