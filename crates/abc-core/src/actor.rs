//! Who acts ([`Actor`], recorded on every event) and who is asking ([`Principal`], the result of
//! authentication). They differ: a `Visitor` asks to claim the server, and the event records the
//! `Person` that the claim creates.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;
use uuid::Uuid;

use crate::id::{FromUuid, PersonId, WorkerId, WorkspaceId};
use crate::identity::TailnetLogin;

/// The actor recorded on an event (I4): a person, a worker or the server. `Agent` arrives with
/// S1. Every person and worker exists as an entity: a row in the `actor` view, created by the
/// event that introduced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Actor {
    /// A person, signed in through the tailnet.
    Person(PersonId),
    /// An enrolled worker: a machine acting under its own credential.
    Worker(WorkerId),
    /// The abc installation itself: what it records on its own (a session it saw end, the
    /// sessions it closes after a restart) and what its host operator asks of it from a shell on
    /// the server (`abc server setup-code`). It has no id: there is one per installation.
    Server,
}

impl Actor {
    /// The stored kind, as in the `event.actor_kind` CHECK constraint.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Person(_) => "person",
            Self::Worker(_) => "worker",
            Self::Server => "server",
        }
    }

    /// The stored id: `None` for the server.
    #[must_use]
    pub const fn id(self) -> Option<Uuid> {
        match self {
            Self::Person(id) => Some(id.as_uuid()),
            Self::Worker(id) => Some(id.as_uuid()),
            Self::Server => None,
        }
    }

    /// Rebuilds an actor from its stored kind and id.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownActor`] for an unknown kind, or an id that does not fit the kind.
    pub fn from_stored(kind: &str, id: Option<Uuid>) -> Result<Self, UnknownActor> {
        match (kind, id) {
            ("person", Some(id)) => Ok(Self::Person(PersonId::from_uuid(id))),
            ("worker", Some(id)) => Ok(Self::Worker(WorkerId::from_uuid(id))),
            ("server", None) => Ok(Self::Server),
            _ => Err(UnknownActor),
        }
    }
}

/// A stored actor this build cannot read: a corrupt or future log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("unknown actor kind, or an actor id that does not fit its kind")]
pub struct UnknownActor;

/// A person's role in a workspace. v1 has one person per server (decision 20), so the only role is
/// `Owner`. Membership is still stored per (workspace, person), so adding a role later is a local
/// change to this enum and the policy matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Full control of the workspace.
    Owner,
}

impl Role {
    /// The stored name, as in the `membership.role` CHECK constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
        }
    }

    /// Reads a stored name.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownRole`] for a name this build does not know.
    pub fn parse(stored: &str) -> Result<Self, UnknownRole> {
        match stored {
            "owner" => Ok(Self::Owner),
            _ => Err(UnknownRole),
        }
    }
}

/// A stored role this build does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("unknown role")]
pub struct UnknownRole;

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
    /// A known person, with every membership they hold.
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
    /// observed, and reading the open sessions it must close at boot. Nothing else.
    Server,
}

impl Principal {
    /// This principal's role in `workspace`, if it is a person who belongs to it.
    #[must_use]
    pub fn role_in(&self, workspace: WorkspaceId) -> Option<Role> {
        match self {
            Self::Person { memberships, .. } => memberships
                .binary_search_by_key(&workspace, |membership| membership.workspace)
                .ok()
                .and_then(|index| memberships.get(index))
                .map(|membership| membership.role),
            Self::Worker { .. } | Self::Visitor { .. } | Self::Anonymous | Self::Server => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(n: u128) -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(n))
    }

    #[test]
    fn role_in_finds_a_persons_membership() {
        let person = Principal::Person {
            id: PersonId::from_uuid(Uuid::from_u128(1)),
            memberships: vec![
                Membership {
                    workspace: workspace(10),
                    role: Role::Owner,
                },
                Membership {
                    workspace: workspace(20),
                    role: Role::Owner,
                },
            ],
        };
        assert_eq!(person.role_in(workspace(10)), Some(Role::Owner), "first");
        assert_eq!(person.role_in(workspace(20)), Some(Role::Owner), "second");
        assert_eq!(person.role_in(workspace(15)), None, "not a member");
        let worker = Principal::Worker {
            id: WorkerId::from_uuid(Uuid::from_u128(2)),
            workspace: workspace(10),
        };
        assert_eq!(worker.role_in(workspace(10)), None, "workers hold no roles");
        assert_eq!(
            Principal::Server.role_in(workspace(10)),
            None,
            "nor does the server"
        );
    }

    #[test]
    fn actors_round_trip_their_stored_form() {
        let id = Uuid::from_u128(7);
        for actor in [
            Actor::Person(PersonId::from_uuid(id)),
            Actor::Worker(WorkerId::from_uuid(id)),
            Actor::Server,
        ] {
            assert_eq!(
                Actor::from_stored(actor.kind(), actor.id()),
                Ok(actor),
                "{actor:?}"
            );
        }
        assert_eq!(
            Actor::from_stored("server", Some(id)),
            Err(UnknownActor),
            "server has no id"
        );
        assert_eq!(
            Actor::from_stored("person", None),
            Err(UnknownActor),
            "a person has an id"
        );
        assert_eq!(
            Actor::from_stored("agent", Some(id)),
            Err(UnknownActor),
            "not yet"
        );
    }

    #[test]
    fn roles_round_trip_their_stored_names() {
        assert_eq!(Role::parse(Role::Owner.as_str()), Ok(Role::Owner), "owner");
        assert_eq!(Role::parse("admin"), Err(UnknownRole), "unknown");
    }
}
