//! abc-store's typed errors (D11). abc-server maps each variant to one HTTP status and one stable
//! `ApiCode`, exhaustively.

use abc_core::event::UndecodableEvent;
use abc_core::policy::Denied;
use thiserror::Error;

/// Opening the store failed.
#[derive(Debug, Error)]
pub enum OpenError {
    /// The database URL does not parse. The message never contains the URL.
    #[error(
        "ABC_DATABASE_URL is not a valid Postgres URL, such as postgres://user:password@host/abc"
    )]
    InvalidUrl,
    /// Postgres did not accept a connection in time. The message never contains the URL.
    #[error("could not connect to Postgres (check ABC_DATABASE_URL and that the database is up)")]
    Connect(#[source] sqlx::Error),
    /// A migration failed.
    #[error("database migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// The applied migrations are not exactly this build's: not migrated yet, or migrated by a
    /// newer build.
    #[error("the database schema does not match this build; start `abc server`, which migrates it")]
    NotMigrated,
    /// Another database error while opening.
    #[error(transparent)]
    Database(#[from] StoreError),
}

/// The database failed, or a stored value is corrupt. Not the caller's fault.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A query failed.
    #[error("database error")]
    Database(#[source] sqlx::Error),
    /// A stored event cannot be decoded by this build.
    #[error("event at position {position} cannot be decoded")]
    CorruptEvent {
        /// Where.
        position: i64,
        /// Why.
        #[source]
        source: UndecodableEvent,
    },
    /// A stored view value cannot be read by this build, or the store found itself in a state it
    /// never creates.
    #[error("stored state is inconsistent: {0}")]
    Corrupt(&'static str),
    /// An event payload could not be encoded.
    #[error("an event payload could not be encoded")]
    Encode(#[source] serde_json::Error),
    /// The log reached `i64::MAX` positions.
    #[error("the event log is full")]
    LogFull,
    /// No randomness could be drawn for ids and secrets.
    #[error(transparent)]
    Entropy(#[from] abc_platform::entropy::EntropyUnavailable),
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// The caller's credential did not resolve to a principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AuthError {
    /// No worker has this key.
    #[error("unknown worker key")]
    UnknownWorkerKey,
    /// The worker was revoked. The worker should stop retrying and ask its user to re-enrol.
    #[error("this worker was revoked")]
    WorkerRevoked,
}

/// Why a command did not run. `R` is the command's own rejection type.
#[derive(Debug, Error)]
pub enum ExecuteError<R: std::error::Error + 'static> {
    /// Authentication failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The policy refused.
    #[error(transparent)]
    Denied(#[from] Denied),
    /// The command's own rules refused.
    #[error(transparent)]
    Rejected(R),
    /// The database failed. The transaction rolled back, so nothing was written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Why a query did not run.
#[derive(Debug, Error)]
pub enum QueryError {
    /// Authentication failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The policy refused.
    #[error(transparent)]
    Denied(#[from] Denied),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What makes the store not ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NotReady {
    /// Postgres did not answer within a second.
    #[error("database unreachable")]
    Database,
    /// The log listener is reconnecting.
    #[error("log listener disconnected")]
    Listener,
}

/// Reads a stored value with `parse`, or reports which column is corrupt.
pub(crate) fn stored<T, E>(column: &'static str, parsed: Result<T, E>) -> Result<T, StoreError> {
    parsed.ok().ok_or(StoreError::Corrupt(column))
}
