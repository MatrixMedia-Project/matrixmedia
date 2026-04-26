# ADR-0003: Lightning Network as cross-instance settlement rail

## Status

Accepted (2026-04-26)

## Context

A federated streaming platform must let Alice on `server-a.com` tip a creator on `server-b.com` without:
- A central payment processor (kills federation pitch + makes us a regulated MSB)
- Bilateral agreements between every pair of operators (doesn't scale)
- Trust between operators (federation is open and operators are heterogeneous)

The only existing peer-to-peer infrastructure that solves this is the **public Lightning Network** — operators don't know each other, but their Lightning nodes can settle through the public network with HTLC-atomic guarantees.

## Decision

**Lightning Network is the canonical settlement rail for cross-instance value transfer.** When a user on server-A tips a user on server-B:
1. server-A asks server-B (via Matrix federation event) for a Lightning invoice
2. server-B's mm-core asks the recipient's wallet (via NWC or LNBits or LNURL) to generate an invoice
3. server-A returns the invoice to the sender's wallet (via NWC)
4. Sender's wallet pays the invoice over the public Lightning Network
5. Preimage proof is posted as a Matrix event in the room

No bilateral operator agreements. No third party between operators. HTLC atomicity guarantees either-the-tip-lands-or-it-reverses.

## Alternatives considered

- **IOU ledger between operators.** Rejected: requires bilateral trust + periodic settlement which itself needs a rail.
- **Stripe Connect chained transfers.** Rejected: forces every operator to be a Stripe Connect platform with the regulatory burden that implies.
- **Stablecoin (USDC) on Solana / Base.** Rejected: smart-contract risk; ecosystem mismatch with Matrix; regulatory uncertainty post-MiCA. May reconsider for M6 Asia/Africa adapters.
- **Fedimint / Cashu eCash bearer tokens.** Considered for future. Adds another protocol layer; out of scope for M1.

## Consequences

- Cross-server tipping is a first-class feature, not a degraded fallback.
- Operators must run a Lightning node (via LNBits / Voltage / self-host). Initial pilot uses Voltage-hosted to reduce ops burden.
- Lightning routing fees apply (~0-0.1%); negligible compared to Stripe's 2.9% + $0.30.
- Settlement is in sats; operator UX must show fiat equivalents using a price oracle (M3 work; pinned 1500 sats/USD for pilot).
- Cryptographic auditability via preimage proofs in receipt events — users can verify settlement without trusting either operator.

## References

- `WorkingDirectory/docs/external/mm-final-design.md` §4 (payment rails matrix)
- `WorkingDirectory/docs/external/MSC-XXXX-tip-events-draft.md` (protocol spec)
- BOLT11 spec: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
