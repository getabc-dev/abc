//! Event payloads and the stream model of the log.
//!
//! An event belongs to exactly one **stream** (`stream_kind`, `stream_id`) and carries a per-stream
//! sequence number, a global [`Position`], an actor, a workspace, a correlation id and an optional
//! causation id. The [`Stream`] trait ties each stream's id type to its event enum, so a worker
//! event cannot be emitted onto a token's stream.
//!
//! Payloads are internal. They are stored as JSON tagged by `type` (for example
//! `"worker.enrolled"`), and the store reads that tag into the generated `event_type` column. They
//! never reach the browser: the web app receives read models (`crate::query`). That keeps this
//! schema free to change without touching any client.
//!
//! Versioning: [`Event::schema_version`] is 1 for every payload today. While abc has no data to
//! keep (S3) payloads change in place. After the first release, a changed payload keeps its old
//! struct and gains an upcast, and decoding dispatches on (type, version).

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::actor::{Actor, Role};
use crate::credential::{EnrolmentTokenDigest, SetupCodeDigest, WorkerKeyDigest};
use crate::id::{
    CorrelationId, EnrolmentTokenId, EventId, PersonId, SessionId, WorkerId, WorkspaceId,
};
use crate::identity::{DisplayName, MachineInfo, MachineLabel, TailnetLogin, WorkspaceName};
use crate::time::Timestamp;

/// A place in the event log: the global sequence number. Positions are gapless and assigned in
/// commit order (the store serializes writers on the head row), so a consumer that has read every
/// event up to `N` has seen everything at or below `N`, forever. `Position::ORIGIN` (0) comes
/// before the first event.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    ts_rs::TS,
    schemars::JsonSchema,
)]
#[serde(transparent)]
#[ts(type = "number")]
pub struct Position(i64);

impl Position {
    /// Before the first event.
    pub const ORIGIN: Self = Self(0);

    /// Rebuilds a position read from storage.
    ///
    /// # Errors
    ///
    /// Returns [`NegativePosition`] for a negative value.
    pub const fn from_stored(raw: i64) -> Result<Self, NegativePosition> {
        if raw < 0 {
            Err(NegativePosition(raw))
        } else {
            Ok(Self(raw))
        }
    }

    /// The stored value.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The next position, or `None` at `i64::MAX`.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}

/// A stored position below zero: a corrupt row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("event position {0} is negative")]
pub struct NegativePosition(i64);

/// The kinds of stream in the log. The `stream_kind` CHECK constraint in
/// `0001_event_log.sql` lists the same names. A test reads the constraint from `pg_constraint` and
/// compares it with `StreamKind::ALL`, so the two cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StreamKind {
    /// The single installation stream: whether the server has been claimed.
    Installation,
    /// A person's identity.
    Person,
    /// A workspace: creation and membership.
    Workspace,
    /// One enrolment token's life.
    EnrolmentToken,
    /// One worker's life and sessions.
    Worker,
}

impl StreamKind {
    /// Every kind, for the constraint drift test.
    pub const ALL: [Self; 5] = [
        Self::Installation,
        Self::Person,
        Self::Workspace,
        Self::EnrolmentToken,
        Self::Worker,
    ];

    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installation => "installation",
            Self::Person => "person",
            Self::Workspace => "workspace",
            Self::EnrolmentToken => "enrolment_token",
            Self::Worker => "worker",
        }
    }

    /// Reads a stored name.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == stored)
    }
}

/// One stream: its kind and id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamRef {
    /// What kind of stream.
    pub kind: StreamKind,
    /// Which one.
    pub id: Uuid,
}

/// Links a stream's id type to the events it may hold.
pub trait Stream: Copy {
    /// The kind of stream this id names.
    const KIND: StreamKind;
    /// The events that may be appended to it.
    type Event: Into<Event>;
    /// The stream's id.
    fn stream_id(self) -> Uuid;
}

/// The one installation stream (singleton; its id is the nil UUID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installation;

/// Every event payload, grouped by stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// See [`InstallationEvent`].
    Installation(InstallationEvent),
    /// See [`PersonEvent`].
    Person(PersonEvent),
    /// See [`WorkspaceEvent`].
    Workspace(WorkspaceEvent),
    /// See [`EnrolmentEvent`].
    Enrolment(EnrolmentEvent),
    /// See [`WorkerEvent`].
    Worker(WorkerEvent),
}

