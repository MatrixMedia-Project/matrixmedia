# Secret rotation runbooks (compose path)

Companion to [secrets-inventory.md](secrets-inventory.md). The tooled entry
point is:

```bash
mmctl rotate <SECRET_NAME> [--dry-run] [--yes]
mmctl rotate --list
```

`--dry-run` prints the exact ordered plan without reading or writing any
secret value. Prefer the tool over hand-typing — it encodes every ordering
rule below. All examples use placeholders only (`${MM_DOMAIN}`, `$MM_ROOT`,
default `/opt/mm`); never paste real values into a shell history or a doc.

## The five-phase pattern

Every rotation follows the same skeleton:

```
Phase 0  PRECHECK   mmctl doctor green; backup .env.secrets + $MM_ROOT/secrets/
                    + $MM_ROOT/config/ to $MM_ROOT/rotate-backups/<ts>/ (mode 700)
Phase 1  GENERATE   openssl rand -hex N -> _upsert_secret KEY NEWVAL
                    (gen_secret cannot be used: it refuses to overwrite)
Phase 2  PROPAGATE  write_secret_files (if KEY is one of the 4 file-backed secrets);
                    render_templates (if KEY appears in any templates/*.tmpl.*);
                    external store mutation where one exists (ALTER ROLE for Postgres)
Phase 3  RESTART    recreate consumers IN ORDER, one compose invocation
                    (verifier before signer; store before client)
Phase 4  VERIFY     targeted probe (below) + mmctl doctor
Phase 5  INVALIDATE negative probe with the old value; purge the backup after
                    the soak window (it contains the OLD secret)
```

**The single most likely operator mistake:** `docker compose restart` does
**not** re-read env files. Rotation restarts must always be
`docker compose ... up -d --force-recreate <services>` — the tool does this
for you.

## Dependency map

| Secret | secret files? | re-render? | external store step | recreate, in order |
|---|---|---|---|---|
| `LK_API_SECRET` | no | yes (livekit/egress/ingress) | — | livekit, livekit-egress, livekit-ingress, mm-core, lk-jwt-service |
| `MM_AS_TOKEN` / `MM_HS_TOKEN` | no | yes (`mm_appservice.yaml`) | — | synapse, mm-core |
| `MM_ADMIN_TOKEN` | no | no | — | mm-core |
| `MM_JWT_SIGNING_KEY` | no | no | — | mm-core (runbook B) |
| `MM_SWITCH_AUTH_SECRET` | no | no | — | mm-switch, mm-core (runbook A) |
| `MM_SIGNUP_IP_HASH_PEPPER` | yes | no | — | mm-core |
| `SYNAPSE_REGISTRATION_SECRET` | yes | yes (`homeserver.yaml`) | — | synapse, mm-core |
| `SYNAPSE_MACAROON_SECRET` / `SYNAPSE_FORM_SECRET` | no | yes (`homeserver.yaml`) | — | synapse |
| `POSTGRES_SYNAPSE_PASS` | no | yes (`homeserver.yaml`) | `ALTER ROLE synapse` | synapse (runbook C) |
| `POSTGRES_APP_ADMIN_PASS` | yes | yes (init SQL, future rebuilds) | `ALTER ROLE mm_admin` | mm-core (runbook C) |
| `POSTGRES_APP_PASS` | yes | yes (init SQL, future rebuilds) | `ALTER ROLE mm_app` | none (runbook C) |
| `MINIO_ROOT_PASSWORD` | no | no | — | minio |
| `REDIS_PASSWORD` | no | yes (`livekit.yaml`) | — | lk-redis, livekit, livekit-egress, livekit-ingress |
| `TURN_PASS` | no | no | — | coturn, mm-switch |
| `MM_SYNAPSE_ADMIN_TOKEN` | no | no | owner re-login (`capture_admin_token`) | mm-core |
| `LK_API_KEY`, `MINIO_ROOT_USER`, `TURN_USER` | paired literals — rotate only together with their paired secret | | | |
| `TURN_SECRET`, `GRAFANA_ADMIN_PASSWORD` | dead secrets — nothing to rotate; flagged for removal (see inventory) | | | |

---

## Runbook A: `MM_SWITCH_AUTH_SECRET` — dual recreate

**Why tricky.** mm-core *signs* stream-control HMAC tokens with this secret;
mm-switch *verifies* with the same single secret. There is no second-secret
acceptance in v1, so any restart ordering has a window where freshly-signed
tokens fail.

**Mitigating facts.** Token TTLs are short (300 s publisher/viewer grants,
60 s server calls) and auth happens only at offer/control time — established
WebRTC sessions and in-progress recordings are not re-authenticated, so live
streams survive the window.

**Procedure (v1 — accepts a ~5–10 s control-plane blip):**

