//! Enrolment: make a key, save it, send its digest, record who the server says this machine is.
//! Always the same two steps, "make sure the record holds a key for this token" and then "send",
//! because the server's replay rule (the same token with the same key digest gets the same answer
//! and records nothing, even after the token expired) makes every crash point converge. A new key
//! waits in the record's pending slot and becomes the enrolment in use only once the server has
//! answered, so a failed `--replace` leaves the old enrolment working.
//!
//! | Crash or loss                           | A re-run finds                | and does                       |
//! | --------------------------------------- | ----------------------------- | ------------------------------ |
//! | before the pending key is saved         | no key for this token         | makes one                      |
//! | after it is saved, before sending       | pending, same token           | sends the same key             |
//! | the server enrolled, the answer is lost | pending, same token           | the same key: the same answer  |
//! | after the answer, before the save       | pending, same token           | as above                       |
//! | after the save                          | in use, same token            | as above, and says so          |
//! | in use, another token or server         | in use, other digest          | refuses unless `--replace`     |
//! | pending for another token or server     | pending, other digest         | a new key replaces the pending |

use abc_core::credential::{EnrolmentToken, WorkerKey};
use abc_core::identity::{DisplayName, MachineInfo};
use abc_core::secret::Secret;
use abc_platform::entropy::{ChaChaMint, Entropy as _, EntropyUnavailable, SystemEntropy};
use abc_protocol::api::ApiCode;
use abc_protocol::enrol::EnrolRequest;
use abc_protocol::url::PublicUrl;
use std::path::Path;
use thiserror::Error;

use crate::dial::{self, DialError};
use crate::home::{Current, HomeError, Slot, WorkerHome};
use crate::{Enrolled, Env, IfEnrolled};

/// What an enrolment did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolment {
    /// Who this machine is now.
    pub enrolled: Enrolled,
    /// Whether this run found the enrolment already complete and only confirmed it.
    pub again: bool,
}

/// Enrols this machine with `server`, using a one-time `token`, as `machine`.
///
/// Idempotent: a re-run with the same token after a crash, a lost answer or a success sends the
/// same key's digest and gets the same answer, and the server records nothing new. Holds the
/// home's lock throughout, so it cannot race a running worker or another enrolment.
///
/// # Errors
///
/// [`EnrolError`]; its `Display` is the sentence the user reads.
pub async fn enrol(
    home: &WorkerHome,
    env: &Env,
    server: &PublicUrl,
    token: Secret<EnrolmentToken>,
    machine: MachineInfo,
    if_enrolled: IfEnrolled,
) -> Result<Enrolment, EnrolError> {
    let held = home.lock()?;
    let digest = token.expose_secret().digest();
    let mut record = held.load()?.unwrap_or_default();
    if let Some(current) = record
        .current
        .as_mut()
        .filter(|current| current.key.is_for(server, &digest))
    {
        current.worker = send(env, server, token, &current.key, machine).await?;
        let enrolled = current.worker.clone();
        held.save(record.current.as_ref(), record.pending.as_ref())?;
        return Ok(Enrolment {
            enrolled,
            again: true,
        });
    }
    if let Some(current) = &record.current
        && if_enrolled == IfEnrolled::Refuse
    {
        return Err(EnrolError::AlreadyEnrolled(current.worker.name.clone()));
    }
    let pending = match record.pending {
        Some(pending) if pending.is_for(server, &digest) => pending,
        Some(_) | None => {
            let mint = ChaChaMint::from_seed(SystemEntropy.seed()?);
            let pending = Slot {
                server: server.clone(),
                token: digest,
                key: WorkerKey::generate(&mint),
            };
            held.save(record.current.as_ref(), Some(&pending))?;
            pending
        }
    };
    let worker = send(env, server, token, &pending, machine).await?;
    let current = Current {
        key: pending,
        worker,
    };
    held.save(Some(&current), None)?;
    Ok(Enrolment {
        enrolled: current.worker,
        again: false,
    })
}

