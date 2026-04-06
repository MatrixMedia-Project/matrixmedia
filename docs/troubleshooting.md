# MatrixMedia Troubleshooting Guide

This guide covers common operational issues, their symptoms, diagnosis steps, likely causes, and fixes. Use it alongside `docs/operations-runbook.md` for incident response.

---

## Client Can't Authenticate

**Symptoms**:
- Client receives HTTP 401 on `/_mm/client/v1/*` endpoints
- Error code: `M_UNKNOWN_TOKEN`, `M_MISSING_TOKEN`, or `MM_OPENID_INVALID`
- `mm_http_requests_total{status="401"}` elevated
- Users report "not logged in" despite valid Matrix session

**Diagnosis**:
1. Verify client sends `Authorization: Bearer <openid_token>` header
2. Check openid token is fresh: Matrix openid tokens expire after ~1 hour
3. Confirm homeserver is reachable from mm-core:
   ```bash
   curl -v https://matrix.example.com/_matrix/federation/v1/openid/userinfo?access_token=TEST
   ```
4. Inspect mm-core logs for `openid_verify_failed` events
5. Check `mm_openid_verifications_total{result="failure"}` metric

**Common Causes**:
- Token expired (client cache too long)
- Clock skew between mm-core and homeserver (> 5 min)
- Homeserver URL misconfigured in `MM_HOMESERVER_URL`
- TLS certificate validation failing (self-signed cert in prod)
- Homeserver federation endpoint disabled or firewalled

**Fixes**:
- Instruct client to refresh openid token from `/_matrix/client/v3/user/{userId}/openid/request_token`
- Sync clocks via NTP
- Correct `MM_HOMESERVER_URL` env var and restart
- Add homeserver CA cert to mm-core trust store
- Enable federation listener on homeserver (port 8448 or .well-known delegation)

---

## Stream Creation Fails

**Symptoms**:
- `POST /_mm/client/v1/streams` returns 4xx or 5xx
- Error codes: `MM_ROOM_FULL`, `MM_STREAM_ACTIVE`, `MM_SFU_UNAVAILABLE`, `MM_PERMISSION_DENIED`
- Clients show "could not start stream" toast

**Diagnosis**:
1. Check response code and body:
   ```bash
   curl -v -X POST http://mm-core:6167/_mm/client/v1/streams \
     -H "Authorization: Bearer $TOKEN" \
     -d '{"room_id":"!abc:example.com","kind":"voice"}'
   ```
2. Query active streams in room: `GET /_mm/admin/v1/streams?room_id=<room>`
3. Check SFU health: `GET /_mm/admin/v1/health` -> sfu field
4. Verify user power level in the Matrix room (need >= `mm.streams` PL)

**Common Causes**:
- **409 MM_STREAM_ACTIVE**: Previous stream did not clean up. Check for stale state event.
- **409 MM_ROOM_FULL**: Participant cap reached (see `MM_MAX_PARTICIPANTS_PER_ROOM`).
- **403 MM_PERMISSION_DENIED**: User lacks required power level.
- **503 MM_SFU_UNAVAILABLE**: LiveKit down or circuit breaker open.
- **500 MM_DB_ERROR**: Database write failed (disk full, connection pool exhausted).

**Fixes**:
- Force-stop stuck stream: `DELETE /_mm/admin/v1/streams/{stream_id}`
- Increase participant cap or split rooms
- Grant user adequate power level in Matrix room
- Restart LiveKit and wait for circuit breaker reset (30s)
- Free disk space, increase `MM_DB_POOL_SIZE`

---

## Join Latency Too High

**Symptoms**:
- `mm_join_latency_seconds` p95 > 2s
- Clients report multi-second delay between "Join" tap and audio flowing
- SLO breach alert fires

**Diagnosis**:
1. Break down latency stages via trace spans: openid -> db -> sfu token -> sfu join
2. Check slow query log: PostgreSQL `log_min_duration_statement = 500`
3. Check SFU response time: `curl -w "%{time_total}\n" http://livekit:7880`
4. Check network RTT client -> mm-core -> sfu -> TURN
5. Inspect `mm_sfu_request_duration_seconds` histogram

**Common Causes**:
- Slow openid token verification (homeserver overloaded)
- Database contention (missing index on `mm_participants`)
- SFU under load (too many rooms per LiveKit node)
- TURN relay in wrong region (client-to-TURN RTT > 100ms)
- mm-core CPU saturated

**Fixes**:
- Cache openid results briefly (30s TTL) if spec permits
- Add index: `CREATE INDEX ON mm_participants(stream_id, user_id)`
- Scale LiveKit horizontally; shard rooms by hash
- Deploy regional TURN servers closer to users
- Scale mm-core replicas; increase CPU limits

---

## Audio/Video Quality Issues

**Symptoms**:
- Users report choppy audio, frozen video, echo, distortion
- `mm_sfu_packet_loss_ratio` elevated
- Participants drop mid-stream

**Diagnosis**:
1. Ask reporter: network type (WiFi/cellular), device, OS, approximate RTT
2. Check LiveKit dashboard for participant-level stats (jitter, RTT, loss)
3. Test TURN reachability from client network:
   ```bash
   turnutils_uclient -u user -w pass -p 3478 turn.example.com
   ```
