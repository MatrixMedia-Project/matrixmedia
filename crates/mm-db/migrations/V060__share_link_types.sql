-- V060: share-link types — the 4 grant types + single-use consumption.
--
-- `share_type` on `mm_share_grants` classifies every issued share link
-- on two axes:
--   * connections: `SHARE_FOR_ALL_*` links admit unlimited viewers;
--     `SINGLE_SHARE_*` links admit exactly one connection and expire the
--     moment that viewer connects (see `consumed_at` below).
--   * payment: `*_FREE` links never ask the viewer for sats (a free
--     link to a priced target is a comp pass — the grant type, not the
--     target's price, decides for this link); `*_PAID` links require
--     the viewer to settle the target's Lightning paywall price
--     (`paywall_price_sats`, V059) before connecting.
--
-- `consumed_at` is stamped by the atomic single-use consume (an
-- UPDATE ... WHERE consumed_at IS NULL — exactly one concurrent
-- connect wins). A grant is live while unrevoked, unexpired AND
-- unconsumed; consumed rows are kept until `expires_at` (then reaped
-- like any expired grant) so a used single link stays distinguishable
-- from a never-issued one. Multi-use grants never get a consumed_at.
--
-- Backfill: pre-V060 grants were all multi-use, and paid exactly when
-- their target carried a paywall price, so priced-target grants are
-- flipped to `SHARE_FOR_ALL_PAID` and the rest keep the column
-- default `SHARE_FOR_ALL_FREE` — no existing link changes behavior.
-- The guarded UPDATEs run once (the apply-once runner records V060 in
-- mm_schema_migrations); they must not re-fire later, when a FREE
-- grant on a priced target is a deliberate comp pass.
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest
-- migration's LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests). V060 creates the
-- `mm_share_link_types_migrated` marker table in its LAST statement,
-- so a V060 that died partway — marker table not yet created — is
-- not adopted.
ALTER TABLE mm_share_grants
    ADD COLUMN IF NOT EXISTS share_type TEXT NOT NULL DEFAULT 'SHARE_FOR_ALL_FREE'
        CONSTRAINT share_grants_share_type_valid
        CHECK (share_type IN (
            'SHARE_FOR_ALL_FREE', 'SHARE_FOR_ALL_PAID',
            'SINGLE_SHARE_FREE', 'SINGLE_SHARE_PAID'));

ALTER TABLE mm_share_grants
    ADD COLUMN IF NOT EXISTS consumed_at TIMESTAMPTZ;

UPDATE mm_share_grants g SET share_type = 'SHARE_FOR_ALL_PAID'
 WHERE g.kind = 'stream' AND g.share_type = 'SHARE_FOR_ALL_FREE'
   AND EXISTS (SELECT 1 FROM mm_streams s
                WHERE s.id = g.target_id AND s.paywall_price_sats > 0);

UPDATE mm_share_grants g SET share_type = 'SHARE_FOR_ALL_PAID'
 WHERE g.kind = 'recording' AND g.share_type = 'SHARE_FOR_ALL_FREE'
   AND EXISTS (SELECT 1 FROM mm_recordings r
                WHERE r.id = g.target_id AND r.paywall_price_sats > 0);

CREATE TABLE IF NOT EXISTS mm_share_link_types_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
