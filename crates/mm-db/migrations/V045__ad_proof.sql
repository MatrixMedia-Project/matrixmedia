-- Ad completion proof storage (audit P0 #A1, 2026-10-18). Idempotent on purpose:
-- the runner re-runs migrations to heal a partially migrated database, so every
-- statement is IF NOT EXISTS.
--
-- The HMAC challenge (nonce + viewer_secret) issued with each ad decision is now
-- persisted alongside the impression, so POST /streams/{id}/ad-complete can verify
-- the client's challenge_response instead of accepting any caller-held token.
-- viewer_secret is a random per-impression HMAC key, not a user credential; it lives
-- in Postgres under the same trust as the rest of the impression row.
-- Rows decided before V045 keep NULL challenge columns (legacy accept-once path).
-- LAST on purpose: the adoption probe in run_pg_migrations keys on challenge_nonce.
ALTER TABLE mm_ad_impressions ADD COLUMN IF NOT EXISTS challenge_nonce TEXT;
ALTER TABLE mm_ad_impressions ADD COLUMN IF NOT EXISTS viewer_secret TEXT;
