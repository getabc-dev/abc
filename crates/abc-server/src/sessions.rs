//! Live worker sockets in this process, for one job only: newest wins.
//!
//! A laptop waking from sleep usually reconnects before the server has noticed that its old socket
//! is dead. Attaching a session for a worker that already has one here kicks the old one, which
//! closes with 4409 (`Superseded`). Whether a worker is *online* is not kept here: it is the
//! `worker` view's `open_session`, derived from the log, so the web app and every other reader see
//! one truth.
//!
//! Each session owns its own entry through an RAII [`Attached`] guard; nothing else writes the map.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use abc_core::id::{SessionId, WorkerId};
use tokio::sync::{oneshot, watch};

/// The registry. Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Sessions {
    live: Arc<Mutex<BTreeMap<WorkerId, Live>>>,
    /// Held while a session records its start and attaches, so starts happen one at a time: the
    /// session recorded last is always the one attached, and a kicked session's end then always
    /// finds a newer session open and records nothing.
    admission: Arc<tokio::sync::Mutex<()>>,
    /// How many sessions are attached, so a drain can wait for them to record their end.
    count: Arc<watch::Sender<usize>>,
    /// How many sockets each worker holds, before and after its handshake.
    sockets: Arc<Mutex<BTreeMap<WorkerId, u32>>>,
}

/// One of a worker's sockets, counted until it drops.
#[derive(Debug)]
pub(crate) struct Socket {
    sockets: Arc<Mutex<BTreeMap<WorkerId, u32>>>,
    worker: WorkerId,
}

impl Drop for Socket {
    fn drop(&mut self) {
        let mut sockets = self.sockets.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = sockets.get_mut(&self.worker) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sockets.remove(&self.worker);
            }
        }
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            live: Arc::default(),
            admission: Arc::default(),
            count: Arc::new(watch::Sender::new(0)),
            sockets: Arc::default(),
        }
    }
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
/// session's: a superseded session's drop changes nothing.
#[derive(Debug)]
pub(crate) struct Attached {
    sessions: Sessions,
    worker: WorkerId,
    session: SessionId,
}

impl Sessions {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<WorkerId, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one more socket for `worker`, unless it already holds `cap`.
    pub(crate) fn open_socket(&self, worker: WorkerId, cap: u32) -> Option<Socket> {
        let mut sockets = self.sockets.lock().unwrap_or_else(PoisonError::into_inner);
        let count = sockets.entry(worker).or_insert(0);
        if *count >= cap {
            return None;
        }
        *count = count.saturating_add(1);
        Some(Socket {
            sockets: Arc::clone(&self.sockets),
            worker,
        })
    }

    /// Waits for this session's turn to record its start and attach. Hold the guard across both.
    pub(crate) async fn admission(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.admission.lock().await
    }

    /// Registers `session` for `worker`, kicking any previous one with `Superseded`. Returns the
    /// guard and the receiver this session's loop selects on.
    pub(crate) fn attach(
        &self,
        worker: WorkerId,
        session: SessionId,
    ) -> (Attached, oneshot::Receiver<Kick>) {
        let (kick, kicked) = oneshot::channel();
        let replaced = self.map().insert(worker, Live { session, kick });
        if let Some(old) = replaced {
            // The old session may already be gone; then there is no one to tell.
            let _unheard = old.kick.send(Kick::Superseded);
        }
        self.count
            .send_modify(|count| *count = count.saturating_add(1));
        let attached = Attached {
            sessions: self.clone(),
            worker,
            session,
        };
        (attached, kicked)
    }

    /// Kicks every session with `Shutdown`.
    pub(crate) fn shutdown_all(&self) {
        let live = std::mem::take(&mut *self.map());
        for (_, entry) in live {
            let _unheard = entry.kick.send(Kick::Shutdown);
        }
    }

    /// Resolves once no session is attached.
    pub(crate) async fn idle(&self) {
        let mut count = self.count.subscribe();
        // The sender lives as long as `self`, so waiting cannot fail.
        let _idle = count.wait_for(|attached| *attached == 0).await;
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        {
            let mut live = self.sessions.map();
            if live
                .get(&self.worker)
                .is_some_and(|entry| entry.session == self.session)
            {
                live.remove(&self.worker);
            }
        }
        self.sessions
            .count
            .send_modify(|count| *count = count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T: abc_core::id::FromUuid>(n: u128) -> T {
        T::from_uuid(uuid::Uuid::from_u128(n))
    }

    #[tokio::test]
    async fn a_newer_session_kicks_the_older_and_outlives_its_drop() {
        let sessions = Sessions::default();
        let (first, mut first_kicked) = sessions.attach(id(1), id(10));
        assert!(
            first_kicked.try_recv().is_err(),
            "nothing kicks a lone session"
        );
        let (second, mut second_kicked) = sessions.attach(id(1), id(11));
        assert_eq!(
            first_kicked.try_recv(),
            Ok(Kick::Superseded),
            "the older is superseded"
        );
        drop(first);
        assert_eq!(
            sessions.map().get(&id(1)).map(|live| live.session),
            Some(id(11)),
            "the older's drop leaves the newer attached"
        );
        let (other, mut other_kicked) = sessions.attach(id(2), id(20));
        sessions.shutdown_all();
        assert_eq!(
            second_kicked.try_recv(),
            Ok(Kick::Shutdown),
            "shutdown reaches every session"
        );
        assert_eq!(other_kicked.try_recv(), Ok(Kick::Shutdown), "every one");
        assert!(sessions.map().is_empty(), "and empties the registry");
        let waiting = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.idle().await }
        });
        drop(second);
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "one session is still attached");
        drop(other);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn a_worker_holds_at_most_its_cap_of_sockets() {
        let sessions = Sessions::default();
        let first = sessions.open_socket(id(1), 2).unwrap();
        let second = sessions.open_socket(id(1), 2).unwrap();
        assert!(
            sessions.open_socket(id(1), 2).is_none(),
            "the third is refused"
        );
        assert!(
            sessions.open_socket(id(2), 2).is_some(),
            "another worker has its own"
        );
        drop(first);
        let third = sessions.open_socket(id(1), 2);
        assert!(third.is_some(), "a closed socket frees its slot");
        drop((second, third));
        assert!(
            sessions.sockets.lock().unwrap().is_empty(),
            "a worker with no sockets is forgotten"
        );
    }

    #[test]
    fn dropping_the_current_session_detaches_it() {
        let sessions = Sessions::default();
        let (attached, _kicked) = sessions.attach(id(1), id(10));
        drop(attached);
        assert!(sessions.map().is_empty(), "detached");
        assert_eq!(*sessions.count.borrow(), 0, "and uncounted");
    }
}