/// Events on the installation stream. Their `workspace_id` is the workspace the claim creates (or
/// created): the setup code reserves it, so even these events belong to a workspace (I4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InstallationEvent {
    /// The host operator asked for a setup code (`abc server setup-code`); actor `Server`. Only its
    /// digest is recorded: the plaintext went to the operator's terminal and nowhere else. A newer
    /// code supersedes every earlier one.
    #[serde(rename = "installation.setup_code_issued")]
    SetupCodeIssued {
        /// The code's digest, compared in constant time at claim.
        digest: SetupCodeDigest,
        /// The person the claim will create, reserved so the claim's events can be attributed to
        /// them (the actor exists before `decide` runs).
        owner: PersonId,
        /// The first workspace the claim will create.
        workspace: WorkspaceId,
        /// When the code stops working (one hour after issue).
        expires_at: Timestamp,
    },
    /// The server was claimed. Recorded once, ever.
    #[serde(rename = "installation.claimed")]
    Claimed {
        /// The person who claimed it, now the owner of `workspace`.
        owner: PersonId,
        /// The first workspace.
        workspace: WorkspaceId,
    },
}

/// Events on a person's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PersonEvent {
    /// A person was registered, linked to the tailnet login they arrived with.
    #[serde(rename = "person.registered")]
    Registered {
        /// How they are shown.
        display_name: DisplayName,
        /// The login that identifies them from now on.
        tailnet_login: TailnetLogin,
    },
}

/// Events on a workspace's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkspaceEvent {
    /// The workspace was created.
    #[serde(rename = "workspace.created")]
    Created {
        /// Its name.
        name: WorkspaceName,
    },
    /// A person joined the workspace.
    #[serde(rename = "workspace.member_added")]
    MemberAdded {
        /// Who joined.
        person: PersonId,
        /// With what role.
        role: Role,
    },
}

/// Events on an enrolment token's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EnrolmentEvent {
    /// A token was issued. Only its digest is recorded.
    #[serde(rename = "enrolment_token.issued")]
    Issued {
        /// The token's digest, used to look it up when it is presented.
        digest: EnrolmentTokenDigest,
        /// The id the enrolled worker will have. Reserving it here makes redemption retries
        /// return the same worker and lets the enrolment be attributed to that worker.
        worker: WorkerId,
        /// The owner's note.
        label: Option<MachineLabel>,
        /// When the token stops working.
        expires_at: Timestamp,
    },
    /// The owner revoked an unredeemed token.
    #[serde(rename = "enrolment_token.revoked")]
    Revoked,
    /// A machine redeemed the token with this key.
    #[serde(rename = "enrolment_token.redeemed")]
    Redeemed {
        /// The digest of the key the new worker generated.
        key: WorkerKeyDigest,
    },
}

/// Events on a worker's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkerEvent {
    /// The worker enrolled. Its actor is the worker itself, and its causation is the token's
    /// `issued` event, which links the worker to the person who issued the token.
    #[serde(rename = "worker.enrolled")]
    Enrolled {
        /// The token it redeemed.
        token: EnrolmentTokenId,
        /// The digest of its key.
        key: WorkerKeyDigest,
        /// Its display name: the token label if there was one, else the hostname.
        name: DisplayName,
        /// What it reported about its machine.
        machine: MachineInfo,
        /// The abc release it ran.
        build: Version,
    },
    /// A WebSocket session completed the handshake; actor: the worker. The `worker` view's
    /// `open_session` becomes this session, which is what "online" means (one source of truth: the
    /// log, not process memory).
    #[serde(rename = "worker.session_started")]
    SessionStarted {
        /// The session.
        session: SessionId,
        /// The abc release the worker runs now.
        build: Version,
    },
    /// A session ended; actor: `Server`, which observed it. Recording it as the server means a
    /// revoked worker's last session still ends in the log. The projection clears `open_session`
    /// only when it names this session, so a superseded session's late end changes nothing.
    #[serde(rename = "worker.session_ended")]
    SessionEnded {
        /// The session.
        session: SessionId,
        /// Why it ended.
        reason: SessionEndReason,
    },
    /// The owner revoked the worker. Its key stops authenticating within the same transaction.
    #[serde(rename = "worker.revoked")]
    Revoked,
}

