# ADR-0007: Stripe + LNBits as primary M1 payment providers

## Status

Accepted (2026-04-26)

## Context

The M1 pilot ships a multi-provider donation flow. We must pick the provider implementations to ship on day 1.

Provider candidates evaluated:

| Provider | Type | Rust SDK quality | Operator complexity | M1 fit |
|---|---|---|---|---|
| **Stripe** (async-stripe) | Fiat processor | Production-grade | Medium (Connect onboarding) | ✅ |
| **LNBits** (HTTP API) | Lightning custodial | We hand-roll client | Low (config + LN node) | ✅ |
| **Strike API** | Lightning + fiat on-ramp | Beta / regional | Medium | ❌ M3+ |
| **MoonPay** | Fiat-to-crypto on-ramp | SDK-based | Medium | ❌ M3+ |
| **Bitnob** | African fiat-to-Lightning | Sandbox available | Medium | ❌ M6 |
| **PayPal** | Fiat | OK | High (compliance) | ❌ Not in scope |

## Decision

**M1 ships with two payment providers:**

1. **Stripe** — recurring subscriptions + one-off donations via Stripe Checkout, Connect Express for creator onboarding. Already integrated in `crates/mm-payment/src/stripe/`.
2. **LNBits** — Lightning invoice creation + webhook for payment confirmation. Already integrated in `crates/mm-payment/src/lnbits/`. Operator points it at their own Lightning node (Voltage hosted recommended for pilot).

The `PaymentProviderRegistry` in `mm-payment/src/registry.rs` allows additional providers (M6 adapters: Bitnob, Tando, Strike Asia, Pouch, MoonPay) without touching handler code.

For the pilot, the BTC↔USD rate is hardcoded at 1500 sats/USD in `mm-payment/src/lnbits/types.rs`. A live oracle (Coingecko or Kraken) is M3 work.

## Alternatives considered

- **Lightning-only at M1 launch.** Rejected: locks out fiat-only donors; Stripe is needed for the credit-card path.
- **Stripe-only at M1 launch.** Rejected: no demonstration of cross-server federated tipping (the pilot's headline feature).
- **Add MoonPay to M1.** Rejected: iOS SDK in private preview as of April 2026; web SDK works but adds complexity to ship in M1. Defer to M3 when subscription engine ships.

## Consequences

- M1 ships with the two best-supported, lowest-complexity providers — sufficient to demo the pilot's headline features.
- Operators must configure `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `LNBITS_URL`, `LNBITS_API_KEY` in env. Documented in operator setup guide.
- Pinned BTC rate causes ~1-cent drift on round-trip conversions (verified by tests in `mm-payment/src/lnbits/types.rs`). Acceptable for pilot.
- Phase M6 adapters layer on without changing M1 contracts (the trait-based registry is extensible).

## References

- Implementation: `crates/mm-payment/src/{stripe,lnbits}/`
- `WorkingDirectory/docs/external/m1-first-pr.md` (multi-provider donations API)
- `WorkingDirectory/docs/external/m1-pilot-kickoff.md` (provider env vars)
