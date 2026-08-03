-- mm-dns sqlite claims store schema (Task 4).
--
-- Applied idempotently by `Store::open` on every startup (see
-- `src/store.rs`) -- no external migration runner. `IF NOT EXISTS` guards
-- are added around the brief's literal schema so re-opening an
-- already-migrated database file (the normal case for a persistent
-- `MM_DNS_DB_PATH`) is a no-op rather than an error.

CREATE TABLE IF NOT EXISTS claims (
  name TEXT PRIMARY KEY,
  ip TEXT NOT NULL,
  claim_token_hash TEXT NOT NULL,
  httpreq_user TEXT NOT NULL UNIQUE,
  httpreq_pass_hash TEXT NOT NULL,
  record_ids TEXT NOT NULL,          -- JSON array of CF record ids (the 3 A records)
  created_at INTEGER NOT NULL,       -- unix secs (passed in, not Date::now -- keep testable)
  released_at INTEGER
);

CREATE TABLE IF NOT EXISTS rate_events ( ip TEXT NOT NULL, at INTEGER NOT NULL );

CREATE INDEX IF NOT EXISTS rate_events_ip_at ON rate_events(ip, at);