/// Why a worker session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    /// The worker closed the socket.
    Closed,
    /// No pong for the heartbeat window: the laptop slept or the network dropped.
    TimedOut,
    /// A newer session for the same worker replaced this one.
    Superseded,
    /// The worker broke the protocol.
    ProtocolError,
    /// The server is shutting down.
    ServerShutdown,
    /// The server found the session still open when it started: the previous process died
    /// without recording the end. Recorded by the boot sweep in `abc_server::run`.
    ServerRestart,
    /// The owner revoked the worker while it was connected.
    Revoked,
}

impl Event {
    /// The stream kind this payload belongs to.
    #[must_use]
    pub const fn stream_kind(&self) -> StreamKind {
        match self {
            Self::Installation(_) => StreamKind::Installation,
            Self::Person(_) => StreamKind::Person,
            Self::Workspace(_) => StreamKind::Workspace,
            Self::Enrolment(_) => StreamKind::EnrolmentToken,
            Self::Worker(_) => StreamKind::Worker,
        }
    }

    /// The payload's schema version. It has an exhaustive match per variant, so a new event has to
    /// state its version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        match self {
            Self::Installation(
                InstallationEvent::SetupCodeIssued { .. } | InstallationEvent::Claimed { .. },
            )
            | Self::Person(PersonEvent::Registered { .. })
            | Self::Workspace(
                WorkspaceEvent::Created { .. } | WorkspaceEvent::MemberAdded { .. },
            )
            | Self::Enrolment(
                EnrolmentEvent::Issued { .. }
                | EnrolmentEvent::Revoked
                | EnrolmentEvent::Redeemed { .. },
            )
            | Self::Worker(
                WorkerEvent::Enrolled { .. }
                | WorkerEvent::SessionStarted { .. }
                | WorkerEvent::SessionEnded { .. }
                | WorkerEvent::Revoked,
            ) => SchemaVersion::V1,
        }
    }

    /// The JSON stored in `event.payload`. It always includes `type`.
    ///
    /// # Errors
    ///
    /// Returns the serde error, which these types never produce but which is not hidden.
    pub fn encode(&self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::Installation(event) => serde_json::to_value(event),
            Self::Person(event) => serde_json::to_value(event),
            Self::Workspace(event) => serde_json::to_value(event),
            Self::Enrolment(event) => serde_json::to_value(event),
            Self::Worker(event) => serde_json::to_value(event),
        }
    }

    /// Decodes a stored payload, choosing the enum from the row's `stream_kind`.
    ///
    /// # Errors
    ///
    /// Returns [`UndecodableEvent`] for an unknown type or version: a corrupt or future log.
    pub fn decode(
        kind: StreamKind,
        version: i16,
        payload: serde_json::Value,
    ) -> Result<Self, UndecodableEvent> {
        let undecodable = |source| UndecodableEvent {
            kind: kind.as_str(),
            version,
            source,
        };
        if version != SchemaVersion::V1.0 {
            return Err(undecodable(None));
        }
        let decoded = match kind {
            StreamKind::Installation => serde_json::from_value(payload).map(Self::Installation),
            StreamKind::Person => serde_json::from_value(payload).map(Self::Person),
            StreamKind::Workspace => serde_json::from_value(payload).map(Self::Workspace),
            StreamKind::EnrolmentToken => serde_json::from_value(payload).map(Self::Enrolment),
            StreamKind::Worker => serde_json::from_value(payload).map(Self::Worker),
        };
        decoded.map_err(|error| undecodable(Some(error)))
    }
}

/// The version of an event payload's schema, stored in `event.schema_version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaVersion(pub i16);

impl SchemaVersion {
    /// The first version, which every payload has today.
    pub const V1: Self = Self(1);
}

/// A stored payload this build cannot read.
#[derive(Debug, Error)]
#[error("cannot decode a {kind} event at schema version {version}")]
pub struct UndecodableEvent {
    /// The stream kind's name.
    pub kind: &'static str,
    /// The stored schema version.
    pub version: i16,
    /// The serde error.
    #[source]
    pub source: Option<serde_json::Error>,
}

