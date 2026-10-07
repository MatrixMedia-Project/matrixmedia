-- GPU provider tool (spec 2026-10-06 §8.1). Idempotent on purpose: the runner re-runs
-- migrations to heal a partially migrated database, so every statement is IF NOT EXISTS,
-- and enums are TEXT + CHECK (CREATE TYPE has no IF NOT EXISTS). Kinds/regions/roles/states
-- MUST match mm_fleet::providers_db constants. The adoption probe in run_pg_migrations keys
-- on mm_fleet_desired.created_by, which is created by the LAST statement here.

CREATE TABLE IF NOT EXISTS mm_fleet_providers (
    id               TEXT PRIMARY KEY,
    label            TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('scaleway','runpod','akamai','ovh','gcp')),
    enabled          BOOLEAN NOT NULL DEFAULT true,
    priority         INTEGER NOT NULL,
    endpoint_display TEXT NOT NULL,
    account_display  TEXT,
    image            TEXT NOT NULL,
    gpu_image        TEXT NOT NULL,
    transcode_image  TEXT,
    max_gpu_nodes    INTEGER NOT NULL DEFAULT 1 CHECK (max_gpu_nodes BETWEEN 0 AND 100),
    bench_state      TEXT NOT NULL DEFAULT 'not_required'
        CHECK (bench_state IN ('not_required','pending','passed','failed')),
    bench_note       TEXT,
    bench_by         TEXT,
    bench_at         TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at       TIMESTAMPTZ
);
-- One priority per live provider. Soft-deleted rows leave the index so the slot frees up.
CREATE UNIQUE INDEX IF NOT EXISTS mm_fleet_providers_priority
    ON mm_fleet_providers (priority) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS mm_fleet_provider_zones (
    provider_id TEXT NOT NULL REFERENCES mm_fleet_providers(id),
    zone        TEXT NOT NULL,
    region      TEXT NOT NULL CHECK (region IN ('eu','us','asia')),
    position    INTEGER NOT NULL,
    PRIMARY KEY (provider_id, zone)
);

CREATE TABLE IF NOT EXISTS mm_fleet_provider_sizes (
    provider_id TEXT NOT NULL,
    zone        TEXT NOT NULL,
    role        TEXT NOT NULL CHECK (role IN ('fanout','edge','transcode')),
    size        TEXT NOT NULL,
    PRIMARY KEY (provider_id, zone, role),
    FOREIGN KEY (provider_id, zone) REFERENCES mm_fleet_provider_zones(provider_id, zone) ON DELETE CASCADE
);

-- Write-only from mm-core's point of view: ciphertext sealed to the runner's key (HPKE,
-- RFC 9180). mm-core never decrypts and never returns these bytes.
CREATE TABLE IF NOT EXISTS mm_fleet_provider_credentials (
    provider_id TEXT PRIMARY KEY REFERENCES mm_fleet_providers(id),
    key_id      TEXT NOT NULL,
    enc         BYTEA NOT NULL,
    ciphertext  BYTEA NOT NULL,
    aad_version INTEGER NOT NULL DEFAULT 1,
    entered_by  TEXT NOT NULL,
    entered_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mm_fleet_provider_status (
    provider_id     TEXT PRIMARY KEY REFERENCES mm_fleet_providers(id),
    checked_at      TIMESTAMPTZ NOT NULL,
    state           TEXT NOT NULL
        CHECK (state IN ('ok','needs_you','waiting_for_token','endpoint_mismatch','unknown')),
    key_scope       TEXT,
    quota           JSONB NOT NULL DEFAULT '{}'::jsonb,
    stock           JSONB NOT NULL DEFAULT '{}'::jsonb,
    prices          JSONB NOT NULL DEFAULT '{}'::jsonb,
    balance_minor   BIGINT,
    last_error      TEXT,
    last_error_kind TEXT,
    last_error_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS mm_fleet_zone_cooldown (
    provider_id TEXT NOT NULL,
    zone        TEXT NOT NULL,
    until       TIMESTAMPTZ NOT NULL,
    reason      TEXT NOT NULL CHECK (reason IN ('capacity','quota')),
    PRIMARY KEY (provider_id, zone)
);

CREATE TABLE IF NOT EXISTS mm_fleet_requests (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL CHECK (kind IN ('test_connection','test_boot')),
    provider_id  TEXT NOT NULL REFERENCES mm_fleet_providers(id),
    zone         TEXT,
    role         TEXT CHECK (role IS NULL OR role IN ('fanout','edge','transcode')),
    reason       TEXT,
    requested_by TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    claimed_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ,
    state        TEXT NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued','running','done','failed','expired')),
    result       JSONB
);
CREATE INDEX IF NOT EXISTS mm_fleet_requests_queue
    ON mm_fleet_requests (requested_at) WHERE state = 'queued';

-- Singleton: the runner's heartbeat and public key. id is CHECKed to 1.
CREATE TABLE IF NOT EXISTS mm_fleet_control (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    runner_version    TEXT NOT NULL,
    public_key        BYTEA NOT NULL,
    key_fingerprint   TEXT NOT NULL,
    heartbeat_at      TIMESTAMPTZ NOT NULL,
    fleet_mode_seen   TEXT NOT NULL,
    settings_rev_seen BIGINT NOT NULL,
    detail            JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS mm_fleet_boot_tokens (
    mm_node_id TEXT PRIMARY KEY,
    token_hash BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS mm_fleet_ops_audit (
    id           BIGSERIAL PRIMARY KEY,
    at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor        TEXT NOT NULL,
    action       TEXT NOT NULL,
    target       TEXT NOT NULL,
    reason       TEXT,
    detail       JSONB NOT NULL DEFAULT '{}'::jsonb,
    settings_rev BIGINT
);
CREATE INDEX IF NOT EXISTS mm_fleet_ops_audit_target ON mm_fleet_ops_audit (target, id DESC);

ALTER TABLE mm_fleet_nodes
    ADD COLUMN IF NOT EXISTS created_backend TEXT
        CONSTRAINT nodes_created_backend_valid CHECK (created_backend IS NULL OR created_backend IN ('api','terraform'));
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS provider_zone TEXT;
ALTER TABLE mm_fleet_nodes
    ADD COLUMN IF NOT EXISTS purpose TEXT NOT NULL DEFAULT 'broadcast'
        CONSTRAINT nodes_purpose_valid CHECK (purpose IN ('broadcast','test_boot'));
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS created_by TEXT;
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS boot_report JSONB;
ALTER TABLE mm_fleet_nodes ADD COLUMN IF NOT EXISTS provider_ref TEXT;

ALTER TABLE mm_fleet_desired
    ADD COLUMN IF NOT EXISTS purpose TEXT NOT NULL DEFAULT 'broadcast'
        CONSTRAINT desired_purpose_valid CHECK (purpose IN ('broadcast','test_boot'));
ALTER TABLE mm_fleet_desired ADD COLUMN IF NOT EXISTS pinned_provider_id TEXT;
ALTER TABLE mm_fleet_desired ADD COLUMN IF NOT EXISTS pinned_zone TEXT;
-- LAST on purpose: the adoption probe keys on this column.
ALTER TABLE mm_fleet_desired ADD COLUMN IF NOT EXISTS created_by TEXT;
