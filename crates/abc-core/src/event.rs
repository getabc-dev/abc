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
#[ts(export, type = "number")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    pub fn schema_version(&self) -> u16 {
        unimplemented!("exhaustive match; every variant is 1 today")
    }

    /// The JSON stored in `event.payload`. It always includes `type`.
    ///
    /// # Errors
    ///
    /// Returns the serde error, which cannot happen for these types but is not hidden.
    pub fn encode(&self) -> Result<serde_json::Value, serde_json::Error> {
        unimplemented!("serialize the inner enum")
    }

    /// Decodes a stored payload, choosing the enum from the row's `stream_kind`.
    ///
    /// # Errors
    ///
    /// Returns [`UndecodableEvent`] for an unknown type or version: a corrupt or future log.
    pub fn decode(
        kind: StreamKind,
        version: u16,
        payload: serde_json::Value,
    ) -> Result<Self, UndecodableEvent> {
        let _ = (kind, version, payload);
        unimplemented!()
    }
}

/// A stored payload this build cannot read.
#[derive(Debug, Error)]
#[error("cannot decode a {kind} event at schema version {version}")]
pub struct UndecodableEvent {
    /// The stream kind's name.
    pub kind: &'static str,
    /// The stored schema version.
    pub version: u16,
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
