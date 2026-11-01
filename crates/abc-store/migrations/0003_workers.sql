-- Views: enrolment tokens and workers. Same rules as 0002.
--
-- No status columns: a token's status (open / expired / revoked / redeemed) and a worker's
-- (active / revoked) are derived when read from the timestamps below, so they cannot go stale and
-- need no sweeper.

CREATE TABLE enrolment_token (
    id              uuid        PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspace (id),
    -- SHA-256 of the token. The plaintext is never stored anywhere.
    digest          bytea       NOT NULL UNIQUE CHECK (octet_length(digest) = 32),
    -- The worker id this token will create, reserved at issue (idempotent redemption).
    reserved_worker uuid        NOT NULL UNIQUE,
    label           text        NULL CHECK (char_length(label) BETWEEN 1 AND 64),
    issued_by       uuid        NOT NULL REFERENCES actor (id),
    issued_at       timestamptz NOT NULL,
    issued_event    uuid        NOT NULL REFERENCES event (id),
    expires_at      timestamptz NOT NULL CHECK (expires_at > issued_at),
    revoked_at      timestamptz NULL,
    redeemed_at     timestamptz NULL,
    redeemed_key    bytea       NULL CHECK (octet_length(redeemed_key) = 32),
    changed_at      bigint      NOT NULL REFERENCES event (position),
    CHECK ((redeemed_at IS NULL) = (redeemed_key IS NULL)),
    -- A token ends one way: revoked or redeemed, never both.
    CHECK (revoked_at IS NULL OR redeemed_at IS NULL)
);
CREATE INDEX enrolment_token_changes ON enrolment_token (workspace_id, changed_at);

CREATE TABLE worker (
    id                      uuid        PRIMARY KEY,
    actor_kind              text        NOT NULL DEFAULT 'worker' CHECK (actor_kind = 'worker'),
    workspace_id            uuid        NOT NULL REFERENCES workspace (id),
    -- Authentication looks workers up by this digest.
    key_digest              bytea       NOT NULL UNIQUE CHECK (octet_length(key_digest) = 32),
    enrolled_via            uuid        NOT NULL UNIQUE REFERENCES enrolment_token (id),
    hostname                text        NOT NULL CHECK (char_length(hostname) BETWEEN 1 AND 64),
    os                      text        NOT NULL CHECK (os IN ('macos', 'linux')),
    arch                    text        NOT NULL CHECK (arch IN ('aarch64', 'x86_64')),
    -- The release at enrolment, then at each session start.
    build                   text        NOT NULL,
    enrolled_at             timestamptz NOT NULL,
    -- The session that is open now, if any: "online". Set by session_started, cleared by the
    -- session_ended that names it (a superseded session's late end leaves it alone).
    open_session            uuid        NULL,
    last_session_started_at timestamptz NULL,
    last_session_ended_at   timestamptz NULL,
    revoked_at              timestamptz NULL,
    changed_at              bigint      NOT NULL REFERENCES event (position),
    -- The display name lives in actor; this ties the worker row to its actor row.
    FOREIGN KEY (id, actor_kind) REFERENCES actor (id, kind),
    -- The reserved id and the worker id are the same value.
    FOREIGN KEY (id) REFERENCES enrolment_token (reserved_worker)
);
CREATE INDEX worker_changes ON worker (workspace_id, changed_at);
CREATE INDEX worker_open_sessions ON worker (id) WHERE open_session IS NOT NULL;
