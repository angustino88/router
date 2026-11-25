-- Router schema. Applied at every start-up, so every statement is idempotent.

-- Registry: one row per department backend.
CREATE TABLE IF NOT EXISTS backend (
    id            bigint       GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Public path prefix: requests to /t/<prefix_key>/... are proxied to upstream_url. Generated, immutable.
    prefix_key    varchar(100) NOT NULL UNIQUE CHECK (prefix_key ~ '^[A-Za-z0-9_-]+$'),
    name          varchar(255) NOT NULL CHECK (length(btrim(name)) > 0),
    -- Logical id the backend reports in GET /internal/info (its BACKEND_ID). Empty = not verified.
    backend_id    varchar(100) NOT NULL DEFAULT '',
    -- scheme://host[:port] nginx proxies to. Empty = not routed.
    upstream_url  varchar(255) NOT NULL DEFAULT '',
    is_active     boolean      NOT NULL DEFAULT true,
    created_at    timestamptz  NOT NULL DEFAULT now(),
    updated_at    timestamptz  NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS backend_backend_id_key ON backend (backend_id) WHERE backend_id <> '';

-- Live claims synced from each backend's streams. A tombstone deletes the row.
-- users: key = entra:<tenant uuid>:<object uuid>; agents: key = agent uuid (both lowercase).
CREATE TABLE IF NOT EXISTS user_claim (
    backend_pk  bigint      NOT NULL REFERENCES backend (id) ON DELETE CASCADE,
    key         text        NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (backend_pk, key)
);
CREATE INDEX IF NOT EXISTS user_claim_key_idx ON user_claim (key);

CREATE TABLE IF NOT EXISTS agent_claim (
    backend_pk  bigint      NOT NULL REFERENCES backend (id) ON DELETE CASCADE,
    key         text        NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (backend_pk, key)
);
CREATE INDEX IF NOT EXISTS agent_claim_key_idx ON agent_claim (key);

-- Last applied position of each (backend, stream).
CREATE TABLE IF NOT EXISTS sync_cursor (
    backend_pk  bigint      NOT NULL REFERENCES backend (id) ON DELETE CASCADE,
    entity      text        NOT NULL CHECK (entity IN ('users', 'agents')),
    epoch       text        NOT NULL,
    version     bigint      NOT NULL CHECK (version >= 0),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (backend_pk, entity)
);

-- Background jobs: schedule (edited on the admin page) and run history.
CREATE TABLE IF NOT EXISTS job_schedule (
    name           text        PRIMARY KEY,
    enabled        boolean     NOT NULL DEFAULT true,
    interval_secs  integer     NOT NULL CHECK (interval_secs > 0),
    last_run_at    timestamptz,
    updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS job_run (
    id           bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job          text        NOT NULL,
    trigger      text        NOT NULL CHECK (trigger IN ('schedule', 'manual', 'admin', 'startup')),
    status       text        NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz,
    result       jsonb,
    error        text
);
CREATE INDEX IF NOT EXISTS job_run_job_id_idx ON job_run (job, id DESC);
