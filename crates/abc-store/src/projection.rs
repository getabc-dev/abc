//! The only writer of view tables. Views are a function of the log: `apply` maps one recorded
//! event to view writes, and `testing::assert_views_replay` checks that replaying the whole log
//! through `apply` reproduces the views exactly.
//!
//! Rules:
//! * every value written comes from `recorded` (no `now()`, no `random()`, no sequences);
//! * every touched row gets `changed_at = recorded.position`;
//! * statements are upserts or keyed updates, so applying an event is deterministic;
//! * the match below is exhaustive over `Event`, so a new event fails the build until projected.

use abc_core::event::{
    EnrolmentEvent, Event, InstallationEvent, PersonEvent, Recorded, WorkerEvent, WorkspaceEvent,
};
use sqlx::PgConnection;

use crate::error::StoreError;

/// Applies one event to the views, inside the transaction that inserted it (or inside the replay
/// test's transaction).
pub(crate) async fn apply(conn: &mut PgConnection, recorded: &Recorded) -> Result<(), StoreError> {
    match &recorded.event {
        Event::Installation(InstallationEvent::SetupCodeIssued {
            digest,
            owner,
            workspace,
            expires_at,
        }) => {
            // INSERT INTO setup_code … ON CONFLICT (singleton) DO UPDATE (a newer code replaces it)
            let _ = (digest, owner, workspace, expires_at);
        }
        Event::Installation(InstallationEvent::Claimed { owner, workspace }) => {
            // INSERT INTO installation (owner_id, workspace_id, claimed_at, changed_at);
            // DELETE FROM setup_code (spent; nothing can be issued once claimed)
            let _ = (owner, workspace);
        }
        Event::Person(PersonEvent::Registered {
            display_name,
            tailnet_login,
        }) => {
            // INSERT INTO actor (id = stream_id, kind 'person', display_name, created_at, changed_at)
            // INSERT INTO tailnet_identity (login, person_id)
            let _ = (display_name, tailnet_login);
        }
        Event::Workspace(WorkspaceEvent::Created { name }) => {
            // INSERT INTO workspace
            let _ = name;
        }
        Event::Workspace(WorkspaceEvent::MemberAdded { person, role }) => {
            // INSERT INTO membership; UPDATE workspace SET changed_at
            let _ = (person, role);
        }
        Event::Enrolment(EnrolmentEvent::Issued {
            digest,
            worker,
            label,
            expires_at,
        }) => {
            // INSERT INTO enrolment_token (issued_by = actor id, issued_event = recorded.id, …)
            let _ = (digest, worker, label, expires_at);
        }
        Event::Enrolment(EnrolmentEvent::Revoked) => {
            // UPDATE enrolment_token SET revoked_at = recorded_at, changed_at
        }
        Event::Enrolment(EnrolmentEvent::Redeemed { key }) => {
            // UPDATE enrolment_token SET redeemed_at = recorded_at, redeemed_key = key, changed_at
            let _ = key;
        }
        Event::Worker(WorkerEvent::Enrolled {
            token,
            key,
            name,
            machine,
            build,
        }) => {
            // INSERT INTO actor (id = stream_id, kind 'worker', display_name = name)
            // INSERT INTO worker (id, workspace_id, key_digest, enrolled_via = token, …)
            let _ = (token, key, name, machine, build);
        }
        Event::Worker(WorkerEvent::SessionStarted { session, build }) => {
            // UPDATE worker SET open_session = session, last_session_started_at = recorded_at, build, changed_at
            let _ = (session, build);
        }
        Event::Worker(WorkerEvent::SessionEnded { session, reason }) => {
            // UPDATE worker SET open_session = NULL, last_session_ended_at = recorded_at, changed_at
            //   WHERE id = stream_id AND open_session = session   (decide already checked; this keeps
            //   replay exact even if two ends for one session were ever recorded)
            let _ = (session, reason);
        }
        Event::Worker(WorkerEvent::Revoked) => {
            // UPDATE worker SET revoked_at = recorded_at, changed_at
        }
    }
    let _ = conn;
    unimplemented!()
}