1. PRECHECK per pattern; confirm no broadcast is mid-*start* (in-flight
   streams are safe; new offer/switch/record calls are not).
2. `mmctl rotate MM_SWITCH_AUTH_SECRET` — generates and upserts the new value.
3. The tool recreates **both consumers in one compose invocation**, verifier
   first: `up -d --force-recreate mm-switch mm-core`. (Never `restart`: it
   keeps the stale env.)
4. VERIFY: mm-switch startup log shows `HMAC auth: enabled`; start a probe
   stream end-to-end; the switch's auth-rejection metrics counter is not
   climbing.
5. INVALIDATE: tokens signed with the old secret expire on their own within
   300 s; a token minted from the old secret must get 401.

**Interrupted mid-rotation?** Signer and verifier disagree → all *new*
streams fail until both are recreated. Rollback: restore `.env.secrets` from
`$MM_ROOT/rotate-backups/<ts>/`, recreate both again.

---

## Runbook B: `MM_JWT_SIGNING_KEY` — forced re-auth

**Why tricky.** mm-core issues HS256 JWTs with no `kid` and validates against
the single configured key. Outstanding tokens: sessions ~15 min, refresh
tokens 24 h, admin sessions 24 h. v1 deliberately uses **forced re-auth**: the
moment mm-core restarts with a new key, every outstanding MM session, refresh
and admin JWT is cryptographically dead.

**What is NOT affected:** Matrix access tokens — they are Synapse's, not
mm-core's. Users stay logged into chat; only MM-specific sessions
(monetization, streaming control, admin console) re-authenticate.

**Procedure:**

1. PRECHECK; if the instance has active paying viewers, announce a
   maintenance moment — every MM session will silently re-auth or re-login.
2. `mmctl rotate MM_JWT_SIGNING_KEY` (new 64-hex value = 64 bytes, well above
   the 32-byte config floor).
3. The tool recreates mm-core (sole consumer).
4. VERIFY: a fresh login issues a working session; a pre-rotation JWT gets 401.
5. INVALIDATE: nothing extra — old-key tokens are dead by construction.

Rotate this key on suspicion of compromise, or per your cadence policy; the
cost is bounded by one re-login per user.

---

## Runbook C: Postgres passwords

Covers `POSTGRES_SYNAPSE_PASS`, `POSTGRES_APP_ADMIN_PASS`, `POSTGRES_APP_PASS`.

**Why tricky.** The compose env vars (`POSTGRES_PASSWORD`,
`POSTGRES_PASSWORD_FILE`) are **initdb-only** — they set the password the
first time the data volume is created and are inert afterwards. Editing
`.env.secrets` and restarting changes **nothing** in the database. The live
password must be changed with `ALTER ROLE` *inside* the DB, and the env/file
copies updated to match (the database is the consumer of record).

**Mitigating fact.** `ALTER ROLE ... PASSWORD` affects only *new*
connections; existing pooled connections keep working, so the outage equals
one consumer recreate.

**Procedure for `POSTGRES_APP_ADMIN_PASS`** (`mm_admin` — used live by
mm-core):

1. PRECHECK; the Phase-0 backup MUST capture the old value — it is the only
   way back into the DB if a step is botched.
2. `mmctl rotate POSTGRES_APP_ADMIN_PASS` does, in order:
   - upsert the new value into `.env.secrets`;
   - `write_secret_files` (refreshes `$MM_ROOT/secrets/mm_db_admin_password`,
     newline-free);
   - `render_templates` (the value sits in the init SQL — that file never
     re-runs on the live volume, but a future volume rebuild must initialise
     with the *current* value);
   - `ALTER ROLE mm_admin PASSWORD ...` inside the mm-postgres container via
     the local superuser socket (trust auth — never blocked by the credential
     being rotated; the value travels via stdin, never argv, never logs);
   - `up -d --force-recreate mm-core` so its DB URLs re-interpolate.
3. VERIFY: mm-core healthcheck green; `psql` login with the new password
   succeeds.
4. INVALIDATE: `psql` login with the old password fails; purge the backup
   after the soak window.

**`POSTGRES_SYNAPSE_PASS`** is the same shape, with the ALTER running in the
`postgres` service (role `synapse`), `render_templates` **required** (the
password is baked into `homeserver.yaml`), then recreate `synapse`. Outage =
one Synapse restart; clients retry, federation queues.

**`POSTGRES_APP_PASS`** is the easy case: the `mm_app` role has no live
consumer in the compose file, so it is ALTER + secret-file refresh with no
restarts.

**Interrupted mid-rotation?** env/DB mismatch → the consumer crash-loops on
reconnect. Rollback: `ALTER ROLE ... PASSWORD` back to the old value via the
container-local superuser socket (which never depends on the rotated value),
restore `.env.secrets` from the backup, recreate the consumer.

