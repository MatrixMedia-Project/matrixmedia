# MatrixMedia E2EE Security Architecture

## Overview

MatrixMedia Phase 4 introduces End-to-End Encryption (E2EE) for media streams.
When E2EE is enabled, media frames are encrypted on the sender's device before
being sent to the SFU, and decrypted only on the receiver's device. The SFU
sees only encrypted bytes and cannot access plaintext media.

## Threat Model

### Threats We Defend Against

1. **Compromised SFU operator** -- cannot eavesdrop on streams
2. **SFU host infrastructure** -- compromised SFU server cannot access media
3. **Network observers** -- transport encryption (DTLS-SRTP) + frame encryption (E2EE)
4. **Malicious MM backend** -- cannot decrypt streams without the key
5. **Passive network attacker** -- both layers of encryption prevent eavesdropping

### Threats We Do NOT Defend Against (v1)

1. **Compromised participants** -- any stream participant has the key and can record
2. **Key leak via Matrix** -- if Matrix room is plaintext, the key is visible to all members
3. **Active MITM before E2EE handshake** -- key distribution trust model depends on Matrix
4. **Stream metadata leakage** -- stream title, participant list, timing are visible to SFU
5. **Traffic analysis** -- frame sizes, timing patterns are observable

### Trust Assumptions

- **Matrix homeserver is trusted for key distribution** -- if you can't trust your homeserver,
  use an E2EE Matrix room (Megolm) to encrypt the state event that carries the key
- **Host is trusted** -- they have the plaintext key and can record/leak it
- **Participants are trusted** -- same as host

## Architecture

### Key Management

- **Shared room key**: 32 bytes (AES-GCM-256)
- **Generation**: random, cryptographically secure (rand::rng crate)
- **Distribution**: Matrix state event `com.matrixmedia.stream.e2ee_key`
- **State key**: stream_id (multiple streams can coexist)
- **Rotation**: on stream start, host change, or configurable interval (default 1h)

### Key Distribution Flow

```
1. Host creates stream with e2ee=true
2. MM backend generates random 32-byte key + key_id
3. MM backend publishes state event to Matrix room:
   { type: "com.matrixmedia.stream.e2ee_key",
     state_key: "<stream_id>",
     content: { key_b64, key_id, generation, algorithm, rotated_at_ms } }
4. MM backend returns key to host in API response
5. Host's SDK configures LiveKit KeyProvider with the key
6. When viewer joins, MM backend returns current key in /join response
7. Viewer's SDK configures LiveKit KeyProvider with the key
8. LiveKit SDK encrypts outgoing frames (host) / decrypts incoming (viewers)
```

### Key Rotation Flow

```
1. Host calls POST /streams/{id}/rotate-key
2. MM backend generates new key with incremented generation
3. MM backend publishes updated state event to Matrix
4. MM backend returns new key to host
5. Viewers receive new key via Matrix state sync
6. All clients update KeyProvider with new key
7. Old key invalidated after grace period (frames from old generation rejected)
```

## Encryption Algorithm

### AES-GCM-256

- **Cipher**: AES-256 in GCM mode
- **Key size**: 32 bytes (256 bits)
- **Nonce**: 12 bytes (96 bits), per-frame random
- **Tag**: 16 bytes (128 bits)
- **AAD**: frame metadata (timestamp, participant_id, track_id)

This is the LiveKit default and widely supported across browsers and mobile platforms.

### Why AES-GCM over ChaCha20-Poly1305?

- Hardware acceleration on most platforms (AES-NI on x86, ARMv8-Crypto)
- Mandatory in TLS 1.3
- LiveKit SDK default

PTT v1 used XChaCha20-Poly1305 because it used a 24-byte nonce (no reuse risk with
seq counters). LiveKit's Insertable Streams use AES-GCM with per-frame random nonces.

## Matrix Room Modes

### Plaintext Matrix Room (most common)

- State events are visible to all room members
- The E2EE key is visible to anyone in the room
- Only prevents SFU eavesdropping, not room-member eavesdropping
- **Use case**: public community streams where anyone in the room is trusted

### E2EE Matrix Room (Megolm)

- State events are encrypted with Megolm by the Matrix homeserver
- The E2EE key is only visible to authenticated room members with Megolm keys
- Prevents both SFU and non-room-member eavesdropping
- **Use case**: private corporate/family streams with verified participants

## What the SFU Can See

Even with E2EE:

- **Participant identities** (via LiveKit participant_id)
- **Track types** (audio/video/screen share)
- **Frame timing and sizes**
- **Connection quality metrics**
- **Who is publishing vs subscribing**

E2EE protects media **content**, not **metadata**.

## Configuration

In `matrixmedia.toml`:

```toml
[e2ee]
enabled = false          # Master switch
required = false         # If true, refuse non-E2EE streams
key_rotation_interval_secs = 3600
algorithm = "aes-gcm-256"
```

Environment overrides: `MM_E2EE_ENABLED`, `MM_E2EE_REQUIRED`, etc.

## Operational Guidance

### For Operators

- **Enable E2EE** (`MM_E2EE_ENABLED=true`) if your deployment handles sensitive content
- **Require E2EE** (`MM_E2EE_REQUIRED=true`) to prevent plaintext streams entirely
- Use **shorter rotation intervals** for higher-security deployments
- **Deploy SFU in a separate trust zone** from MM backend (defense in depth)

### For Developers

- Call `createStream({ e2ee: true })` from clients to enable E2EE
- Check the `e2ee` field in API responses to know if E2EE is active
- Show a UI indicator (lock icon) when E2EE is active
- Handle key rotation events (state event changes) gracefully

### For Users

- Look for the lock icon to confirm E2EE is active
- Verify you're in a room with only trusted participants
- Understand that the stream **host** can still record the stream

## Future Improvements (Post v1)

- **Per-device keys** via MLS (Messaging Layer Security)
- **Device verification** via Matrix cross-signing
- **Key backup** via Matrix SSSS
- **Forward secrecy** via regular key rotation
- **Post-quantum crypto** evaluation