/// An event as recorded: payload plus envelope. Read by projections and, from S1, by log consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// Global position.
    pub position: Position,
    /// The event's id.
    pub id: EventId,
    /// When the store recorded it (from its clock, not the database's `now()`).
    pub recorded_at: Timestamp,
    /// The workspace it belongs to.
    pub workspace: WorkspaceId,
    /// Its stream.
    pub stream: StreamRef,
    /// Its place in the stream, starting at 1.
    pub stream_seq: i32,
    /// Who acted.
    pub actor: Actor,
    /// The request or run it belongs to.
    pub correlation: CorrelationId,
    /// The event that caused the command that produced it, if any.
    pub causation: Option<EventId>,
    /// The payload.
    pub event: Event,
}

macro_rules! stream {
    ($id:ty, $kind:ident, $event:ident, $variant:ident, |$s:ident| $uuid:expr) => {
        impl Stream for $id {
            const KIND: StreamKind = StreamKind::$kind;
            type Event = $event;
            fn stream_id(self) -> Uuid {
                let $s = self;
                $uuid
            }
        }

        impl From<$event> for Event {
            fn from(event: $event) -> Self {
                Self::$variant(event)
            }
        }
    };
}

stream!(
    Installation,
    Installation,
    InstallationEvent,
    Installation,
    |_s| Uuid::nil()
);
stream!(PersonId, Person, PersonEvent, Person, |s| s.as_uuid());
stream!(WorkspaceId, Workspace, WorkspaceEvent, Workspace, |s| s
    .as_uuid());
