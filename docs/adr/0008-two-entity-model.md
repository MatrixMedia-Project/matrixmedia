# ADR-0008: Two-entity model — Foundation + Commercial Ltd

## Status

Accepted (2026-04-26)

## Context

MatrixMedia must function both as a long-lived open-source protocol (community-credible, no commercial bias in the spec) AND as a sustainable commercial venture (someone needs to pay engineers + ship operator services). These roles have conflicting incentives if conflated in a single legal entity.

Matrix.org's own playbook splits this: Matrix.org Foundation owns the protocol; Element Ltd is the commercial vehicle. This has worked for ~10 years.

## Decision

**Two legal entities:**

1. **MatrixMedia Foundation** (Estonia MTÜ — non-profit). Owns:
   - The Matrix Spec Change (MSC) drafts we publish upstream
   - The trademark "MatrixMedia" / "MM"
   - Reference protocol implementation (this monorepo, Apache-2.0)
   - Operates a community grant program (modest scale)
   - Funded by foundation grants (OpenSats, NLnet, Spiral, etc.) + corporate membership

2. **MatrixMedia Commercial Ltd** (Delaware C-corp). Sells:
   - **MM Cloud** — hosted mm-core + mm-switch + LNBits SaaS
   - **MM Compliance Pack** — KYC/AML/OFAC tooling for licensed operators
   - **MM Enterprise** — SSO, audit logs, SLAs, white-label
   - Premium support contracts

The Foundation licenses the trademark to Commercial Ltd under terms that preserve community use.

## Alternatives considered

- **Single for-profit entity.** Rejected: Matrix community treats commercial-only protocol shepherds with suspicion (justifiably). Hurts adoption of MSCs upstream.
- **Single non-profit / foundation.** Rejected: hard to attract VC, restricts equity compensation, slow product iteration. Element learned this lesson.
- **Foundation in Switzerland (Stiftung).** Rejected after analysis (`mm-fundraising-team-plan.md` §7): CHF 75-100K Y1 + CHF 20-35K ongoing vs Estonia MTÜ's much lower setup. Reconsider re-domicile to Switzerland in Y4+ if scale justifies.
- **Commercial entity in EU (UK Ltd or Estonia OÜ).** Rejected: meaningful US fundraising requires Delaware C-corp. EU founders use EOR services for tax. (See `mm-fundraising-team-plan.md` §8.)

## Consequences

- Operating two entities adds annual overhead (~$15-25K/yr for the Foundation + ~$10K/yr Delaware franchise tax + accounting). Acceptable.
- Foundation governance must include independent board members (not just the founder). Bylaws TBD.
- Conflict-of-interest policy required where Foundation grants any contract to Commercial Ltd (must be at arm's length).
- Foundation owns MSC IP — protects the protocol from being captured by any commercial entity (including ours).
- Commercial Ltd has employee equity pool (~18%) and standard preferred-stock structure for fundraising.
- Delaware C-corp incurs OFAC obligations (must screen sanctioned countries). Acceptable; EU operators can use the protocol without touching Commercial Ltd.

## References

- `WorkingDirectory/docs/external/mm-fundraising-team-plan.md` §7-8
- `WorkingDirectory/docs/external/mm-legal-operating-plan.md`
- `WorkingDirectory/docs/external/mm-final-design.md` §9
- Matrix Foundation precedent: https://matrix.org/foundation/
