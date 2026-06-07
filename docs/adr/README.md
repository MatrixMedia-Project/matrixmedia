# Architecture Decision Records

Short, immutable docs that capture **why** a design choice was made. Per `mm-architecture-decisions.md` in the WorkingDirectory research tree.

## Format

`NNNN-kebab-title.md`, numbered sequentially, never deleted. To overturn an ADR, write a new ADR that supersedes it (mark the old one Superseded).

Each ADR has six sections:

```
# ADR-NNNN: Title

## Status
Accepted | Proposed | Superseded by ADR-XXXX | Deprecated

## Context
What problem are we solving? What constraints apply?

## Decision
What did we choose to do?

## Alternatives considered
What did we reject, and why?

## Consequences
What follows from this — both wins and trade-offs?

## References
Links to research docs, prior art, related ADRs.
```

## Index

| # | Title | Status |
|---|---|---|
| 0001 | [Three-surface architecture](0001-three-surface-architecture.md) | Accepted |
| 0002 | [BYO wallet via NWC as default custody](0002-byo-wallet-nwc-default.md) | Accepted |
| 0003 | [Lightning Network as cross-instance settlement rail](0003-lightning-cross-instance-settlement.md) | Accepted |
| 0004 | [Operator manifest at .well-known/matrixmedia/operator.json](0004-operator-manifest-location.md) | Accepted |
| 0005 | [Monorepo for mm-* crates](0005-monorepo.md) | Accepted |
| 0006 | [Apache-2.0 license across all code](0006-apache-2-license.md) | Accepted |
| 0007 | [Stripe + LNBits as primary M1 payment providers](0007-m1-payment-providers.md) | Accepted |
| 0008 | [Two-entity model — Foundation + Commercial Ltd](0008-two-entity-model.md) | Accepted |
| 0009 | [mm-core source of truth for stream tiles; host stream resume](0009-stream-timeline-source-of-truth.md) | Accepted |
| 0010 | [Web SDK packaging — three npm packages](0010-web-sdk-packaging.md) | Accepted |
