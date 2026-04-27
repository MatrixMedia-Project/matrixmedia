# ADR-0007: Stripe + LNURL-pay (with LNBits as opt-in fallback) for M1

## Status

Accepted (2026-04-26). **Amended (2026-04-26)** — original decision (Stripe + LNBits as the two primaries) was superseded by the true-P2P pivot documented in `WorkingDirectory/docs/external/mm-demo-path-pivot.md`. LNURL-pay is now the canonical Lightning rail for M1; LNBits remains in-tree as an opt-in custodial fallback for operators that want to offer wallet-for-hire.

## Context

The M1 pilot ships a multi-provider donation flow. We must pick the provider implementations to ship on day 1.

Provider candidates evaluated:

| Provider | Type | Custody | Operator capital | M1 fit |
|---|---|---|---|---|
| **Stripe** (async-stripe) | Fiat processor | Operator-mediated escrow | $0 (just keys) | ✅ Primary |
| **LNURL-pay** (LUD-06 + LUD-16) | Lightning, **non-custodial** | None — wallet-to-wallet | $0, no LN node | ✅ Primary |
| **LNBits** (HTTP API) | Lightning, **custodial** | Operator holds creator funds | $700+ channel liquidity | ⚠️ Opt-in fallback |
| **Strike API** | Lightning + fiat on-ramp | Custodial | Medium | ❌ M3+ |
| **MoonPay** | Fiat-to-crypto on-ramp | None (passthrough) | Medium | ❌ M3+ |
| **Bitnob** | African fiat-to-Lightning | Custodial | Medium | ❌ M6 |
| **PayPal** | Fiat | Operator-mediated | High (compliance) | ❌ Not in scope |

The architectural constraint that forced the amendment: **operator never custodies funds** (per `mm-final-design.md` §3 + `mm-demo-path-pivot.md`). Holding funds on behalf of creators triggers MiCA CASP licensing in the EU once the operator passes scale thresholds; for a federated open-source project that's a non-starter. Routing through the recipient's published Lightning Address keeps the operator out of the path entirely.

## Decision

**M1 ships with three payment-routing options:**

1. **Stripe** — recurring subscriptions + one-off donations via Stripe Checkout, Connect Express for creator onboarding. Provider lives in `crates/mm-payment/src/stripe/`. **Primary fiat rail.**
2. **LNURL-pay (LUD-06 + LUD-16)** — operator resolves the creator's Lightning Address (`name@domain.tld`) into a fresh BOLT11 invoice on every tip; donor's wallet pays it directly via NWC, scan, or copy/paste. Operator runs no Lightning node. Lives in `crates/mm-payment/src/lnurl/`. **Primary Lightning rail.**
3. **LNBits (opt-in)** — operator-custodial Lightning, kept in-tree for operators that want to offer hosted wallets for creators that don't want to manage their own. Lives in `crates/mm-payment/src/lnbits/`. **Opt-in fallback only — disabled by default.**

The donation handler (`crates/mm-api/src/monetization.rs::create_donation`) routes Lightning requests as:

```
provider == "lightning" + creator.lightning_address set  →  LNURL-pay (P2P, no custody)
provider == "lightning" + no lightning_address           →  LNBits (if operator opted in)
provider == "lightning" + neither configured             →  400 Lightning unavailable
provider == "stripe"                                     →  Stripe Checkout via registry
```

The `PaymentProviderRegistry` in `mm-payment/src/registry.rs` still allows additional providers (M6 adapters: Bitnob, Tando, Strike Asia, Pouch, MoonPay) without touching handler code — but new fiat-on-ramp / fiat-only providers, not Lightning custody.

For the pilot, the BTC↔USD rate is hardcoded at 1500 sats/USD in `mm-payment/src/lnbits/types.rs`. A live oracle (Coingecko or Kraken) is M3 work.

## Alternatives considered

- **Lightning-only at M1 launch.** Rejected: locks out fiat-only donors; Stripe is still needed for the credit-card path and for subscription billing.
- **Stripe-only at M1 launch.** Rejected: no demonstration of cross-server federated tipping (the pilot's headline feature).
- **LNBits as the primary Lightning rail (original ADR text).** Superseded: forces operator custody, triggers MiCA CASP at scale, and burns ~$700 in channel liquidity per operator just to demo. The pivot doc lists the full trade-off matrix.
- **Add MoonPay to M1.** Rejected: iOS SDK in private preview as of April 2026; web SDK works but adds complexity to ship in M1. Defer to M3 when subscription engine ships.

## Consequences

- M1 ships with the two best-supported, lowest-complexity providers — sufficient to demo the pilot's headline features.
- Default operator configuration requires only `STRIPE_SECRET_KEY` + `STRIPE_WEBHOOK_SECRET`. Lightning works as long as creators publish a Lightning Address; no Lightning-node env vars needed in the default path.
- Operators that opt into LNBits set `LNBITS_URL` + `LNBITS_API_KEY` and accept the custody / capital implications.
- Operator regulatory exposure is now **zero by default**: the operator never holds funds when creators use a Lightning Address.
- Settlement for the LNURL-pay path bypasses operator-side webhooks entirely. Donation status moves to `succeeded` via the donor-side `m.tip.proof` Matrix event (NIP-57-shaped receipt); see `MSC-XXXX-tip-events-draft.md`.
- Pinned BTC rate causes ~1-cent drift on round-trip conversions (verified by tests in `mm-payment/src/lnbits/types.rs`). Acceptable for pilot.
- Phase M6 adapters layer on without changing M1 contracts (the trait-based registry is extensible).

## References

- Implementation: `crates/mm-payment/src/{stripe,lnbits,lnurl}/`
- `crates/mm-db/migrations/V017__creator_lightning_address.sql` — schema for creator-published Lightning Addresses
- `crates/mm-api/src/monetization.rs::{create_donation, update_creator_profile}`
- `WorkingDirectory/docs/external/mm-demo-path-pivot.md` — the pivot rationale
- `WorkingDirectory/docs/external/m1-first-pr.md` (multi-provider donations API)
- `WorkingDirectory/docs/external/m1-pilot-kickoff.md` (provider env vars)
- LUD-06 (LNURL-pay): https://github.com/lnurl/luds/blob/luds/06.md
- LUD-16 (Lightning Address): https://github.com/lnurl/luds/blob/luds/16.md
