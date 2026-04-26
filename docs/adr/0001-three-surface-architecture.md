# ADR-0001: Three-surface architecture

## Status

Accepted (2026-04-26)

## Context

MatrixMedia must serve three distinct constituencies whose constraints contradict each other:
- **Casual users** want native mobile apps from the App Store / Play Store
- **Power users / privacy-conscious / EU users** want full features without app-store gatekeeping
- **Apple App Store** rejects in-app purchases that bypass IAP, custodial Lightning wallets, and certain crypto features

Trying to satisfy all three with a single binary forces UX compromises that hurt every user group.

## Decision

Ship **three distinct surfaces**, each with the maximal feature set its constraints permit:

1. **iOS App Store native** — Damus-shape: NWC wallet pairing only, P2P tips with no in-app benefit unlock, no fiat top-up, no in-app subscription purchase. Ships under our Apple Developer account.
2. **Web app** — full feature kit (NWC, custodial mode, fiat top-up via Stripe Crypto Onramp / MoonPay, subscription purchase). Browser is jurisdiction-neutral.
3. **EU AltStore PAL native + Android (Play / F-Droid / direct APK)** — full feature kit. EU iOS uses AltStore PAL distribution under the EU DMA. Android already permits sideloading universally.

## Alternatives considered

- **Single binary, lowest-common-denominator features.** Rejected: would strip subscriptions and fiat top-up from every user, killing the commercial story.
- **Single binary, maximal features.** Rejected: guaranteed App Store rejection (Damus precedent).
- **Two surfaces (web + native).** Rejected: native iOS users in EU lose the DMA-enabled features unnecessarily.

## Consequences

- Multiple build configurations to maintain (one per surface). Mitigated by shared codebases (Rust mm-core for backend, TypeScript for web, separate Swift/Kotlin for native).
- Documentation and onboarding must steer users to the right surface for their needs. Trade-off accepted.
- App Store native build is a **marketing funnel**, not the revenue surface. Web is the revenue surface.
- EU users get the best of both worlds (native UX + full features). Non-EU iOS users either use the App Store light client or the web.

## References

- `WorkingDirectory/docs/external/mm-final-design.md` §2
- `WorkingDirectory/docs/external/eu-deployment-landscape.md` (AltStore PAL + DMA analysis)
- Damus App Store precedent (`monetization-technical-stack.md` §7)