/// Sends `key`'s digest with `token` and returns who the server says this machine is.
async fn send(
    env: &Env,
    server: &PublicUrl,
    token: Secret<EnrolmentToken>,
    key: &Slot,
    machine: MachineInfo,
) -> Result<Enrolled, EnrolError> {
    let request = EnrolRequest {
        token,
        key: key.key.expose_secret().digest(),
        machine,
        build: env.build.clone(),
    };
    let response = dial::post_enrol(env, server, &request).await?;
    Ok(Enrolled {
        worker: response.worker,
        workspace: response.workspace,
        name: response.name,
    })
}

/// This machine as it introduces itself at enrolment: its own name (on macOS the computer name the
/// owner gave it, else its local network name; elsewhere, or when neither is set, the kernel's
/// hostname), or `hostname` when given, and the operating system and architecture this build is
/// for.
///
/// # Errors
///
/// [`EnrolError::Hostname`] when the name is not a valid display name (1 to 64 characters, no
/// control characters).
pub fn machine(hostname: Option<&str>) -> Result<MachineInfo, EnrolError> {
    let name = match hostname {
        Some(name) => name.to_owned(),
        None => own_name(),
    };
    Ok(MachineInfo {
        hostname: DisplayName::parse(&name).map_err(|_invalid| EnrolError::Hostname)?,
        os: crate::OS,
        arch: crate::ARCH,
    })
}

/// Where macOS keeps its names for the machine.
const SCUTIL: &str = "/usr/sbin/scutil";

/// This machine's own name. On macOS, the name its owner gave it in System Settings
/// (`scutil --get ComputerName`), else its local network name (`LocalHostName`): there the kernel's
/// hostname often names the network instead, such as the IP address a DHCP server handed out, which
/// a machine card showed. Elsewhere, and when neither is set, the kernel's hostname.
fn own_name() -> String {
    let nodename = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let asked = if cfg!(target_os = "macos") {
        ["ComputerName", "LocalHostName"]
            .into_iter()
            .map(|key| ask(Path::new(SCUTIL), key))
            .collect()
    } else {
        Vec::new()
    };
    first_name(asked, nodename)
}

/// The first of `names` that can name a worker, else `fallback`.
fn first_name(names: Vec<Option<String>>, fallback: String) -> String {
    names
        .into_iter()
        .flatten()
        .find(|name| DisplayName::parse(name).is_ok())
        .unwrap_or(fallback)
}

/// What `program --get key` prints, as `scutil` answers for a name it holds; none when it fails or
/// prints nothing.
fn ask(program: &Path, key: &str) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(["--get", key])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8(output.stdout).ok()?;
    let name = name.trim();
    (output.status.success() && !name.is_empty()).then(|| name.to_owned())
}

