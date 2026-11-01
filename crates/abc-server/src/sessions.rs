//! Live worker sockets in this process, for one job only: newest wins.
//!
//! A laptop waking from sleep usually reconnects before the server has noticed that its old socket
//! is dead. Attaching a session for a worker that already has one here kicks the old one, which
//! closes with 4409 (`Superseded`). Whether a worker is *online* is not kept here: it is the
//! `worker` view's `open_session`, derived from the log (`session_started` / `session_ended`), so
//! the web app and every other reader see one truth.
//!
//! Each session owns its own entry through an RAII [`Attached`] guard (per-actor state); nothing
//! else writes the map.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use abc_core::id::{SessionId, WorkerId};
use tokio::sync::oneshot;

/// The registry. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sessions {
    live: Arc<Mutex<BTreeMap<WorkerId, Live>>>,
}

#[derive(Debug)]
struct Live {
    session: SessionId,
    kick: oneshot::Sender<Kick>,
}

/// Why a session is told to close from outside its own loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kick {
    /// A newer session for the same worker attached.
    Superseded,
    /// The server is shutting down.
    Shutdown,
}

/// Held by a session while attached. Dropping it removes the entry only if it is still this
/// session's (a superseded session's drop changes nothing).
#[derive(Debug)]
pub(crate) struct Attached {
    sessions: Sessions,
    worker: WorkerId,
    session: SessionId,
}

impl Sessions {
    /// Registers `session` for `worker`, kicking any previous one with `Superseded`. Returns the
    /// guard and the receiver this session's loop selects on.
    pub(crate) fn attach(
        &self,
        worker: WorkerId,
        session: SessionId,
    ) -> (Attached, oneshot::Receiver<Kick>) {
        let _ = (worker, session);
        unimplemented!("lock; replace; send Superseded to the replaced entry's kick")
    }

    /// Kicks every session with `Shutdown` (graceful drain).
    pub(crate) fn shutdown_all(&self) {
        unimplemented!()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        // lock; remove only if live[worker].session == self.session.
        let _ = (&self.sessions, self.worker, self.session);
    }
}
