//! Enrolment: make a key, save it, send its digest, record who the server says this machine is.
//! Always the same two steps, "make sure the record holds a key for this token" and then "send",
//! because the server's replay rule (the same token with the same key digest gets the same answer
//! and records nothing, even after the token expired) makes every crash point converge:
//!
//! | Crash or loss                          | A re-run finds              | and does                         |
//! | -------------------------------------- | --------------------------- | -------------------------------- |
//! | before the pending record is saved     | nothing                     | starts clean                     |
//! | after it is saved, before sending      | pending, same token         | sends the same key               |
//! | the server enrolled, the answer is lost | pending, same token        | the same key: the same answer    |
//! | after the answer, before the save      | pending, same token         | as above                         |
//! | after the save                         | enrolled, same token        | as above, and says so            |
//! | enrolled, another token or server      | enrolled, other digest      | refuses unless `--replace`       |
//! | pending for another token or server    | pending, other digest       | a new key replaces the old one   |

use abc_core::credential::{EnrolmentToken, WorkerKey};
use abc_core::identity::{DisplayName, MachineInfo};
use abc_core::secret::Secret;
use abc_platform::entropy::{ChaChaMint, Entropy as _, EntropyUnavailable, SystemEntropy};
use abc_protocol::api::ApiCode;
use abc_protocol::enrol::EnrolRequest;
use abc_protocol::url::PublicUrl;
use thiserror::Error;

use crate::dial::{self, DialError};
use crate::home::{HomeError, Record, WorkerHome};
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
    let mut record = match held.load()? {
        Some(record) if record.server == *server && record.token == digest => record,
        Some(Record {
            enrolled: Some(enrolled),
            ..
        }) if if_enrolled == IfEnrolled::Refuse => {
            return Err(EnrolError::AlreadyEnrolled(enrolled.name));
        }
        Some(_) | None => {
            let mint = ChaChaMint::from_seed(SystemEntropy.seed()?);
            let record = Record::pending(server.clone(), digest, WorkerKey::generate(&mint));
            held.save(&record)?;
            record
        }
    };
    let again = record.enrolled.is_some();
    let request = EnrolRequest {
        token,
        key: record.key.expose_secret().digest(),
        machine,
        build: env.build.clone(),
    };
    let response = dial::post_enrol(env, server, &request).await?;
    let enrolled = Enrolled {
        worker: response.worker,
        workspace: response.workspace,
        name: response.name,
    };
    record.enrolled = Some(enrolled.clone());
    held.save(&record)?;
    Ok(Enrolment { enrolled, again })
}

/// This machine as it introduces itself at enrolment: its hostname, or `hostname` when given, and
/// the operating system and architecture this build is for.
///
/// # Errors
///
/// [`EnrolError::Hostname`] when the name is not a valid display name (1 to 64 characters, no
/// control characters).
pub fn machine(hostname: Option<&str>) -> Result<MachineInfo, EnrolError> {
    let system = rustix::system::uname();
    let name = match hostname {
        Some(name) => name.to_owned(),
        None => system.nodename().to_string_lossy().into_owned(),
    };
    Ok(MachineInfo {
        hostname: DisplayName::parse(&name).map_err(|_invalid| EnrolError::Hostname)?,
        os: crate::OS,
        arch: crate::ARCH,
    })
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
            rustix::system::uname().nodename().to_str().unwrap().trim(),
            "else the hostname"
        );
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
}
