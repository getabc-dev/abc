-- Views: graph runs and their leases. Same rules as 0002.
--
-- They hold what the run events say and nothing that needs the graph: which step a run is on is
-- the runner's state, rebuilt in memory from the run's stream (spec/runner/), so there is no
-- status column to go stale or to disagree with the graph.

ALTER TABLE event DROP CONSTRAINT event_stream_kind_check;
ALTER TABLE event ADD CONSTRAINT event_stream_kind_check CHECK (stream_kind IN
    ('installation', 'person', 'workspace', 'enrolment_token', 'worker', 'run'));

CREATE TABLE run (
    id           uuid        PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspace (id),
    -- The graph version the run pinned when it started: the sha256 of its canonical form.
    graph        bytea       NOT NULL CHECK (octet_length(graph) = 32),
    -- Who started it (I4).
    started_by   uuid        NOT NULL REFERENCES actor (id),
    started_at   timestamptz NOT NULL,
    started_event uuid       NOT NULL REFERENCES event (id),
    -- The lease that is live now, if any: at most one, and it names a lease of this run.
    live_lease   uuid        NULL,
    -- The last answer a person gave, if any.
    last_answer  text        NULL CHECK (last_answer IN ('retry', 'accept', 'cancel')),
    changed_at   bigint      NOT NULL REFERENCES event (position)
);
CREATE INDEX run_changes ON run (workspace_id, changed_at);

CREATE TABLE run_lease (
    id           uuid        PRIMARY KEY,
    run_id       uuid        NOT NULL REFERENCES run (id),
    worker_id    uuid        NOT NULL REFERENCES worker (id),
    granted_at   timestamptz NOT NULL,
    -- How the lease ended: expired, or its result applied. NULL while live.
    ended        text        NULL CHECK (ended IN ('expired', 'applied')),
    ended_at     timestamptz NULL,
    -- The applied result, as the event carries it.
    result       jsonb       NULL,
    changed_at   bigint      NOT NULL REFERENCES event (position),
    CHECK ((ended IS NULL) = (ended_at IS NULL)),
    CHECK ((result IS NULL) OR ended = 'applied')
);
-- One live lease per run, and one per worker: the invariants the runner relies on, held by the
-- database as well.
CREATE UNIQUE INDEX run_lease_one_live_per_run ON run_lease (run_id) WHERE ended IS NULL;
CREATE UNIQUE INDEX run_lease_one_live_per_worker ON run_lease (worker_id) WHERE ended IS NULL;
CREATE INDEX run_lease_by_run ON run_lease (run_id, granted_at);
