//! The only writer of view tables. Views are a function of the log: `apply` maps one recorded
//! event to view writes, and `testing::assert_views_replay` checks that replaying the whole log
//! through `apply` reproduces the views exactly.
//!
//! Rules:
//! * every value written comes from `recorded` (no `now()`, no `random()`, no sequences);
//! * every touched row gets `changed_at = recorded.position`;
//! * statements are inserts, upserts or keyed updates, so applying an event is deterministic;
//! * the match below is exhaustive over `Event`, so a new event fails the build until projected.

use abc_core::event::{
    EnrolmentEvent, Event, InstallationEvent, PersonEvent, Recorded, RunEvent, WorkerEvent,
    WorkspaceEvent,
};
use abc_core::identity::MachineLabel;
use sqlx::PgConnection;

use crate::error::StoreError;

/// Applies one event to the views, inside the transaction that inserted it (or inside the replay
/// check's transaction).
pub(crate) async fn apply(conn: &mut PgConnection, recorded: &Recorded) -> Result<(), StoreError> {
    let row = Row {
        at: recorded.recorded_at.as_offset(),
        changed_at: recorded.position.get(),
        stream: recorded.stream.id,
        workspace: recorded.workspace.as_uuid(),
        actor: recorded.actor.id(),
        event: recorded.id.as_uuid(),
    };
    match &recorded.event {
        Event::Installation(event) => installation(conn, &row, event).await,
        Event::Person(event) => person(conn, &row, event).await,
        Event::Workspace(event) => workspace(conn, &row, event).await,
        Event::Enrolment(event) => enrolment(conn, &row, event).await,
        Event::Worker(event) => worker(conn, &row, event).await,
        Event::Run(event) => run(conn, &row, event).await,
    }
}

/// What view rows take from an event's envelope.
#[derive(Clone, Copy)]
struct Row {
    /// When it was recorded.
    at: time::OffsetDateTime,
    /// Its position: every touched row's `changed_at`.
    changed_at: i64,
    /// Its stream's id: the id of the entity it is about.
    stream: uuid::Uuid,
    /// Its workspace.
    workspace: uuid::Uuid,
    /// Its actor's id (none for the server).
    actor: Option<uuid::Uuid>,
    /// Its own id.
    event: uuid::Uuid,
}

