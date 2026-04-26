# ADR-0004: Operator manifest at `.well-known/matrixmedia/operator.json`

## Status

Accepted (2026-04-26)

## Context

Operators in a federated system need a way to advertise:
- Custody posture (non-custodial vs custodial-licensed)
- Available payment providers (Lightning, Stripe, etc.)
- Tip protocol versions supported
- Compliance metadata (jurisdiction, contact, attestations)

Other operators and clients need a deterministic location to fetch this without prior knowledge of the server.

The `.well-known/` URI prefix is the IETF-standard place for service-discovery metadata (RFC 8615).

## Decision

Operators publish their manifest at:

```
https://{server}/.well-known/matrixmedia/operator.json
```

This is reachable on the public client port (port 443 typically). The endpoint returns JSON conforming to the schema in `mm-final-design.md` §6 (key fields: `schema_version`, `operator.{legal_name,country_code}`, `custody.{model,license_number?}`, `payment_providers[]`, `tip_protocol_versions[]`).

For backward compatibility during M1, mm-core ALSO serves the same content at `/.well-known/matrix/matrixmedia` (the legacy path). The matrix-style namespace is deprecated post-M3.

## Alternatives considered

- **`.well-known/matrix/matrixmedia` only.** Rejected: collides with Matrix's `.well-known` convention and confuses non-Matrix tooling.
- **`.well-known/matrixmedia.json` (no subdirectory).** Rejected: inflexible for future MM-related discovery files.
- **Discovery via Matrix state event.** Rejected: requires a Matrix-aware client; HTTP `.well-known` works for any tool (curl, fetch, etc.).
- **DNS TXT record (`_matrixmedia`).** Rejected: harder to query; harder to update; not human-readable.

## Consequences

- Any tool that can fetch a URL can discover MatrixMedia capabilities for a server.
- Manifest must be served with `Content-Type: application/json`, `Cache-Control: max-age=300` (5 min TTL).
- Operators control the manifest; they can sign it (future ADR on signing).
- mm-api/src/wellknown.rs serves both the legacy `/well-known/matrix/matrixmedia` and the canonical `/.well-known/matrixmedia/operator.json` during M1; legacy path is deprecated post-M3.

## References

- `WorkingDirectory/docs/external/mm-final-design.md` §6
- RFC 8615 (Well-Known URIs)
- Implementation: `crates/mm-api/src/wellknown.rs`
