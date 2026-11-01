//! abc-store's typed errors (D11). abc-server maps each variant to one HTTP status and one stable
//! `ApiCode`, exhaustively.

use abc_core::event::UndecodableEvent;
use abc_core::policy::Denied;
use thiserror::Error;

/// Opening the store failed.
#[derive(Debug, Error)]
pub enum OpenError {
    /// Postgres did not accept a connection within the retry budget. The message never contains
    /// the URL.
    #[error("could not connect to Postgres (check ABC_DATABASE_URL and that the database is up)")]
    Connect(#[source] sqlx::Error),
    /// A migration failed.
    #[error("database migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// `Store::open` found migrations not yet applied.
    #[error("the database is not migrated to this build's schema")]
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
    /// The log reached `i64::MAX` positions.
    #[error("the event log is full")]
    LogFull,
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
    /// Postgres did not answer `SELECT 1` within a second.
    #[error("database unreachable")]
    Database,
    /// The log listener is reconnecting.
    #[error("log listener disconnected")]
    Listener,
}