async fn installation(
    conn: &mut PgConnection,
    row: &Row,
    event: &InstallationEvent,
) -> Result<(), StoreError> {
    let Row { at, changed_at, .. } = *row;
    match event {
        InstallationEvent::SetupCodeIssued {
            digest,
            owner,
            workspace,
            expires_at,
        } => {
            sqlx::query!(
                "INSERT INTO setup_code (singleton, digest, owner_id, workspace_id, expires_at,
                    changed_at)
                 VALUES (true, $1, $2, $3, $4, $5)
                 ON CONFLICT (singleton) DO UPDATE SET digest = EXCLUDED.digest,
                    owner_id = EXCLUDED.owner_id, workspace_id = EXCLUDED.workspace_id,
                    expires_at = EXCLUDED.expires_at, changed_at = EXCLUDED.changed_at",
                digest.as_bytes().as_slice(),
                owner.as_uuid(),
                workspace.as_uuid(),
                expires_at.as_offset(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        InstallationEvent::Claimed { owner, workspace } => {
            sqlx::query!(
                "INSERT INTO installation (singleton, owner_id, workspace_id, claimed_at, changed_at)
                 VALUES (true, $1, $2, $3, $4)",
                owner.as_uuid(),
                workspace.as_uuid(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
            // The code is spent: nothing can be issued or claimed once claimed.
            sqlx::query!("DELETE FROM setup_code")
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

async fn person(conn: &mut PgConnection, row: &Row, event: &PersonEvent) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        ..
    } = *row;
    match event {
        PersonEvent::Registered {
            display_name,
            tailnet_login,
        } => {
            sqlx::query!(
                "INSERT INTO actor (id, kind, display_name, created_at, changed_at)
                 VALUES ($1, 'person', $2, $3, $4)",
                stream,
                display_name.as_str(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "INSERT INTO tailnet_identity (login, person_id) VALUES ($1, $2)",
                tailnet_login.as_str(),
                stream,
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn workspace(
    conn: &mut PgConnection,
    row: &Row,
    event: &WorkspaceEvent,
) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        ..
    } = *row;
    match event {
        WorkspaceEvent::Created { name } => {
            sqlx::query!(
                "INSERT INTO workspace (id, name, created_at, changed_at) VALUES ($1, $2, $3, $4)",
                stream,
                name.as_str(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        WorkspaceEvent::MemberAdded { person, role } => {
            sqlx::query!(
                "INSERT INTO membership (workspace_id, person_id, role, since, changed_at)
                 VALUES ($1, $2, $3, $4, $5)",
                stream,
                person.as_uuid(),
                role.as_str(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "UPDATE workspace SET changed_at = $2 WHERE id = $1",
                stream,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn enrolment(
    conn: &mut PgConnection,
    row: &Row,
    event: &EnrolmentEvent,
) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        workspace,
        actor,
        event: event_id,
    } = *row;
    match event {
        EnrolmentEvent::Issued {
            digest,
            worker,
            label,
            expires_at,
        } => {
            sqlx::query!(
                "INSERT INTO enrolment_token (id, workspace_id, digest, reserved_worker, label,
                    issued_by, issued_at, issued_event, expires_at, changed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                stream,
                workspace,
                digest.as_bytes().as_slice(),
                worker.as_uuid(),
                label.as_ref().map(MachineLabel::as_str),
                actor,
                at,
                event_id,
                expires_at.as_offset(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        EnrolmentEvent::Revoked => {
            sqlx::query!(
                "UPDATE enrolment_token SET revoked_at = $2, changed_at = $3 WHERE id = $1",
                stream,
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        EnrolmentEvent::Redeemed { key } => {
            sqlx::query!(
                "UPDATE enrolment_token SET redeemed_at = $2, redeemed_key = $3, changed_at = $4
                 WHERE id = $1",
                stream,
                at,
                key.as_bytes().as_slice(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn worker(conn: &mut PgConnection, row: &Row, event: &WorkerEvent) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        workspace,
        ..
    } = *row;
    match event {
        WorkerEvent::Enrolled {
            token,
            key,
            name,
            machine,
            build,
        } => {
            sqlx::query!(
                "INSERT INTO actor (id, kind, display_name, created_at, changed_at)
                 VALUES ($1, 'worker', $2, $3, $4)",
                stream,
                name.as_str(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "INSERT INTO worker (id, workspace_id, key_digest, enrolled_via, hostname, os, arch,
                    build, enrolled_at, changed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                stream,
                workspace,
                key.as_bytes().as_slice(),
                token.as_uuid(),
                machine.hostname.as_str(),
                machine.os.as_str(),
                machine.arch.as_str(),
                build.to_string(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        WorkerEvent::SessionStarted { session, build } => {
            sqlx::query!(
                "UPDATE worker SET open_session = $2, last_session_started_at = $3, build = $4,
                    changed_at = $5
                 WHERE id = $1",
                stream,
                session.as_uuid(),
                at,
                build.to_string(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        WorkerEvent::SessionEnded { session, reason: _ } => {
            // Only the session that is open ends. `decide` already checked; the condition keeps
            // replay exact even if two ends for one session were ever recorded.
            sqlx::query!(
                "UPDATE worker SET open_session = NULL, last_session_ended_at = $3, changed_at = $4
                 WHERE id = $1 AND open_session = $2",
                stream,
                session.as_uuid(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        WorkerEvent::Revoked => {
            sqlx::query!(
                "UPDATE worker SET revoked_at = $2, changed_at = $3 WHERE id = $1",
                stream,
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn run(conn: &mut PgConnection, row: &Row, event: &RunEvent) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        workspace,
        actor,
        event: event_id,
    } = *row;
    match event {
        RunEvent::Started { graph } => {
            let starter = actor.ok_or(StoreError::Corrupt("run.started has no actor"))?;
            sqlx::query!(
                "INSERT INTO run (id, workspace_id, graph, started_by, started_at, started_event,
                    changed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                stream,
                workspace,
                graph.as_bytes().as_slice(),
                starter,
                at,
                event_id,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        RunEvent::LeaseGranted { lease, worker } => {
            sqlx::query!(
                "INSERT INTO run_lease (id, run_id, worker_id, epoch, granted_at, changed_at)
                 SELECT $1, id, $3, epoch, $4, $5 FROM run WHERE id = $2",
                lease.as_uuid(),
                stream,
                worker.as_uuid(),
                at,
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "UPDATE run SET live_lease = $2, changed_at = $3 WHERE id = $1",
                stream,
                lease.as_uuid(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
        RunEvent::LeaseExpired { lease } => {
            end_lease(conn, row, *lease, "expired", None).await?;
        }
        RunEvent::ResultApplied { lease, result } => {
            let result = serde_json::to_value(result).map_err(StoreError::Encode)?;
            end_lease(conn, row, *lease, "applied", Some(result)).await?;
            sqlx::query!("UPDATE run SET epoch = epoch + 1 WHERE id = $1", stream,)
                .execute(&mut *conn)
                .await?;
        }
        RunEvent::Answered { answer } => {
            let answer = serde_json::to_value(answer).map_err(StoreError::Encode)?;
            sqlx::query!(
                "UPDATE run SET last_answer = $2, changed_at = $3 WHERE id = $1",
                stream,
                answer.as_str(),
                changed_at,
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Ends a lease and frees its run.
async fn end_lease(
    conn: &mut PgConnection,
    row: &Row,
    lease: abc_core::id::LeaseId,
    how: &str,
    result: Option<serde_json::Value>,
) -> Result<(), StoreError> {
    let Row {
        at,
        changed_at,
        stream,
        ..
    } = *row;
    sqlx::query!(
        "UPDATE run_lease SET ended = $2, ended_at = $3, result = $4, changed_at = $5
         WHERE id = $1",
        lease.as_uuid(),
        how,
        at,
        result,
        changed_at,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE run SET live_lease = NULL, changed_at = $3 WHERE id = $1 AND live_lease = $2",
        stream,
        lease.as_uuid(),
        changed_at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}
