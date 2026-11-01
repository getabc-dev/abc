//! Queries and the read models they return. Read models are the only shapes the web app sees
//! (generated to TypeScript): views, never raw events.
//!
//! A query passes the same [`crate::policy::authorize`] as a command (I4). abc-store implements
//! `Fetchable` for each query, and that trait's `fetch` takes an `&Authorized<Q>`, so an
//! implementation cannot be invoked without the check.

use schemars::JsonSchema;
use serde::Serialize;
use ts_rs::TS;

use crate::actor::Role;
use crate::event::Position;
use crate::id::{EnrolmentTokenId, PersonId, WorkerId, WorkspaceId};
use crate::identity::{Arch, DisplayName, MachineLabel, Os, TailnetLogin, WorkspaceName};
use crate::policy::{DenialReason, NoProof, ReadAction, Request, Scope};
use crate::time::Timestamp;

/// A request that only reads.
pub trait Query: Request<Action = ReadAction, Facts = (), Proof = NoProof> {
    /// What it returns.
    type Answer;
}

macro_rules! query {
    ($name:ty, $action:expr, $answer:ty, |$s:ident| $scope:expr) => {
        impl Request for $name {
            type Action = ReadAction;
            const ACTION: ReadAction = $action;
            type Facts = ();
            type Proof = NoProof;
            fn scope(&self) -> Scope {
                let $s = self;
                $scope
            }
            fn prove(&self, (): &(), _now: Timestamp) -> Result<NoProof, DenialReason> {
                Ok(NoProof)
            }
        }
        impl Query for $name {
            type Answer = $answer;
        }
    };
}

/// Who am I, and what can I do here? The web app's first call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhoAmI;
query!(WhoAmI, ReadAction::ViewSession, SessionView, |_s| {
    Scope::Installation
});

/// The answer to [`WhoAmI`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionView {
    /// The request carried no tailnet identity (a tagged device, or a non-browser client).
    NoIdentity,
    /// The server has no owner yet. The web app shows the setup-code form.
    Unclaimed {
        /// The login that will own the server.
        login: TailnetLogin,
    },
    /// The server is claimed and this login is not a member.
    NotAMember {
        /// The visitor's login.
        login: TailnetLogin,
    },
    /// A member.
    Member {
        /// The person.
        person: PersonId,
        /// Their name.
        display_name: DisplayName,
        /// Every workspace they belong to.
        workspaces: Vec<WorkspaceSummary>,
    },
}

/// One of a person's workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct WorkspaceSummary {
    /// The workspace.
    pub id: WorkspaceId,
    /// Its name.
    pub name: WorkspaceName,
    /// The person's role in it.
    pub role: Role,
}

/// Every machine in a workspace, with the log position the snapshot reflects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListMachines {
    /// The workspace.
    pub workspace: WorkspaceId,
}
query!(
    ListMachines,
    ReadAction::ViewWorkspace,
    MachinesSnapshot,
    |s| Scope::Workspace(s.workspace)
);

/// The machines changed after `after`. The SSE stream's delta read, and its resume point: the SSE
/// `id:` is the head, so a reconnect with `Last-Event-ID: N` asks for exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachinesSince {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The position the subscriber has already seen.
    pub after: Position,
}
query!(
    MachinesSince,
    ReadAction::ViewWorkspace,
    MachinesSnapshot,
    |s| Scope::Workspace(s.workspace)
);

/// Machine cards and the head position they reflect. Both are read in one REPEATABLE READ
/// transaction, so `head` is exact: nothing at or below it is missing from `machines`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct MachinesSnapshot {
    /// The log head the cards reflect.
    pub head: Position,
    /// The cards: all of them for `ListMachines`, only those changed after `after` for `MachinesSince`.
    pub machines: Vec<MachineCard>,
}

