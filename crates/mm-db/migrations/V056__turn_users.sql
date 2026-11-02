-- V056: per-call TURN users for dynamic coturn credentials.
--
-- Replaces the static `--user=user:pass` row-that-never-dies with one row per
-- active call/stream, deleted at end-of-call plus a 60s reaper sweep. coturn
-- reads this table directly via `--psql-userdb` (long-term mechanism):
-- `turnusers_lt(name, realm, hmackey)` is coturn's own schema from
-- `turndb/schema.sql`; `hmackey` is `md5(username:realm:password)`, so the
-- cleartext password exists only in flight (mm-core -> client) and never here.
--
-- `call_id` is the stream id (or native-call id later); `owner_user_id` lets
-- end-of-call revoke its rows without knowing usernames. `expires_at` is the
-- backstop the reaper sweeps — revocation at end-of-call is the fast path.
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's LAST
-- statement being a `CREATE TABLE IF NOT EXISTS` (see migration_registry_tests).
CREATE TABLE IF NOT EXISTS turn_users (
    call_id TEXT NOT NULL,
    username TEXT PRIMARY KEY,
    owner_user_id TEXT NOT NULL,
    hmackey TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_turn_users_call ON turn_users(call_id);
CREATE INDEX IF NOT EXISTS idx_turn_users_expires ON turn_users(expires_at);
CREATE TABLE IF NOT EXISTS mm_turn_users_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
