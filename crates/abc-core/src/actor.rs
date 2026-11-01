//! Who acts ([`Actor`], recorded on every event) and who is asking ([`Principal`], the result of
//! authentication). They differ: a `Visitor` is asking to claim the server, and the event records
//! the `Person` that the claim creates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::id::{PersonId, WorkerId, WorkspaceId};
use crate::identity::TailnetLogin;

/// The actor recorded on an event (I4). Every person and worker exists as an entity: a row in the
/// `actor` view, created by the event that introduced it.
///
/// `Agent` arrives with S1. `Worker` is a machine acting under its own credential, and `Server` is
/// the installation itself (see the rationale's open question on I4's wording).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[ts(export)]
pub enum Actor {
    /// A person, signed in through the tailnet.
    Person(PersonId),
    /// An enrolled worker.
    Worker(WorkerId),
    /// The abc installation itself: what it records on its own (a session it saw end, the
    /// sessions it closes after a restart) and what its host operator asks of it from a shell on
    /// the server (`abc server setup-code`). It has no id: there is one per installation.
    Server,
}

/// A person's role in a workspace. v1 has one person per server (decision 20), so the only role is
/// `Owner`. Membership is still stored per (workspace, person), so adding a role later is a local
/// change to this enum and the policy matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Role {
    /// Full control of the workspace.
    Owner,
}

/// A person's membership in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Membership {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The person's role in it.
    pub role: Role,
}

/// The authenticated party behind a request, resolved by abc-store from a `Caller` **inside the
/// request's transaction**, so it is exactly as fresh as the write it authorizes.
///
/// It is plain data. Building one outside abc-store is harmless: the only consumer that can act on
/// it is `abc_core::policy::authorize`, and the store's public API never accepts a `Principal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A known person, with every membership they hold, loaded together.
    Person {
        /// The person.
        id: PersonId,
        /// Their memberships, ordered by workspace id.
        memberships: Vec<Membership>,
    },
    /// An enrolled, unrevoked worker.
    Worker {
        /// The worker.
        id: WorkerId,
        /// The one workspace it belongs to.
        workspace: WorkspaceId,
    },
    /// A tailnet identity with no person behind it yet. It may only claim an unclaimed server.
    Visitor {
        /// The login Tailscale asserted.
        login: TailnetLogin,
    },
    /// No identity: a tagged device, or a request whose authority is a capability in its body
    /// (an enrolment token).
    Anonymous,
    /// The installation itself (see [`Actor::Server`]). The policy matrix grants it exactly the
    /// actions only the server can take: issuing a setup code while unclaimed, ending sessions it
    /// observed, closing sessions left open by a restart. Nothing else.
    Server,
}

impl Principal {
    /// This principal's role in `workspace`, if it is a person who belongs to it.
    #[must_use]
    pub fn role_in(&self, workspace: WorkspaceId) -> Option<Role> {
        let _ = workspace;
        unimplemented!("binary search memberships for Person; None for other variants")
    }
}
