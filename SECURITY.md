# Security Policy

## Reporting a Vulnerability

**Please do not file public GitHub issues for security vulnerabilities.**

Email security reports to **`security@matrixmedia.org`** (PGP-encrypted preferred — key fingerprint TBD pre-launch). Include:

- Affected component (mm-core / mm-payment / mm-switch / web client / etc.)
- Affected version (commit hash from `git log`)
- Reproduction steps
- Impact assessment in your view

We respond within **72 hours** to acknowledge receipt and within **7 days** with a triage decision.

## Disclosure Timeline

- **Critical** (active exploit, fund loss, identity compromise): coordinated disclosure within **30 days**, faster if a working patch exists.
- **High** (potential fund loss, privilege escalation): **60 days**.
- **Medium / Low**: **90 days** standard coordinated disclosure.

We commit to crediting reporters in the public advisory (unless the reporter requests anonymity).

## Scope

In-scope:
- `crates/mm-core`, `crates/mm-api`, `crates/mm-payment`, `crates/mm-db`, `crates/mm-server`
- `services/mm-switch` (Go SFU)
- Web clients in `web/packages/`
- Federation event handling
- Cryptographic integrity of payment flows (BOLT11 invoice handling, webhook signature verification, NWC pairing)

Out-of-scope:
- Social engineering of MM contributors or operators
- Denial-of-service against our reference deployment (steegler.com) — operate against your own deployment for testing
- Vulnerabilities in third-party dependencies that we have already patched (use the latest tagged release)
- Vulnerabilities specific to operator misconfigurations (these are operator concerns, not code concerns)

## Bug Bounty

A bug bounty program is planned post-pilot launch. Per `WorkingDirectory/docs/external/mm-security-tech-plan.md` §8, planned tiers (subject to change at launch):

| Severity | Pilot reward | Post-M3 reward |
|---|---|---|
| Low | $50 | $100 |
| Medium | $250 | $500 |
| High | $750 | $2,000 |
| Critical | $2,000 | $10,000 |

The `security@matrixmedia.org` channel is open today; bug bounty enrollment + safe-harbor language follow at launch.

## Supply Chain

We track upstream Rust + npm advisories via:
- `cargo audit` in CI (blocks on advisory-database matches)
- Renovate / Dependabot for version updates
- Manual review for major version bumps in payment-path crates

If you find a vulnerability in one of our dependencies that we haven't yet patched, please report it both to the upstream maintainer AND to us so we can coordinate.

## Security Architecture

For technical detail on our threat model, federation auth design, webhook signature handling, and key rotation procedures, see:
- `WorkingDirectory/docs/external/mm-security-program.md` (strategic security position)
- `WorkingDirectory/docs/external/mm-security-tech-plan.md` (technical security plan)
- ADR-0002 (BYO wallet — minimizes custody attack surface)
- ADR-0003 (Lightning settlement — cryptographic preimage proofs)

## Coordinated Disclosure

We follow the [CERT/CC Vulnerability Disclosure Policy](https://www.kb.cert.org/vuls/guidance/) as a baseline. Researchers acting in good faith are protected by safe-harbor commitments to the extent legally permissible — we will not pursue legal action against testing that complies with this policy.
