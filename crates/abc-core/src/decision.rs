//! What a command decided: events to append plus a typed outcome for the caller.
//!
//! A [`Decision`] can only come from a [`DecisionBuilder`], and a builder can only come from an
//! `&Authorized<C>`. The builder copies the authorized actor and workspace and stamps them on
//! every event, so no `decide` body can record the wrong actor or forget the workspace. The store's
//! append accepts nothing but a `Decision`.

use uuid::Uuid;
use zeroize::Zeroizing;

use crate::command::Command;
use crate::event::{Event, Stream, StreamRef};
use crate::id::{CorrelationId, EventId, FromUuid};
use crate::policy::{Authorized, WritePermit};
use crate::time::Timestamp;

/// The source of fresh ids and secret bytes. Implemented by the store: `OsMint` (OS entropy, UUIDv7)
/// in production, `SeededMint` (ChaCha with a fixed seed) in tests, which keeps every test
/// deterministic (P2). The worker uses `OsMint` too, to generate its key.
pub trait Mint: Send + Sync + std::fmt::Debug {
    /// A UUIDv7 whose time part is `now`.
    fn uuid(&self, now: Timestamp) -> Uuid;
    /// 32 bytes of secret entropy, zeroized when dropped.
    fn secret(&self) -> Zeroizing<[u8; 32]>;
}

/// Why a request happened: what the store writes into every event's `correlation_id` and
/// `causation_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    /// The request (HTTP: the request id) or, from S1, the graph run.
    pub correlation: CorrelationId,
    /// The event this command reacts to, if any (S1 runner reactions).
    pub causation: Option<EventId>,
}

/// What `decide` may use besides its facts: the request's instant and a mint.
#[derive(Debug)]
pub struct DecideCtx<'a> {
    now: Timestamp,
    mint: &'a dyn Mint,
}

impl<'a> DecideCtx<'a> {
    /// Built by the store, once per request.
    #[must_use]
    pub const fn new(now: Timestamp, mint: &'a dyn Mint) -> Self {
        Self { now, mint }
    }

    /// The request's instant: the same `now` the policy saw.
    #[must_use]
    pub const fn now(&self) -> Timestamp {
        self.now
    }

    /// A fresh id of any kind.
    #[must_use]
    pub fn id<T: FromUuid>(&self) -> T {
        T::from_uuid(self.mint.uuid(self.now))
    }

    /// The mint, for credential generation (`EnrolmentToken::generate`).
    #[must_use]
    pub const fn mint(&self) -> &'a dyn Mint {
        self.mint
    }
}

/// An event waiting to be appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    stream: StreamRef,
    event: Event,
}

impl Pending {
    /// Its stream.
    #[must_use]
    pub const fn stream(&self) -> StreamRef {
        self.stream
    }

    /// Its payload.
    #[must_use]
    pub const fn event(&self) -> &Event {
        &self.event
    }
}

/// Collects a command's events under its authorization.
#[derive(Debug)]
#[must_use]
pub struct DecisionBuilder {
    permit: WritePermit,
    cause: Option<EventId>,
    events: Vec<Pending>,
}

impl DecisionBuilder {
    /// Starts a decision for an authorized command. Taking the `Authorized` here is what makes it
    /// impossible to decide (and therefore to append) without passing the check.
    pub fn new<C: Command>(auth: &Authorized<C>) -> Self {
        Self {
            permit: *auth.permit(),
            cause: None,
            events: Vec::new(),
        }
    }

    /// Appends `event` to `stream`. The `Stream` trait makes a mismatched pair a type error.
    pub fn emit<S: Stream>(&mut self, stream: S, event: S::Event) {
        self.events.push(Pending {
            stream: StreamRef {
                kind: S::KIND,
                id: stream.stream_id(),
            },
            event: event.into(),
        });
    }

    /// Records that this decision's events were caused by `event` (for example, enrolment by the
    /// token's `issued` event). An explicit cause overrides `Origin::causation`.
    pub const fn caused_by(&mut self, event: EventId) {
        self.cause = Some(event);
    }

    /// Finishes with the outcome. Zero events is a valid decision: it is how an idempotent retry
    /// answers ("already enrolled, here is your worker").
    pub fn finish<O>(self, outcome: O) -> Decision<O> {
        Decision {
            permit: self.permit,
            cause: self.cause,
            events: self.events,
            outcome,
        }
    }
}

/// A command's decision. Only the store consumes it.
#[derive(Debug)]
#[must_use = "a decision that is not appended changes nothing"]
pub struct Decision<O> {
    permit: WritePermit,
    cause: Option<EventId>,
    events: Vec<Pending>,
    outcome: O,
}

impl<O> Decision<O> {
    /// The authorized actor and workspace, stamped on every event.
    #[must_use]
    pub const fn permit(&self) -> WritePermit {
        self.permit
    }

    /// The explicit cause, if `decide` set one.
    #[must_use]
    pub const fn cause(&self) -> Option<EventId> {
        self.cause
    }

    /// The events to append, in order.
    #[must_use]
    pub fn events(&self) -> &[Pending] {
        &self.events
    }

    /// The outcome for the caller, once the events are durable.
    pub fn into_outcome(self) -> O {
        self.outcome
    }
}