/// One worker as the Machines screen shows it. Everything here, online-ness included, is derived
/// from the log through the `worker` view, so one SSE mechanism (a cursor over positions) keeps the
/// screen current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct MachineCard {
    /// The worker.
    pub worker: WorkerId,
    /// Its display name.
    pub name: DisplayName,
    /// Its hostname at enrolment.
    pub hostname: DisplayName,
    /// Its OS.
    pub os: Os,
    /// Its architecture.
    pub arch: Arch,
    /// The release it last connected with.
    pub build: String,
    /// Whether it has an open session: its last `session_started` has no matching `session_ended`.
    /// After a server crash the boot sweep ends stale sessions before any socket is accepted, so
    /// this is never stale for longer than the server is down.
    pub online: bool,
    /// When it enrolled.
    pub enrolled_at: Timestamp,
    /// The later of its last session start and its last session end, if it ever connected.
    pub last_seen_at: Option<Timestamp>,
    /// Whether the owner revoked it. Revoked workers stay listed, greyed out.
    pub revoked: bool,
    /// The position of the last event that changed this card.
    pub changed_at: Position,
}

/// The workspace's enrolment tokens (never their secrets), newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEnrolmentTokens {
    /// The workspace.
    pub workspace: WorkspaceId,
}
query!(
    ListEnrolmentTokens,
    ReadAction::ViewWorkspace,
    Vec<TokenSummary>,
    |s| Scope::Workspace(s.workspace)
);

/// A token as the owner sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS, JsonSchema)]
pub struct TokenSummary {
    /// The token.
    pub id: EnrolmentTokenId,
    /// The owner's note.
    pub label: Option<MachineLabel>,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// When it stops working.
    pub expires_at: Timestamp,
    /// Its status at the store's clock when read.
    pub status: TokenStatus,
}

/// A token's status. Derived when read from `revoked_at`, `redeemed_at` and `expires_at`, never
/// stored, so it never goes stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TokenStatus {
    /// Redeemable.
    Open,
    /// Past `expires_at`, unredeemed.
    Expired,
    /// Revoked by the owner.
    Revoked,
    /// Redeemed by a worker.
    Redeemed {
        /// The worker it created.
        worker: WorkerId,
    },
}

impl TokenStatus {
    /// A token's status at `now`: revoked and redeemed are final; otherwise it is open until
    /// `expires_at`.
    #[must_use]
    pub fn at(
        revoked: bool,
        redeemed_by: Option<WorkerId>,
        expires_at: Timestamp,
        now: Timestamp,
    ) -> Self {
        match (revoked, redeemed_by) {
            (true, _) => Self::Revoked,
            (false, Some(worker)) => Self::Redeemed { worker },
            (false, None) if now >= expires_at => Self::Expired,
            (false, None) => Self::Open,
        }
    }
}

/// A connected worker reading its own identity: at the WebSocket upgrade, and again each time the
/// log head moves, to notice revocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSelf;
query!(
    WorkerSelf,
    ReadAction::ViewSelfAsWorker,
    WorkerIdentity,
    |_s| Scope::OwnWorkspace
);

/// The answer to [`WorkerSelf`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerIdentity {
    /// The worker.
    pub worker: WorkerId,
    /// Its workspace.
    pub workspace: WorkspaceId,
}

/// Every worker with an open session, across workspaces: the boot sweep's input. Server only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenSessions;
query!(
    OpenSessions,
    ReadAction::ViewAsServer,
    Vec<crate::command::OpenSession>,
    |_s| Scope::Installation
);

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;
    use crate::id::FromUuid;

    #[test]
    fn a_tokens_status_is_derived_at_read_time() {
        let expires = Timestamp::from_utc(datetime!(2026-01-01 01:00:00 UTC));
        let before = Timestamp::from_utc(datetime!(2026-01-01 00:59:59 UTC));
        let worker = WorkerId::from_uuid(Uuid::from_u128(1));
        assert_eq!(
            TokenStatus::at(false, None, expires, before),
            TokenStatus::Open,
            "open"
        );
        assert_eq!(
            TokenStatus::at(false, None, expires, expires),
            TokenStatus::Expired,
            "expired at expires_at"
        );
        assert_eq!(
            TokenStatus::at(true, None, expires, before),
            TokenStatus::Revoked,
            "revoked"
        );
        assert_eq!(
            TokenStatus::at(false, Some(worker), expires, expires),
            TokenStatus::Redeemed { worker },
            "redeemed stays redeemed after expiry"
        );
        assert_eq!(
            serde_json::to_string(&TokenStatus::Redeemed { worker }).unwrap(),
            format!(r#"{{"status":"redeemed","worker":"{}"}}"#, worker.as_uuid()),
            "tagged by status on the wire"
        );
    }
}
