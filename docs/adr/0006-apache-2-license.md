# ADR-0006: Apache-2.0 license across all code

## Status

Accepted (2026-04-26) — supersedes prior tentative AGPL stance noted in `mm-final-design.md` §16.19.

## Context

Open-source license choice has long-term commercial and ecosystem implications:

| License | Operator commercial use | Patent grant | Ecosystem alignment |
|---|---|---|---|
| **AGPL-3.0** | Allowed but with copyleft burden on hosted services (operator must publish their fork) | Yes | Some commercial operators avoid AGPL |
| **Apache-2.0** | Allowed without copyleft | Yes (explicit) | matches matrix-rust-sdk, much of Matrix ecosystem |
| **MIT** | Allowed without copyleft | No | Patent-litigation risk |
| **Dual MIT+Apache** | Same as Apache | Same | More complex |

Codebase started under AGPL-3.0-or-later. The original reasoning was to ensure operator forks contribute back. In practice this has discouraged commercial operator adoption — operators don't want copyleft obligations on a codebase they need to customize for their compliance needs.

The MM-the-project commercial play (per `mm-business-model.md`) is operator services + hosted SaaS, not protocol monopoly. We don't need copyleft to capture value.

## Decision

**License all MatrixMedia code under Apache-2.0.** This applies to:
- All Rust crates in the workspace (`crates/*`)
- All TypeScript packages (`web/packages/*`)
- All Dart, Swift, Kotlin SDKs
- All documentation in `docs/` (CC-BY-4.0 also acceptable for docs but Apache-2.0 covers them too)

The `LICENSE` file in repo root contains the full Apache-2.0 text. Workspace `Cargo.toml` declares `license = "Apache-2.0"`.

## Alternatives considered

- **Keep AGPL-3.0-or-later.** Rejected: operator-uptake friction outweighs copyleft benefit; matches `mm-legal-operating-plan.md` and `mm-architecture-decisions.md` independent recommendations.
- **Dual MIT/Apache (Rust convention).** Rejected: marginal benefit; Apache-2.0 alone is sufficient and simpler.
- **Mozilla Public License 2.0.** Rejected: weaker than AGPL on copyleft, weaker than Apache on patent grant — neither/nor.
- **BUSL (Business Source License).** Rejected: not OSI-approved; would conflict with Matrix Foundation alignment.

## Consequences

- Operators can fork and modify without copyleft obligations. Removes a major adoption blocker.
- Includes explicit patent grant (Apache-2.0 §3) — protects users + contributors from patent litigation.
- Aligns with matrix-rust-sdk and broader Matrix ecosystem licensing.
- Anyone (including competitors) can use the code for any purpose. We compete on execution + reference-impl quality + commercial services, not protocol IP.
- Existing AGPL contributors must consent to relicensing. As of this ADR there is one primary contributor (the founder); CLA going forward.
- Pre-existing fork(s) under AGPL retain AGPL terms — Apache-2.0 only applies to commits from this point forward (commit `5e1ecd2` and later).

## References

- `WorkingDirectory/docs/external/mm-legal-operating-plan.md` §3 (license recommendation)
- `WorkingDirectory/docs/external/mm-architecture-decisions.md` (initial ADR list, ADR-0006)
- `WorkingDirectory/docs/external/mm-final-design.md` §16.19 (resolved by this ADR)
- Apache License 2.0: https://www.apache.org/licenses/LICENSE-2.0
- Implementation commit: `5e1ecd2 chore(license): switch to Apache-2.0 (lock per ADR-0006)`