---

## All remaining secrets (one-liners)

- `LK_API_SECRET`: upsert → re-render (3 LiveKit configs) → recreate livekit,
  livekit-egress, livekit-ingress, then mm-core + lk-jwt-service.
  **In-room media drops when livekit restarts — this is the most disruptive
  rotation; schedule it.**
- `MM_AS_TOKEN` / `MM_HS_TOKEN`: rotate both, back to back → re-render
  (`mm_appservice.yaml`) → recreate synapse, then mm-core. Appservice
  transactions 403+queue during the window and retry; self-heals.
- `MM_ADMIN_TOKEN`: upsert → recreate mm-core (the healthcheck label
  re-interpolates on recreate).
- `SYNAPSE_REGISTRATION_SECRET`: upsert → secret file + re-render → recreate
  synapse, then mm-core. Signup briefly errors.
- `SYNAPSE_MACAROON_SECRET` / `SYNAPSE_FORM_SECRET`: upsert → re-render →
  recreate synapse. Synapse derives long-lived material from the macaroon
  key — rotate it **only on suspicion of compromise**; expect re-login of
  password-login flows.
- `MM_SIGNUP_IP_HASH_PEPPER`: upsert → secret file → recreate mm-core. No
  user-visible effect.
- `REDIS_PASSWORD`: upsert → re-render (`livekit.yaml`) → recreate lk-redis,
  then livekit + egress + ingress together. Active calls drop.
- `TURN_PASS`: upsert → recreate coturn, then mm-switch. Established relays
  drop and ICE-restart.
- `MINIO_ROOT_PASSWORD`: upsert → recreate minio.
- `MM_SYNAPSE_ADMIN_TOKEN`: not generated — `mmctl rotate` prompts for the
  owner's credentials and re-logs-in (`capture_admin_token`), then recreates
  mm-core. If you are rotating because the token leaked, also log out that
  device via the Synapse admin API.
- Paired literals (`LK_API_KEY`, `MINIO_ROOT_USER`, `TURN_USER`): rotate only
  together with their paired secret; the tool refuses them with a pointer.
- Dead secrets (`TURN_SECRET`, `GRAFANA_ADMIN_PASSWORD`): nothing to rotate —
  flagged for removal, see the inventory.
- Operator-supplied (`MM_STRIPE_SECRET_KEY`, `MM_STRIPE_WEBHOOK_SECRET`,
  `MM_LNBITS_INVOICE_KEY`, `MM_LNBITS_ADMIN_KEY`): rotate at the provider,
  paste the new value into `$MM_ROOT/.env`, then
  `up -d --force-recreate mm-core`.

## Verification probes

Run on the deployment host; placeholders only.

```bash
# Stack health after any rotation
mmctl doctor && mmctl status

# MM_ADMIN_TOKEN: new value works (reads the value inline, never echoes it)
curl -sf -H "Authorization: Bearer $(grep '^MM_ADMIN_TOKEN=' "$MM_ROOT/.env.secrets" | cut -d= -f2-)" \
  "https://matrix.${MM_DOMAIN}/_mm/admin/v1/health"

# MM_JWT_SIGNING_KEY: fresh login round-trip; a pre-rotation JWT must get 401

# MM_SWITCH_AUTH_SECRET: mm-switch log shows HMAC auth enabled; rejection
# counters flat (run from any container on the internal network)
mmctl logs mm-switch | grep -i 'HMAC auth'

# Postgres: positive login with the new password, negative with the old
# (password supplied via PGPASSWORD/stdin, never argv)
```

## Rollback (uniform)

Every rotation's Phase 0 writes
`$MM_ROOT/rotate-backups/<timestamp>/{.env.secrets,secrets/,config/}`
(mode 700 — it contains the OLD values; purge after the soak window).

Rollback = copy the three back, re-run the external-store step in reverse
where one exists (`ALTER ROLE ... PASSWORD` back via the container-local
superuser socket), `up -d --force-recreate <same recreate set>`, then
`mmctl doctor`. For `MM_JWT_SIGNING_KEY`, rollback re-invalidates the sessions
issued since rotation — acceptable, since rollback implies the rotation was
faulty.

## Kubernetes / Helm path

The charts consume pre-existing named Secrets (`mm-core-secrets`, etc.)
emitted by the platform. Rotation there = update the value in the platform's
secret store, let it re-emit the Secret, then **explicitly**
`kubectl rollout restart deploy/<consumer>` — the Deployment checksum
annotation covers only the ConfigMap, so a refreshed Secret does not roll pods
by itself. A managed story (External Secrets Operator) is planned; until then
the manual rollout restart is mandatory.
