# MatrixMedia Federation Architecture

## Overview

MatrixMedia Phase 5 introduces federation support: users on one Matrix homeserver
can join streams hosted on MM instances paired with different homeservers. This
leverages Matrix's existing federation model for authentication and room state.

## Federation Model (v1)

**What v1 supports:**
- A user on server-a.org can join streams hosted on server-b.org's MM instance
- Each MM instance remains paired with a single Matrix homeserver
- OpenID tokens are validated against the token-issuer's homeserver (with allow/deny list)
- Stream discovery via Matrix room state events
- Service discovery via `.well-known/matrix/matrixmedia`

**What v1 does NOT support (deferred):**
- SFU cascading (all viewers connect to the origin SFU regardless of server)
- Peer-to-peer MM-to-MM discovery (no MM federation protocol)
- Multi-region origin routing
- Cross-server moderation

## Data Flow: Federated Stream Join

```
Alice on server-a.org wants to join Bob's stream on server-b.org's MM
─────────────────────────────────────────────────────────────────────

1. Bob creates stream on server-b.org's MM instance
   ↓
2. MM publishes state event to the Matrix room:
   { com.matrixmedia.stream:
     { mm_server_url: "https://mm.server-b.org",
       mm_matrix_server: "server-b.org",
       ... } }
   ↓
3. Matrix federation propagates state event to server-a.org
   ↓
4. Alice's client reads state event, sees mm_server_url
   ↓
5. Alice's client asks her Matrix server for an OpenID token:
   POST /_matrix/client/v3/user/@alice:server-a.org/openid/request_token
   ↓
6. Alice's client calls mm.server-b.org/_mm/client/v1/auth/token
   with the OpenID token (issued by server-a.org)
   ↓
7. mm.server-b.org checks:
   a. Is federation enabled? 
   b. Is server-a.org in allow_list (or not in deny_list)?
   c. Call https://server-a.org:8448/_matrix/federation/v1/openid/userinfo
   d. Verify returned sub matches @alice:server-a.org
   ↓
8. mm.server-b.org issues MM JWT for @alice:server-a.org
   ↓
9. Alice calls /streams/{id}/join on mm.server-b.org
   ↓
10. mm.server-b.org returns SFU token
    ↓
11. Alice connects directly to server-b's LiveKit SFU
```

## Trust Model

### Trust Layers

1. **Matrix homeserver** (per server): trusted for its own users' OpenID tokens
2. **Federation allow/deny list** (per MM operator): explicit server trust decisions
3. **E2EE key** (per stream, optional): stream-level confidentiality

### Who Can Do What

| Actor | Capability |
|---|---|
| User's home server | Issues OpenID tokens, attests to user identity |
| Foreign MM server | Validates OpenID via federation, issues stream access |
| Foreign SFU | Routes media (may see plaintext if no E2EE) |
| User's home server admin | Cannot eavesdrop on streams (if E2EE) |
| Foreign MM server operator | Can see stream metadata, participant lists |

### Federation Decision Tree

```
Request from federated user:
  └─> Is federation.enabled?
      ├─ No  → 403 Forbidden
      └─ Yes → Is server_name in deny_list?
                ├─ Yes → 403 Forbidden
                └─ No  → Is allow_list empty?
                          ├─ Yes → Validate against foreign HS
                          └─ No  → Is server_name in allow_list?
                                    ├─ Yes → Validate against foreign HS
                                    └─ No  → 403 Forbidden
```

## Service Discovery

### `.well-known/matrix/matrixmedia`

MM servers expose service info at:
`GET {public_url}/.well-known/matrix/matrixmedia`

Response:
```json
{
  "mm_server": {
    "base_url": "https://mm.example.org",
    "version": "0.1.0",
    "matrix_server": "example.org",
    "federation_enabled": true,
    "e2ee": {
      "enabled": true,
      "required": false,
      "algorithms": ["aes-gcm-256"]
    },
    "recording_enabled": true
  }
}
```

### Stream State Event

MM embeds its service info in every stream state event:

```json
{
  "type": "com.matrixmedia.stream",
  "content": {
    "stream_id": "mm_abc123",
    "mm_server_url": "https://mm.example.org",
    "mm_matrix_server": "example.org",
    "federation_enabled": true,
    ...
  }
}
```

## Security Considerations

### OpenID Token Validation

- The `matrix_server_name` in the OpenID token tells MM **which server to ask**
- MM must NOT trust the token's claimed user_id without validation
- The `sub` returned from the foreign homeserver is the **authoritative** user_id
- Verify `sub` ends with `:{matrix_server_name}` to prevent server-name spoofing

### Foreign Server Request Safeguards

- **Timeout**: bounded HTTP requests (default 10s) prevent slow-loris DoS
- **Cache**: validated tokens cached for 5 minutes (configurable)
- **Allow/Deny list**: operator controls which servers can federate
- **Rate limiting**: per-source-server rate limits (future enhancement)

### What Foreign Servers Can't Do

- Impersonate users on other servers (OpenID `sub` is verified)
- Access streams without Matrix room membership
- Modify stream state (only the stream's own MM can publish state events)
- Read E2EE media (keys distributed via Matrix room state, not federation)

## Configuration

In `matrixmedia.toml`:

```toml
[federation]
enabled = true
allow_list = []              # Empty = allow all (not in deny_list)
deny_list = ["spam.example"] # Blocked servers
validation_timeout_secs = 10
validation_cache_ttl_secs = 300
```

Environment variables:
- `MM_FEDERATION_ENABLED=true`
- `MM_FEDERATION_ALLOW_LIST=matrix.org,element.io`
- `MM_FEDERATION_DENY_LIST=spam.example.com`
- `MM_FEDERATION_VALIDATION_TIMEOUT_SECS=10`
- `MM_FEDERATION_VALIDATION_CACHE_TTL_SECS=300`

## Metrics

- `mm_federation_validations_total` — foreign OpenID validations attempted
- `mm_federation_rejections_total` — rejected due to allow/deny list
- `mm_federation_validation_errors_total` — network/parse errors
- `mm_federated_joins_total` — federated users joining streams
- `mm_federated_streams_total` — streams with federated viewers

## Future Work (Post-v1)

- **SFU cascading** — each server runs its own SFU, media relays between them
- **MM-to-MM protocol** — direct server-to-server discovery and coordination
- **Cross-server moderation** — host on server-a can moderate federated users
- **Federated recording storage** — recordings available across federation
- **Post-quantum OpenID validation** — future spec work
