//! Reading events back from the log: rows decoded into [`Recorded`] values.

use abc_core::actor::Actor;
use abc_core::event::{Event, Position, Recorded, StreamKind, StreamRef};
use abc_core::id::{CorrelationId, EventId, FromUuid as _, WorkspaceId};
use abc_core::time::Timestamp;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::StoreError;

/// One `event` row as stored.
struct Row {
    position: i64,
    id: Uuid,
    recorded_at: time::OffsetDateTime,
    workspace_id: Uuid,
    stream_kind: String,
    stream_id: Uuid,
    stream_seq: i32,
    schema_version: i16,
    actor_kind: String,
    actor_id: Option<Uuid>,
    correlation_id: Uuid,
    causation_id: Option<Uuid>,
    payload: serde_json::Value,
}

impl TryFrom<Row> for Recorded {
    type Error = StoreError;

    fn try_from(row: Row) -> Result<Self, StoreError> {
        let kind =
            StreamKind::parse(&row.stream_kind).ok_or(StoreError::Corrupt("event.stream_kind"))?;
        Ok(Self {
            position: Position::from_stored(row.position)
                .ok()
                .ok_or(StoreError::Corrupt("event.position"))?,
            id: EventId::from_uuid(row.id),
            recorded_at: Timestamp::from_utc(row.recorded_at),
            workspace: WorkspaceId::from_uuid(row.workspace_id),
            stream: StreamRef {
                kind,
                id: row.stream_id,
            },
            stream_seq: row.stream_seq,
            actor: Actor::from_stored(&row.actor_kind, row.actor_id)
                .ok()
                .ok_or(StoreError::Corrupt("event.actor"))?,
            correlation: CorrelationId::from_uuid(row.correlation_id),
            causation: row.causation_id.map(EventId::from_uuid),
            event: Event::decode(kind, row.schema_version, row.payload).map_err(|source| {
                StoreError::CorruptEvent {
                    position: row.position,
                    source,
                }
            })?,
        })
    }
}

/// Every event in the log, in order: what replaying the views starts from.
#[cfg(feature = "testing")]
pub(crate) async fn all(conn: &mut PgConnection) -> Result<Vec<Recorded>, StoreError> {
    let rows = sqlx::query_as!(
        Row,
        "SELECT position, id, recorded_at, workspace_id, stream_kind, stream_id, stream_seq,
                schema_version, actor_kind, actor_id, correlation_id, causation_id, payload
         FROM event ORDER BY position"
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(Recorded::try_from).collect()
}

/// One stream's events, in the order they happened.
pub(crate) async fn stream(
    conn: &mut PgConnection,
    stream: StreamRef,
) -> Result<Vec<Recorded>, StoreError> {
    let rows = sqlx::query_as!(
        Row,
        "SELECT position, id, recorded_at, workspace_id, stream_kind, stream_id, stream_seq,
                schema_version, actor_kind, actor_id, correlation_id, causation_id, payload
         FROM event WHERE stream_kind = $1 AND stream_id = $2 ORDER BY stream_seq",
        stream.kind.as_str(),
        stream.id,
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(Recorded::try_from).collect()
}
