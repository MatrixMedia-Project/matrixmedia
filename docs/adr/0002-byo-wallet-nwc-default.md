# ADR-0002: BYO wallet via NWC as default custody

## Status

Accepted (2026-04-26)

## Context

EU MiCA (effective Dec 30, 2024) classifies operators that hold user crypto funds as Crypto-Asset Service Providers (CASPs), requiring authorization that costs ~€350K/year first-year (€125K capital + €100K supervisory + €100K compliance program). This is unviable below ~10K paying users. Wallet of Satoshi exited the EU in early 2026 because they couldn't justify these costs.

MiCA explicitly exempts non-custodial wallet providers. Nostr Wallet Connect (NIP-47) lets an app request payments from a user-controlled wallet without ever holding funds.

Apple App Store rules (3.1.5(b)) explicitly permit P2P crypto transfers with no in-app benefit unlock. Custodial Lightning in iOS apps is a grey area.

## Decision

**BYO wallet via Nostr Wallet Connect (NIP-47) is the default custody model for all MatrixMedia clients.** The user pairs their existing Lightning wallet (Phoenix, Mutiny, Wallet of Satoshi self-custody mode, Breez, Alby, Zeus) once via QR scan. From then on, MatrixMedia requests payment confirmations and the wallet handles authorization.

Custodial mode is opt-in for operators with proper licensing (CASP / MSB / VASP / equivalent). mm-core hard-fails to start in custodial mode unless the operator declares `MM_CASP_LICENSE_NUMBER` at startup.

## Alternatives considered

- **Custodial-default with LNBits everywhere.** Rejected: forces every operator into MiCA CASP territory (or equivalent in other jurisdictions). Kills the federation pitch.
- **Hybrid default.** Rejected: confusing UX; operators that want custody can opt in via the gate.
- **Lightning-Address-only (no NWC).** Rejected: works for receiving, not for sending. NWC is the Lightning ecosystem standard.

## Consequences

- Onboarding has one friction step: user must have or install a Lightning wallet. Mitigated by clear setup docs and operator-recommended wallets per region.
- Operators bear no regulatory exposure for non-custodial flows. Major win for hobbyist + small commercial operators.
- We don't control the wallet ecosystem; if NWC adoption stalls, our flow degrades. Mitigated by raw BOLT11 QR fallback always available.
- Cross-server tipping works because settlement is on the public Lightning Network — operators don't need bilateral agreements.

## References

- `WorkingDirectory/docs/external/mica-mm-implications.md` (full legal analysis)
- `WorkingDirectory/docs/external/mm-final-design.md` §3
- NIP-47 spec: https://github.com/nostr-protocol/nips/blob/master/47.md
