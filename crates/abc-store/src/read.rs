//! The read pipeline: authenticate, then the same `abc_core::policy::authorize`, then fetch, all in
//! one REPEATABLE READ, READ ONLY transaction (one consistent snapshot, no head lock).

use std::future::Future;

use abc_core::actor::Principal;
use abc_core::policy::{Authorized, authorize};
use abc_core::query::{
    ListEnrolmentTokens, ListMachines, MachinesSince, OpenSessions, Query, WhoAmI, WorkerSelf,
};
use abc_core::time::Timestamp;
use sqlx::PgConnection;

use crate::error::{QueryError, StoreError};
use crate::{Caller, Inner};

mod sealed {
    pub trait Sealed {}
}

/// A query the store can answer. Sealed; one impl per abc-core query. `fetch` takes the
/// `Authorized<Self>`, so no implementation can run without the check (D10 for reads, I4).
pub trait Fetchable: Query + Send + sealed::Sealed {
    /// Reads the answer.
    fn fetch<'c>(
        auth: &'c Authorized<Self>,
        principal: &'c Principal,
        conn: &'c mut PgConnection,
        now: Timestamp,
    ) -> impl Future<Output = Result<Self::Answer, StoreError>> + Send + 'c;
}

/// The whole pipeline for one query.
pub(crate) async fn run<Q: Fetchable>(
    inner: &Inner,
    caller: &Caller,
    query: Q,
) -> Result<Q::Answer, QueryError> {
    // let mut tx = begin REPEATABLE READ, READ ONLY;
    // let principal = crate::execute::authenticate(&mut tx, caller).await?;
    // let auth = authorize(&principal, query, &(), now)?;
    // let answer = Q::fetch(&auth, &principal, &mut tx, now).await?;
    // tx.commit(); Ok(answer)
    let _ = (inner, caller, query, authorize::<Q>);
    unimplemented!()
}

// Fetchable impls (each also implements sealed::Sealed):
//
// WhoAmI             principal Anonymous -> NoIdentity; Visitor + no installation row -> Unclaimed;
//                    Visitor + claimed -> NotAMember; Person -> Member { display_name from actor,
//                    workspaces joined from membership + workspace }.
// ListMachines       SELECT head FROM event_log_head; SELECT worker JOIN actor WHERE workspace_id
//                    ORDER BY enrolled_at; online = open_session IS NOT NULL. One snapshot, so head
//                    is exact.
// MachinesSince      SELECT head; SELECT … WHERE workspace_id = $1 AND changed_at > $after
//                    (index worker_changes). An empty result still returns the new head.
// ListEnrolmentTokens SELECT … ORDER BY issued_at DESC; status derived from the timestamps against `now`.
// OpenSessions       SELECT id, workspace_id, open_session FROM worker WHERE open_session IS NOT NULL
//                    (index worker_open_sessions). Server only; the boot sweep's input.
// WorkerSelf         from the principal alone: Worker { id, workspace } -> WorkerIdentity. Running
//                    it is the point: authenticate() fails with WorkerRevoked once revoked.
macro_rules! fetchable {
    ($($query:ty),*) => {$(
        impl sealed::Sealed for $query {}
        impl Fetchable for $query {
            fn fetch<'c>(
                auth: &'c Authorized<Self>,
                principal: &'c Principal,
                conn: &'c mut PgConnection,
                now: Timestamp,
            ) -> impl Future<Output = Result<Self::Answer, StoreError>> + Send + 'c {
                async move {
                    let _ = (auth, principal, conn, now);
                    unimplemented!("see the table above")
                }
            }
        }
    )*};
}
fetchable!(
    WhoAmI,
    ListMachines,
    MachinesSince,
    ListEnrolmentTokens,
    WorkerSelf,
    OpenSessions
);