4. Review codec negotiation: Opus 32kbps is default for voice

**Common Causes**:
- Asymmetric NAT forcing TURN relay (bandwidth constrained)
- Corporate firewall blocking UDP 3478/5349 -> falls back to TCP, higher latency
- Client on cellular with packet loss > 3%
- Echo: missing AEC on mobile client, or dual participants in same physical room
- CPU throttled on low-end device causing encoder underrun

**Fixes**:
- Enable TURN TCP fallback on port 443
- Reduce bitrate in LiveKit room config for constrained links
- Enable hardware AEC in client SDKs (`echoCancellation: true`)
- Add bandwidth adaptation / simulcast for video
- Rate-limit max_bitrate per participant

---

## E2EE Decryption Failures

**Symptoms**:
- Client shows "unable to decrypt" placeholder in stream
- Recipients cannot hear speaker despite join success
- `mm_e2ee_decrypt_errors_total` elevated (client-side metric)

**Diagnosis**:
1. Confirm all participants in room have verified Olm/Megolm session
2. Check mm-core did NOT touch ciphertext (it must be opaque):
   ```bash
   grep -i "decrypt\|plaintext" mm-core.log
   ```
3. Verify SRTP key exchange happened over Matrix to-device messages
4. Check room encryption state event: `m.room.encryption` with `m.megolm.v1.aes-sha2`
5. Review client sender-key rotation schedule

**Common Causes**:
- Participant joined after key exchange; no backfill of keys
- Device not verified; client policy blocks decryption
- Key rotation mid-stream; late joiners missing new epoch
- Clock skew breaking MAC validation
- mm-core misconfigured to log/inspect SRTP payloads (security bug)

**Fixes**:
- Trigger key rotation on membership change: update `com.matrixmedia.keys` state event
- Enable key backup / cross-signing on client
- Ensure mm-core SFU is in "end-to-end" mode (passes SRTP untouched)
- Sync clocks; relax MAC window if in debug
- Review mm-core config for `e2ee.passthrough = true`

See also: `docs/e2ee-key-rotation.md`, `docs/e2ee-security.md`.

---

## Federation Rejections

**Symptoms**:
- Remote user cannot join local stream
- Error: `MM_FEDERATION_DENIED`, `MM_SIGNATURE_INVALID`, or 403 from remote homeserver
- Logs show `signature_verification_failed` or `destination_not_allowed`

**Diagnosis**:
1. Confirm remote server is federated in Matrix first (can they send messages?)
2. Verify mm-core federation feature flag enabled: `MM_FEDERATION_ENABLED=true`
3. Check signing key advertised via `.well-known/matrix/server` and `/_matrix/key/v2/server`
4. Inspect federation log: `grep federation mm-core.log`
5. Validate signature manually against remote server's published keys

**Common Causes**:
- Remote homeserver signing key rotated; cache stale
- `matrix.example.com` DNS not resolvable from remote peer
- ACL denies the remote server in room state (`m.room.server_acl`)
- mm-core federation endpoint not exposed (firewall, missing ingress)
- Clock skew causing `origin_server_ts` to appear future-dated

**Fixes**:
- Force key refresh: `DELETE /_mm/admin/v1/federation/keys/{server_name}`
- Fix DNS / SRV records / `.well-known` delegation
- Remove restrictive server ACL or whitelist the remote
- Expose `/_matrix/federation/*` routes through ingress
- NTP sync; allow +/- 60s tolerance

See also: `docs/federation-architecture.md`, `docs/federation-operator-guide.md`.

---

## WebSocket Disconnects

**Symptoms**:
- Clients report frequent reconnects ("signaling dropped")
- `mm_ws_connections_closed_total{reason!="client_close"}` elevated
- Stream pauses for 2-5s periodically

**Diagnosis**:
1. Check close codes: `mm_ws_connections_closed_total` by `reason` label
2. Inspect client logs for WS close reason/code
3. Verify ingress/load-balancer idle timeout > ping interval
4. Test from known-good network vs reporter's network
5. Check mm-core goroutine count: leak would cause OOM kills

**Common Causes**:
- LB idle timeout < WS ping interval (e.g., AWS ALB 60s, nginx 75s)
- Proxy buffering WS frames (nginx without `proxy_buffering off`)
- Client behind mobile NAT that rebinds ports every ~30s
- mm-core restart during deploy (missing graceful drain)
- Backpressure: client can't keep up, server force-closes slow consumers

**Fixes**:
- Set LB idle timeout >= 120s
- Configure nginx:
  ```nginx
  proxy_buffering off;
  proxy_read_timeout 3600s;
  proxy_send_timeout 3600s;
  ```
- Reduce client ping interval to 20s
- Enable graceful shutdown with 30s drain window
- Tune `MM_WS_WRITE_BUFFER_SIZE` and slow-consumer threshold

---

## General Diagnostic Checklist

When triaging any issue, collect:

1. **Timestamp range** of the issue
2. **Affected users/rooms/streams** (IDs)
3. **Client platform + version**
4. **mm-core version**: `curl http://mm-core:6168/_mm/admin/v1/version`
5. **Recent deployments**: `kubectl rollout history deployment/matrixmedia`
6. **Relevant metrics** from Grafana dashboard
7. **Correlated logs** (use request_id if available)

Attach these when escalating.
