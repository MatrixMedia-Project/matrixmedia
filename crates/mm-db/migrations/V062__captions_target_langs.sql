-- V062: broadcaster default caption translation targets.
--
-- The dashboard captions card has always sent/read `default_target_langs`
-- (BCP-47 tags produced for every captioned video), but no migration ever
-- persisted them, so GET /creator/me/captions could not return the field
-- and the card crashed on `.join(...)`. Store them on the existing
-- defaults row as a text array; empty means "spoken language only".
--
-- Re-runnable like every migration here: IF NOT EXISTS throughout.
--
-- LAST on purpose: mm-db's adoption probe keys on the newest migration's
-- LAST statement being a `CREATE TABLE IF NOT EXISTS` (see
-- migration_registry_tests).
ALTER TABLE mm_creator_defaults
    ADD COLUMN IF NOT EXISTS captions_target_langs TEXT[] NOT NULL DEFAULT '{}';

CREATE TABLE IF NOT EXISTS mm_captions_target_langs_migrated (
    id BOOLEAN PRIMARY KEY DEFAULT true,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
