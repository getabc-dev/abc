//! Typed identifiers. Every id is a `UUIDv7` minted through [`crate::decision::Mint`]. The type
//! names what the id refers to, so a `WorkerId` cannot be passed where a `WorkspaceId` is expected.
//! Ids are not secrets or capabilities: knowing one grants nothing, which is why their
//! constructors are public.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// Builds an id from a UUID. Implemented by every id type.
pub trait FromUuid {
    /// Wraps `uuid`. The caller guarantees it names this kind of entity: a freshly minted UUID, or
    /// one read back from storage where it was recorded as this kind.
    fn from_uuid(uuid: Uuid) -> Self;
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize, TS, JsonSchema,
        )]
        #[serde(transparent)]
        #[ts(type = "string")]
        pub struct $name(Uuid);

        impl $name {
            /// The UUID behind this id, for storage.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl FromUuid for $name {
            fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// A workspace: the unit everything in abc belongs to (I4).
    WorkspaceId
);
id_type!(
    /// A person. Persons are actors and can belong to several workspaces.
    PersonId
);
id_type!(
    /// An enrolled machine running `abc worker`. Workers are actors and belong to one workspace.
    WorkerId
);
id_type!(
    /// An enrolment token, and the stream of its lifecycle events.
    EnrolmentTokenId
);
id_type!(
    /// One WebSocket session of a worker, from handshake to close.
    SessionId
);
id_type!(
    /// One run of a graph, and the stream of its events.
    RunId
);
id_type!(
    /// One lease: a run's step and phase given to one worker.
    LeaseId
);
id_type!(
    /// One event in the log.
    EventId
);
id_type!(
    /// Ties together every event one request (or, from S1, one graph run) caused. For HTTP it is
    /// the request id, so a log line and the events it produced share it.
    CorrelationId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_its_uuid_on_the_wire_and_in_text() {
        let uuid = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001);
        let id = WorkerId::from_uuid(uuid);
        assert_eq!(id.as_uuid(), uuid, "the id keeps its UUID");
        assert_eq!(id.to_string(), uuid.to_string(), "Display is the UUID's");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(
            json,
            format!("\"{uuid}\""),
            "an id is a bare string on the wire"
        );
        assert_eq!(
            serde_json::from_str::<WorkerId>(&json).unwrap(),
            id,
            "and reads back"
        );
    }
}
