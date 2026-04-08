# MatrixMedia -- Known Limitations

**Updated:** 2026-04-07

This document lists all known limitations of MatrixMedia at the current release.
Items are grouped by area. Each limitation notes the planned resolution where applicable.

---

## Streaming

1. **No end-to-end encryption of media through the SFU.**
   The LiveKit SFU sees all media in the clear. Self-hosters control their own SFU,
   so the trust boundary is explicit. Insertable Streams E2EE (Phase 4) encrypts
   between participants, but the SFU relay itself is not zero-knowledge.

2. **Token revocation window -- up to 60 seconds.**
   SFU access tokens are cached for up to 60 seconds. A revoked token may remain
   valid for the remainder of the cache TTL.

3. **Bot cannot read encrypted messages in E2EE rooms.**
   The appservice bot does not perform Olm/Megolm decryption. Bot commands
   (`!mm start`, `!mm donate`, etc.) will not work in encrypted rooms unless the
   room also has the bot verified as a session.

4. **Widget support limited to Element Web/Desktop and SchildiChat.**
   The SolidJS streaming widget relies on the Matrix Widget API, which is currently
   only implemented by Element Web, Element Desktop, and SchildiChat. Other clients
   (FluffyChat, Nheko, Cinny) can use bot commands but not the widget UI.

5. **Single stream per room.**
   A Matrix room can host at most one active MatrixMedia stream at a time. Starting
   a second stream while one is active returns `MM_STREAM_ACTIVE`.

6. **SQLite write serialization.**
   SQLite supports only one concurrent writer. For production workloads with many
   concurrent streams, PostgreSQL is recommended. Set `MM_DATABASE_URL` to a
   `postgres://` connection string.

---

## Monetization (Phase 7)

7. **Stripe only -- no PayPal or cryptocurrency.**
   Phase 7 supports Stripe Connect exclusively. PayPal and BTCPay Server (crypto)
   are planned for Phase 8 via the `PaymentProvider` trait abstraction.

8. **Mock provider for development only.**
   The `MockProvider` simulates payment flows without real charges. Full end-to-end
   testing of payment flows requires Stripe test-mode API keys
   (`sk_test_...` / `pk_test_...`).

9. **No real-time donation overlay via WebSocket.**
   The widget polls the donation feed endpoint every 3 seconds rather than receiving
   push notifications over a WebSocket. Overlay latency is therefore up to 3 seconds
   after the webhook is processed.

10. **Subscription proofs are not federated.**
    Entitlement checks (subscription tier verification) are single-server only.
    A user's subscription on Server A is not recognized on Server B. Federation of
    subscription proofs is planned for Phase 8+.

11. **No refund handling in the API.**
    Refunds must be processed through the Stripe Dashboard. There is no
    `POST /refund` endpoint. Creators and operators manage refunds directly in
    Stripe.

12. **Content gating preview uses a short SFU token.**
    When a gated stream is accessed without sufficient entitlement, the viewer
    receives a brief preview token. After the preview expires the viewer must
    subscribe and re-join the stream to receive a full-duration token.

13. **Donation message moderation is basic.**
    Messages are capped at 150 characters. There is no word filter, profanity
    detection, or moderation queue. Server operators should rely on Matrix room
    moderation tools for now.

14. **No email notifications for subscription events.**
    Subscription creation, renewal, cancellation, and payment failure do not
    trigger email notifications. Stripe may send its own receipts depending on
    the creator's Stripe settings.

15. **Entitlement cache TTL is 15 seconds.**
    The `EntitlementService` uses a moka cache with a 15-second TTL. A cancelled
    subscription may continue to grant access for up to 15 seconds after
    cancellation.

---

## Discovery (Phase 7c)

16. **Trending is rule-based only.**
    The trending algorithm uses interactions-per-hour with time decay. There is no
    machine learning model or collaborative filtering. Ranking quality depends
    entirely on the volume and recency of interaction signals.

17. **Personalized feeds require follow data -- cold start for new users.**
    The "For You" feed blends 80% followed-creator content, 10% trending, and
    10% discovery. A new user with no follows receives only trending and discovery
    content until they follow at least one creator.

18. **No full-text search.**
    There is no search endpoint for streams, creators, or categories by keyword.
    Discovery relies on the trending, for-you, and category browsing endpoints.

---

## General

- **USD only.** All monetary amounts are in US dollars. Multi-currency support is
  not yet implemented.
- **Fixed donation tiers.** The 7 donation amounts ($1, $2, $5, $10, $25, $50,
  $100) are hard-coded. Custom amounts are not supported.
- **Docker image does not yet include Phase 7 code.** The published
  `matrixmedia/mm-core:0.1.0` image predates monetization. Rebuild with
  `docker build -f infra/docker/Dockerfile -t matrixmedia/mm-core:0.2.0 .`
