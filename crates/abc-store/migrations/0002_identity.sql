-- Views: actors, workspaces, membership, the setup code and the installation claim.
--
-- Rules for every view table (asserted by assert_views_replay):
-- * written only by abc-store/src/projection.rs, from events;
-- * every value comes from an event (no now(), no random(), no sequences), so replay is exact;
-- * `changed_at` is the position of the last event that touched the row, so "changed since N" is
--   an index range scan on the view itself;
-- * foreign keys go view -> view or view -> log, never log -> view.

-- Every actor (I4), whatever its kind: one place to resolve "who did this" for any event.
CREATE TABLE actor (
    id           uuid        PRIMARY KEY,
    kind         text        NOT NULL CHECK (kind IN ('person', 'worker')),
    display_name text        NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 64),
    created_at   timestamptz NOT NULL,
    changed_at   bigint      NOT NULL REFERENCES event (position),
    UNIQUE (id, kind)
);

-- How a person is recognised: the Tailscale login the sidecar asserts. One login per person in v1;
-- the UNIQUE on person_id goes when a person may link several identities.
CREATE TABLE tailnet_identity (
    login       text PRIMARY KEY CHECK (char_length(login) BETWEEN 1 AND 254),
    person_id   uuid NOT NULL UNIQUE,
    person_kind text NOT NULL DEFAULT 'person' CHECK (person_kind = 'person'),
    FOREIGN KEY (person_id, person_kind) REFERENCES actor (id, kind)
);

CREATE TABLE workspace (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
    created_at timestamptz NOT NULL,
    changed_at bigint      NOT NULL REFERENCES event (position)
);

-- Built for teams underneath (decision 20): membership is per (workspace, person) from day one.
CREATE TABLE membership (
    workspace_id uuid        NOT NULL REFERENCES workspace (id),
    person_id    uuid        NOT NULL,
    person_kind  text        NOT NULL DEFAULT 'person' CHECK (person_kind = 'person'),
    role         text        NOT NULL CHECK (role IN ('owner')),
    since        timestamptz NOT NULL,
    changed_at   bigint      NOT NULL REFERENCES event (position),
    PRIMARY KEY (workspace_id, person_id),
    FOREIGN KEY (person_id, person_kind) REFERENCES actor (id, kind)
);
CREATE INDEX membership_by_person ON membership (person_id);

-- A row exists if and only if the server has been claimed. At most one row, ever.
CREATE TABLE installation (
    singleton    boolean     PRIMARY KEY DEFAULT true CHECK (singleton),
    owner_id     uuid        NOT NULL REFERENCES actor (id),
    workspace_id uuid        NOT NULL REFERENCES workspace (id),
    claimed_at   timestamptz NOT NULL,
    changed_at   bigint      NOT NULL REFERENCES event (position)
);

-- The newest owner setup code, while the server is unclaimed. One row at most: a newer code
-- replaces it (the projection upserts). The digest only; the plaintext went to the operator's
-- terminal. The reserved ids are the ones the claim creates.
CREATE TABLE setup_code (
    singleton    boolean     PRIMARY KEY DEFAULT true CHECK (singleton),
    digest       bytea       NOT NULL CHECK (octet_length(digest) = 32),
    owner_id     uuid        NOT NULL,
    workspace_id uuid        NOT NULL,
    expires_at   timestamptz NOT NULL,
    changed_at   bigint      NOT NULL REFERENCES event (position)
);