/// Why enrolment failed. `Display` is the sentence the user reads; the server's own refusals are
/// mapped from its `ApiCode` in one exhaustive match.
#[derive(Debug, Error)]
pub enum EnrolError {
    /// `enrolment_token_unknown`.
    #[error("the server does not know this enrolment token; check that you copied all of it")]
    TokenUnknown,
    /// `enrolment_token_expired`.
    #[error("the enrolment token expired; ask the server's owner for a new one")]
    TokenExpired,
    /// `enrolment_token_revoked`.
    #[error("the enrolment token was revoked; ask the server's owner for a new one")]
    TokenRevoked,
    /// `enrolment_token_used`: another key redeemed it.
    #[error(
        "another machine already used this enrolment token; ask the server's owner for a new one"
    )]
    TokenUsed,
    /// `worker_revoked` on a retry: the worker this key enrolled as was revoked since.
    #[error(
        "the worker this machine enrolled as was revoked; enrol again with a new token and --replace"
    )]
    Revoked,
    /// `worker_key_in_use`: another worker holds this key.
    #[error(
        "another worker already holds this machine's key (was the worker home copied?); enrol again with --replace"
    )]
    KeyInUse,
    /// The home holds an enrolment for another token or server.
    #[error("this machine is already enrolled as {}; pass --replace to enrol it as a new worker", .0.as_str())]
    AlreadyEnrolled(DisplayName),
    /// The hostname is not a valid display name.
    #[error(
        "this machine's hostname cannot name a worker; pass --hostname with 1 to 64 characters"
    )]
    Hostname,
    /// `rate_limited`.
    #[error(
        "too many enrolment attempts from this machine; wait a minute and run the same command again"
    )]
    RateLimited,
    /// `unavailable`, or a gateway error from Serve.
    #[error(
        "the server is starting, stopping or cannot reach its database; run the same command again"
    )]
    Unavailable,
    /// The server could not be reached, or did not answer in time.
    #[error("could not reach the server; run the same command again")]
    Unreachable(#[source] DialError),
    /// The server refused for another reason.
    #[error("the server refused the enrolment ({code:?}): {message}")]
    Refused {
        /// Its code.
        code: ApiCode,
        /// Its message.
        message: String,
    },
    /// The answer was not abc's.
    #[error("that URL does not answer like an abc server (HTTP {status})")]
    NotAbc {
        /// The HTTP status.
        status: u16,
    },
    /// The request could not be encoded; a bug.
    #[error("could not encode the enrolment request")]
    Encode(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The home.
    #[error(transparent)]
    Home(#[from] HomeError),
    /// The operating system could not seed the key's generator.
    #[error(transparent)]
    Entropy(#[from] EntropyUnavailable),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_introduces_itself() {
        let named = machine(Some("  build box ")).unwrap();
        assert_eq!(
            named.hostname.as_str(),
            "build box",
            "a given name, trimmed"
        );
        assert_eq!(
            (named.os, named.arch),
            (crate::OS, crate::ARCH),
            "this build's platform"
        );
        let own = machine(None).unwrap();
        assert_eq!(
            own.hostname.as_str(),
            own_name().trim(),
            "else its own name"
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(
                own_name(),
                rustix::system::uname().nodename().to_str().unwrap(),
                "the kernel's hostname"
            );
        }
        assert!(
            matches!(machine(Some(&"x".repeat(65))), Err(EnrolError::Hostname)),
            "a name too long to show"
        );
        assert_eq!(
            EnrolError::AlreadyEnrolled(DisplayName::parse("mbp").unwrap()).to_string(),
            "this machine is already enrolled as mbp; pass --replace to enrol it as a new worker",
            "the name it is enrolled as"
        );
    }

    /// On macOS the machine card showed `192.168.1.3`: the kernel's hostname named the network.
    /// The name the owner gave the Mac comes first, then its local network name, then the
    /// hostname.
    #[test]
    fn the_first_name_that_can_name_a_worker_wins() {
        let some = |name: &str| Some(name.to_owned());
        let ip = "192.168.1.3".to_owned();
        assert_eq!(
            first_name(
                vec![some("Ada's MacBook Pro"), some("Adas-MacBook-Pro")],
                ip.clone()
            ),
            "Ada's MacBook Pro"
        );
        assert_eq!(
            first_name(vec![None, some("Adas-MacBook-Pro")], ip.clone()),
            "Adas-MacBook-Pro"
        );
        assert_eq!(
            first_name(vec![some(&"x".repeat(65)), None], ip.clone()),
            "192.168.1.3",
            "a name too long to show is passed over"
        );
        assert_eq!(first_name(Vec::new(), ip.clone()), ip, "elsewhere");
    }

    /// `scutil --get` prints the name, or fails when the name is not set.
    #[test]
    fn asks_scutil_for_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("scutil");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncase \"$1 $2\" in\n\
             '--get ComputerName') echo \"Ada's Mac \" ;;\n\
             '--get LocalHostName') echo '' ;;\n\
             *) echo 'ComputerName: not set' >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        assert_eq!(ask(&fake, "ComputerName").as_deref(), Some("Ada's Mac"));
        assert_eq!(ask(&fake, "LocalHostName"), None, "printed nothing");
        assert_eq!(ask(&fake, "HostName"), None, "not set");
        assert_eq!(ask(&dir.path().join("missing"), "ComputerName"), None);
    }
}
