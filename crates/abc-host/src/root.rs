//! The one step that runs as root, and what it may do.
//!
//! Tailscale lets only root point Serve at a Unix socket (`ipn/ipnserver/actor.go`: "Only root
//! is a local admin. The operator user is not"), so setup runs exactly one `sudo`:
//!
//! - `tailscale --socket=<abc's> serve --bg unix:<abc's socket>`, which writes Serve's setting
//!   into abc's node's own state, where it stays across restarts with no root again;
//! - on Linux, `loginctl enable-linger <uid>` first, in the same `sudo`, so the user's services
//!   start at boot and keep running after they log out.
//!
//! Nothing else runs as root. Setup prints what the step does before `sudo` asks, and runs it
//! only when one of the two is missing, so running setup again asks nothing.

use std::path::PathBuf;

use crate::process::Invocation;

/// Lingering for one user: their services run without a login session.
#[derive(Debug, Clone)]
pub(crate) struct Linger {
    /// `loginctl`.
    pub(crate) loginctl: PathBuf,
    /// The user.
    pub(crate) uid: u32,
}

impl Linger {
    fn invocation(&self) -> Invocation {
        Invocation::new(&self.loginctl)
            .arg("enable-linger")
            .arg(self.uid.to_string())
    }
}

/// What the root step does: one of the two, or both.
#[derive(Debug, Clone)]
pub(crate) enum RootStep {
    /// Point Serve on abc's node at abc's socket.
    Serve(Invocation),
    /// Enable lingering.
    Linger(Linger),
    /// Both, lingering first, in one `sudo`.
    Both(Linger, Invocation),
}

const SERVE_PURPOSE: &str = "point Tailscale Serve on abc's own node at abc's socket. Tailscale \
                             lets only root set a socket as Serve's target; the setting stays in \
                             abc's node, so this is not asked again.";
const LINGER_PURPOSE: &str = "let your user's services start at boot and keep running after you \
                              log out (systemd's lingering).";

impl RootStep {
    /// The step for what is missing, or `None` when nothing is.
    pub(crate) fn of(linger: Option<Linger>, serve: Option<Invocation>) -> Option<Self> {
        match (linger, serve) {
            (Some(linger), Some(serve)) => Some(Self::Both(linger, serve)),
            (Some(linger), None) => Some(Self::Linger(linger)),
            (None, Some(serve)) => Some(Self::Serve(serve)),
            (None, None) => None,
        }
    }

    /// The exact command under `sudo`: the action itself, or for both, one shell running them
    /// with every word passed as an argument, never spliced into the script.
    pub(crate) fn command(&self, sudo: &std::path::Path) -> Invocation {
        let sudo = Invocation::new(sudo).arg("--");
        match self {
            Self::Serve(serve) => sudo.arg(serve.program()).args(serve.arguments()),
            Self::Linger(linger) => {
                let action = linger.invocation();
                sudo.arg(action.program()).args(action.arguments())
            }
            Self::Both(linger, serve) => sudo
                .args([
                    "/bin/sh",
                    "-c",
                    r#""$1" enable-linger "$2" && shift 2 && exec "$@""#,
                ])
                .arg("abc-setup")
                .arg(&linger.loginctl)
                .arg(linger.uid.to_string())
                .arg(serve.program())
                .args(serve.arguments()),
        }
    }

    /// What the person reads before `sudo` asks for their password.
    pub(crate) fn explanation(&self, sudo: &std::path::Path) -> Vec<String> {
        let purposes: &[&str] = match self {
            Self::Serve(_) => &[SERVE_PURPOSE],
            Self::Linger(_) => &[LINGER_PURPOSE],
            Self::Both(..) => &[LINGER_PURPOSE, SERVE_PURPOSE],
        };
        let mut lines = vec![
            "abc needs your administrator password once, to run one command as root. It will:"
                .to_owned(),
        ];
        lines.extend(purposes.iter().map(|purpose| format!("  - {purpose}")));
        lines.push(format!("The command: {}", self.command(sudo).shown()));
        lines.push("Nothing else runs as root.".to_owned());
        lines
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{Linger, RootStep};
    use crate::process::Invocation;

    const SUDO: &str = "/usr/bin/sudo";

    fn serve() -> Invocation {
        Invocation::new("/opt/abc/libexec/tailscale")
            .arg("--socket=/h/run/tailscaled.sock")
            .args(["serve", "--bg", "unix:/h/run/abc.sock"])
    }

    fn linger() -> Linger {
        Linger {
            loginctl: PathBuf::from("/usr/bin/loginctl"),
            uid: 1000,
        }
    }

    #[test]
    fn nothing_missing_asks_for_nothing() {
        assert!(RootStep::of(None, None).is_none(), "no sudo");
    }

    #[test]
    fn serve_alone_is_one_sudo_of_abcs_own_tailscale() {
        let step = RootStep::of(None, Some(serve())).unwrap();
        assert_eq!(
            step.command(Path::new(SUDO)).shown(),
            "/usr/bin/sudo -- /opt/abc/libexec/tailscale --socket=/h/run/tailscaled.sock serve \
             --bg unix:/h/run/abc.sock",
            "the exact command line"
        );
        let said = step.explanation(Path::new(SUDO));
        assert_eq!(said.len(), 4, "{said:?}");
        assert!(said[1].contains("only root"), "{said:?}");
        assert!(
            said[2].starts_with("The command: /usr/bin/sudo -- "),
            "{said:?}"
        );
        assert_eq!(said[3], "Nothing else runs as root.", "and no more");
    }

    #[test]
    fn linger_and_serve_share_one_sudo_with_every_word_an_argument() {
        let step = RootStep::of(Some(linger()), Some(serve())).unwrap();
        assert_eq!(
            step.command(Path::new(SUDO)).shown(),
            "/usr/bin/sudo -- /bin/sh -c '\"$1\" enable-linger \"$2\" && shift 2 && exec \"$@\"' \
             abc-setup /usr/bin/loginctl 1000 /opt/abc/libexec/tailscale \
             --socket=/h/run/tailscaled.sock serve --bg unix:/h/run/abc.sock",
            "one sudo, lingering first"
        );
        let said = step.explanation(Path::new(SUDO));
        assert_eq!(said.len(), 5, "{said:?}");
        assert!(said[1].contains("lingering"), "{said:?}");
        assert!(said[2].contains("Serve"), "{said:?}");
        let alone = RootStep::of(Some(linger()), None).unwrap();
        assert_eq!(
            alone.command(Path::new(SUDO)).shown(),
            "/usr/bin/sudo -- /usr/bin/loginctl enable-linger 1000",
            "lingering alone"
        );
        assert!(
            alone.explanation(Path::new(SUDO))[1].contains("lingering"),
            "says why"
        );
    }

    #[test]
    fn the_shell_runs_both_actions_with_the_words_it_was_given() {
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("log");
        let recorder = scratch.path().join("record");
        std::fs::write(
            &recorder,
            format!(
                "#!/bin/sh\nprintf '%s|' \"$@\" >> '{log}'\necho >> '{log}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let step = RootStep::of(
            Some(Linger {
                loginctl: recorder.clone(),
                uid: 7,
            }),
            Some(Invocation::new(&recorder).args(["serve", "a b"])),
        )
        .unwrap();
        // env stands in for sudo: it runs what follows `--` as given.
        step.command(Path::new("/usr/bin/env")).run("f").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "enable-linger|7|\nserve|a b|\n",
            "both ran, in order, words intact"
        );
    }
}
