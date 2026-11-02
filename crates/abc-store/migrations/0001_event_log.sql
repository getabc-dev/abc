-- The event log: abc's system of record.
--
-- Every write in abc goes through abc-store's `execute`, which in ONE transaction locks the head,
-- authenticates, checks permission, decides, inserts events here and updates the views from those
-- events (abc-store/src/execute.rs). Views (0002, 0003) are derived from this table, and a test
-- rebuilds them from it and compares (abc_store::testing::assert_views_replay).

-- One row. Every write transaction runs `SELECT position FROM event_log_head FOR UPDATE` as its
-- first statement, so writers run one at a time, no writer waits while holding another lock
-- (no deadlocks), and positions are handed out in commit order with no gaps: a consumer that has
-- read every event up to N has seen everything at or below N, forever.
CREATE TABLE event_log_head (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    position  bigint  NOT NULL CHECK (position >= 0)
);
INSERT INTO event_log_head (singleton, position) VALUES (true, 0);

CREATE TABLE event (
    -- Global, gapless, commit-ordered (see event_log_head). The SSE and S1 cursor.
    position       bigint      PRIMARY KEY CHECK (position > 0),
    id             uuid        NOT NULL UNIQUE,
    -- From the store's clock, not now(), so replaying events reproduces views exactly.
    recorded_at    timestamptz NOT NULL,
    -- Everything belongs to a workspace (I4). No foreign key: the log never depends on views.
    workspace_id   uuid        NOT NULL,
    -- Kept in step with abc_core::event::StreamKind::ALL by a test that reads this constraint.
    stream_kind    text        NOT NULL CHECK (stream_kind IN
                       ('installation', 'person', 'workspace', 'enrolment_token', 'worker')),
    stream_id      uuid        NOT NULL,
    -- 1, 2, 3... within (stream_kind, stream_id). S1 graph runs order their steps by it.
    stream_seq     integer     NOT NULL CHECK (stream_seq > 0),
    -- Derived from the payload's tag so the two can never disagree.
    event_type     text        GENERATED ALWAYS AS (payload ->> 'type') STORED NOT NULL,
    schema_version smallint    NOT NULL CHECK (schema_version > 0),
    -- Every action records its actor (I4). 'agent' joins in S1. The installation itself ('server')
    -- has no id; the CHECK below keeps the pair consistent.
    actor_kind     text        NOT NULL CHECK (actor_kind IN ('person', 'worker', 'server')),
    actor_id       uuid        NULL,
    CONSTRAINT event_actor_id CHECK ((actor_kind = 'server') = (actor_id IS NULL)),
    -- The request (HTTP request id) or, from S1, the graph run.
    correlation_id uuid        NOT NULL,
    -- The event this one reacts to; must exist.
    causation_id   uuid        NULL REFERENCES event (id),
    payload        jsonb       NOT NULL CHECK (jsonb_typeof(payload) = 'object' AND payload ? 'type'),
    CONSTRAINT event_stream_seq UNIQUE (stream_kind, stream_id, stream_seq)
);

-- Per-workspace tailing and per-request (or per-run) tracing.
CREATE INDEX event_by_workspace ON event (workspace_id, position);
CREATE INDEX event_by_correlation ON event (correlation_id, position);

-- Append-only, enforced by the database. abc-store never issues these statements; the triggers
-- make a mistake loud instead of silent.
CREATE FUNCTION abc_reject_event_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'the event log is append-only: % on event is not allowed', TG_OP
        USING ERRCODE = 'restrict_violation';
END
$$;
CREATE TRIGGER event_append_only BEFORE UPDATE OR DELETE ON event
    FOR EACH ROW EXECUTE FUNCTION abc_reject_event_mutation();
CREATE TRIGGER event_no_truncate BEFORE TRUNCATE ON event
    FOR EACH STATEMENT EXECUTE FUNCTION abc_reject_event_mutation();

-- The head only moves forward, and only to the position of the last inserted event.
CREATE FUNCTION abc_check_head_advance() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.position < OLD.position THEN
        RAISE EXCEPTION 'event_log_head moved backwards (% to %)', OLD.position, NEW.position
            USING ERRCODE = 'restrict_violation';
    END IF;
    IF NEW.position > OLD.position AND NOT EXISTS (SELECT 1 FROM event WHERE position = NEW.position) THEN
        RAISE EXCEPTION 'event_log_head moved to % with no event there', NEW.position
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER event_log_head_advances BEFORE UPDATE ON event_log_head
    FOR EACH ROW EXECUTE FUNCTION abc_check_head_advance();
