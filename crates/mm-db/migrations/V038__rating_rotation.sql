-- V038: stop one wallet's unrateable usage from blocking everyone else's (WS-D).
--
-- The rater takes the oldest N unrated events. Events it cannot rate — the wallet
-- cannot cover them, or they cannot be priced — stay unrated on purpose (forgiving
-- them would forgive real debt). But they also stay OLDEST, so once N of them pile
-- up they fill every batch, and nobody else is ever billed again. Found in review
-- 2026-09-25: after five passes a funded broadcaster's usage was still unbilled
-- behind two unaffordable events in a batch of two.
--
-- Being unable to pay is a property of the WALLET, not of one event, so that is
-- where the mark goes. The rater orders the queue `rating_blocked_at NULLS FIRST`:
-- wallets that can be billed come first, blocked wallets after, the longest-blocked
-- first — and each failed attempt re-stamps it, so blocked wallets rotate rather
-- than the same one always leading.
--
-- ADD COLUMN IF NOT EXISTS rather than an edit to V035, so any database that already
-- ran V035 picks it up.

ALTER TABLE mm_broadcaster_wallet
    ADD COLUMN IF NOT EXISTS rating_blocked_at TIMESTAMPTZ;