stream!(
    EnrolmentTokenId,
    EnrolmentToken,
    EnrolmentEvent,
    Enrolment,
    |s| s.as_uuid()
);
stream!(WorkerId, Worker, WorkerEvent, Worker, |s| s.as_uuid());

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::credential::Digest;
    use crate::id::FromUuid;
    use crate::identity::{Arch, Os};

    fn id<T: FromUuid>(n: u128) -> T {
        T::from_uuid(Uuid::from_u128(n))
    }

    fn at() -> Timestamp {
        Timestamp::from_utc(datetime!(2026-01-01 00:00:00 UTC))
    }

    /// One payload of every variant.
    pub(crate) fn every_event() -> Vec<Event> {
        let name = DisplayName::parse("mbp").unwrap();
        vec![
            InstallationEvent::SetupCodeIssued {
                digest: Digest::from_bytes([1; 32]),
                owner: id(1),
                workspace: id(2),
                expires_at: at(),
            }
            .into(),
            InstallationEvent::Claimed {
                owner: id(1),
                workspace: id(2),
            }
            .into(),
            PersonEvent::Registered {
                display_name: name.clone(),
                tailnet_login: TailnetLogin::parse("alice@example.com").unwrap(),
            }
            .into(),
            WorkspaceEvent::Created {
                name: WorkspaceName::parse("home").unwrap(),
            }
            .into(),
            WorkspaceEvent::MemberAdded {
                person: id(1),
                role: Role::Owner,
            }
            .into(),
            EnrolmentEvent::Issued {
                digest: Digest::from_bytes([2; 32]),
                worker: id(3),
                label: Some(MachineLabel::parse("laptop").unwrap()),
                expires_at: at(),
            }
            .into(),
            EnrolmentEvent::Revoked.into(),
            EnrolmentEvent::Redeemed {
                key: Digest::from_bytes([3; 32]),
            }
            .into(),
            WorkerEvent::Enrolled {
                token: id(4),
                key: Digest::from_bytes([3; 32]),
                name: name.clone(),
                machine: MachineInfo {
                    hostname: name,
                    os: Os::Linux,
                    arch: Arch::Aarch64,
                },
                build: Version::new(0, 1, 0),
            }
            .into(),
            WorkerEvent::SessionStarted {
                session: id(5),
                build: Version::new(0, 1, 1),
            }
            .into(),
            WorkerEvent::SessionEnded {
                session: id(5),
                reason: SessionEndReason::ServerRestart,
            }
            .into(),
            WorkerEvent::Revoked.into(),
        ]
    }

    #[test]
    fn every_event_survives_encode_and_decode() {
        for event in every_event() {
            let payload = event.encode().unwrap();
            assert!(payload["type"].is_string(), "{event:?} is tagged");
            assert_eq!(
                event.schema_version(),
                SchemaVersion(1),
                "every payload is at version 1"
            );
            assert_eq!(
                Event::decode(event.stream_kind(), event.schema_version().0, payload).unwrap(),
                event,
                "decodes to itself"
            );
        }
    }

    #[test]
    fn every_stream_kind_is_covered_and_named() {
        let kinds: Vec<StreamKind> = every_event().iter().map(Event::stream_kind).collect();
        for kind in StreamKind::ALL {
            assert!(kinds.contains(&kind), "{kind:?} has an event");
            assert_eq!(
                StreamKind::parse(kind.as_str()),
                Some(kind),
                "{kind:?} round-trips"
            );
        }
        assert_eq!(StreamKind::parse("run"), None, "not yet");
    }

    #[test]
    fn the_stored_tags_are_stable() {
        let tags: Vec<String> = every_event()
            .iter()
            .map(|event| event.encode().unwrap()["type"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            tags,
            [
                "installation.setup_code_issued",
                "installation.claimed",
                "person.registered",
                "workspace.created",
                "workspace.member_added",
                "enrolment_token.issued",
                "enrolment_token.revoked",
                "enrolment_token.redeemed",
                "worker.enrolled",
                "worker.session_started",
                "worker.session_ended",
                "worker.revoked",
            ],
            "the log's event types"
        );
    }

    #[test]
    fn an_unknown_version_or_type_is_undecodable() {
        let payload = WorkerEvent::Revoked.into_payload();
        let future = Event::decode(StreamKind::Worker, 2, payload.clone()).unwrap_err();
        assert_eq!(
            future.to_string(),
            "cannot decode a worker event at schema version 2",
            "a future version is refused"
        );
        assert!(
            future.source.is_none(),
            "no serde error for a version mismatch"
        );
        let misfiled = Event::decode(StreamKind::Person, 1, payload).unwrap_err();
        assert!(
            misfiled.source.is_some(),
            "a worker payload is not a person event"
        );
        assert_eq!(misfiled.kind, "person", "the error names the stream kind");
    }

    #[test]
    fn positions_are_non_negative_and_advance() {
        assert_eq!(
            Position::from_stored(0),
            Ok(Position::ORIGIN),
            "zero is the origin"
        );
        assert_eq!(Position::from_stored(5).map(Position::get), Ok(5), "kept");
        assert_eq!(
            Position::from_stored(-1).unwrap_err().to_string(),
            "event position -1 is negative",
            "negative is corrupt"
        );
        assert_eq!(Position::ORIGIN.next().map(Position::get), Some(1), "next");
        assert_eq!(
            Position::from_stored(i64::MAX).unwrap().next(),
            None,
            "the end of the log"
        );
    }

    #[test]
    fn streams_name_their_kind_and_id() {
        assert_eq!(
            Installation.stream_id(),
            Uuid::nil(),
            "the installation is a singleton"
        );
        assert_eq!(
            <Installation as Stream>::KIND,
            StreamKind::Installation,
            "kind"
        );
        let worker: WorkerId = id(9);
        assert_eq!(
            worker.stream_id(),
            Uuid::from_u128(9),
            "an id is its stream's id"
        );
        assert_eq!(<WorkerId as Stream>::KIND, StreamKind::Worker, "kind");
        assert_eq!(<PersonId as Stream>::KIND, StreamKind::Person, "kind");
        assert_eq!(<WorkspaceId as Stream>::KIND, StreamKind::Workspace, "kind");
        assert_eq!(
            <EnrolmentTokenId as Stream>::KIND,
            StreamKind::EnrolmentToken,
            "kind"
        );
        let person: PersonId = id(10);
        let workspace: WorkspaceId = id(11);
        let token: EnrolmentTokenId = id(12);
        assert_eq!(person.stream_id(), Uuid::from_u128(10), "person");
        assert_eq!(workspace.stream_id(), Uuid::from_u128(11), "workspace");
        assert_eq!(token.stream_id(), Uuid::from_u128(12), "token");
    }

    impl WorkerEvent {
        fn into_payload(self) -> serde_json::Value {
            Event::from(self).encode().unwrap()
        }
    }
}
