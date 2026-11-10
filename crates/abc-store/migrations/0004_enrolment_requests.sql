-- Enrolment requests (I3): a machine asks, the owner approves or denies in the browser. Same view
-- rules as 0002. A request's status (pending / approved / denied / expired) is derived when read
-- from `decision` and `expires_at`, so it needs no sweeper.

ALTER TABLE event DROP CONSTRAINT event_stream_kind_check;
ALTER TABLE event ADD CONSTRAINT event_stream_kind_check CHECK (stream_kind IN
    ('installation', 'person', 'workspace', 'enrolment_token', 'enrolment_request', 'worker'));

CREATE TABLE enrolment_request (
    id              uuid        PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspace (id),
    -- The worker id approval creates, reserved when asked.
    reserved_worker uuid        NOT NULL UNIQUE,
    -- SHA-256 of the key the machine generated. A key names one request.
    key_digest      bytea       NOT NULL UNIQUE CHECK (octet_length(key_digest) = 32),
    -- Who asked: the person (an actor), their tailnet login and the node, as Serve and abc's
    -- tailscaled reported them.
    requested_by    uuid        NOT NULL REFERENCES actor (id),
    login           text        NOT NULL CHECK (char_length(login) BETWEEN 1 AND 254),
    node            text        NOT NULL CHECK (char_length(node) BETWEEN 1 AND 253),
    hostname        text        NOT NULL CHECK (char_length(hostname) BETWEEN 1 AND 64),
    os              text        NOT NULL CHECK (os IN ('macos', 'linux')),
    arch            text        NOT NULL CHECK (arch IN ('aarch64', 'x86_64')),
    build           text        NOT NULL,
    code            text        NOT NULL CHECK (code ~ '^[0-9A-HJKMNP-TV-Z]{8}$'),
    requested_at    timestamptz NOT NULL,
    requested_event uuid        NOT NULL REFERENCES event (id),
    expires_at      timestamptz NOT NULL CHECK (expires_at > requested_at),
    decision        text        NULL CHECK (decision IN ('approved', 'denied')),
    decided_by      uuid        NULL REFERENCES actor (id),
    decided_at      timestamptz NULL,
    changed_at      bigint      NOT NULL REFERENCES event (position),
    CHECK ((decision IS NULL) = (decided_at IS NULL)),
    CHECK ((decision IS NULL) = (decided_by IS NULL))
);
CREATE INDEX enrolment_request_changes ON enrolment_request (workspace_id, changed_at);

-- A worker enrols through a token or through a request, exactly one.
ALTER TABLE worker DROP CONSTRAINT worker_id_fkey;
ALTER TABLE worker DROP CONSTRAINT worker_enrolled_via_fkey;
ALTER TABLE worker RENAME COLUMN enrolled_via TO enrolled_via_token;
ALTER TABLE worker ALTER COLUMN enrolled_via_token DROP NOT NULL;
ALTER TABLE worker ADD CONSTRAINT worker_enrolled_via_token_fkey
    FOREIGN KEY (enrolled_via_token) REFERENCES enrolment_token (id);
ALTER TABLE worker ADD COLUMN enrolled_via_request uuid NULL UNIQUE
    REFERENCES enrolment_request (id);
ALTER TABLE worker ADD CONSTRAINT worker_enrolled_once
    CHECK ((enrolled_via_token IS NULL) <> (enrolled_via_request IS NULL));
