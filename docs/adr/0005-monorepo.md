# ADR-0005: Monorepo for mm-* crates

## Status

Accepted (2026-04-26)

## Context

The MatrixMedia codebase comprises ~10 Rust crates (`mm-core`, `mm-api`, `mm-db`, `mm-payment`, `mm-matrix`, `mm-sfu`, `mm-recommendations`, `mm-ads`, `mm-server`, `mm-fakestripe`), Go (`mm-switch`), TypeScript (`mm-dashboard`, `mm-widget`), Dart (Flutter SDK), Swift (iOS SDK), and Kotlin (Android SDK).

Two repo organization choices exist:
1. **Monorepo** (current): everything in `~/Documents/MatrixMedia/matrixmedia/`. Single git history, single CI/CD, atomic cross-crate refactors, single PR review.
2. **Polyrepo**: each crate / language / region in its own repo. Independent release cycles, finer-grained access control, easier external contribution.

## Decision

**Single monorepo for all mm-* code through Phase M6.** All crates, web packages, SDKs, and infra code live in `~/Documents/MatrixMedia/matrixmedia/`. CI runs against the monorepo. Releases are tagged at the workspace level.

**Native client apps are exceptions** — they live in separate repos (`MatrixMedia-iOS`, `MatrixMedia-Android` per memory) because they have separate publishing accounts, separate App Store identities, and separate release cadences.

**Triggers for splitting (not to be done preemptively):**
- A community-maintained adapter (e.g. `mm-payment-bitnob`) that needs independent release cycle
- A separate documentation site (e.g. `matrixmedia-docs.matrixmedia.org`) that has its own deploy pipeline
- A community-developed alternative client that wants its own repo identity

## Alternatives considered

- **Polyrepo from the start.** Rejected: premature for a project with one team. Adds CI/release complexity and slows cross-crate refactors.
- **Monorepo even for native apps.** Rejected: native apps already live in separate repos with their own CI; merging them in adds friction without value.

## Consequences

- Atomic refactors across `mm-core` ↔ `mm-api` ↔ `mm-db` are straightforward.
- CI runs all tests on every PR — slower than per-crate CI, but catches integration issues earlier.
- External contributors must clone the whole repo (~50MB+ of dependencies). Mitigated by clear module boundaries.
- Workspace `Cargo.toml` dependency declarations apply to all crates — version bumps are workspace-wide.
- When an adapter needs to be split off (per the triggers above), publish it as its own crate with `mm-payment-onramp` trait dep on `mm-payment`.

## References

- `WorkingDirectory/docs/external/mm-architecture-decisions.md` §6
- Workspace config: `Cargo.toml` (root)
