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
                    (not secrets/fleet-runner/) + $MM_ROOT/config/ to
                    $MM_ROOT/rotate-backups/<ts>/ (mode 700)
Phase 1  GENERATE   openssl rand -hex N -> _upsert_secret KEY NEWVAL
                    (gen_secret cannot be used: it refuses to overwrite);
                    then a copy of the updated .env.secrets is kept as
                    rotate-backups/<ts>/.env.secrets.after-generate (mode 600)
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
| `POSTGRES_FLEET_RUNNER_PASS` | no | no (compose interpolation) | `ALTER ROLE mm_fleet_runner` | mm-fleet-runner (runbook C) |
| `REDIS_PASSWORD` | no | yes (`livekit.yaml`) | — | lk-redis, livekit, livekit-egress, livekit-ingress |
| `TURN_PASS` | no | no | — | coturn, mm-switch |
| `MM_SYNAPSE_ADMIN_TOKEN` | no | no | owner re-login (`capture_admin_token`) | mm-core |
| `LK_API_KEY`, `TURN_USER` | paired literals — rotate only together with their paired secret | | | |

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

Covers `POSTGRES_SYNAPSE_PASS`, `POSTGRES_APP_ADMIN_PASS`, `POSTGRES_APP_PASS`,
`POSTGRES_FLEET_RUNNER_PASS`.

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

**`POSTGRES_FLEET_RUNNER_PASS`** (`mm_fleet_runner` — used live by
mm-fleet-runner) has no secret file and no rendered template: only the compose
file reads it, so it is ALTER, then recreate `mm-fleet-runner`. The role must
already exist (`deploy/sql/mm_fleet_runner_role.sql` creates it without a
password); on a host where it does not, the ALTER step fails and the rollback
below applies. The runner restarts and takes the Postgres leader lock again.

mm-fleet-runner is **opt-in**: it sits behind the compose profile `fleet`, which
`MM_FLEET_RUNNER=true` in `.env` switches on for every `mmctl` verb (start, stop,
update, upgrade, rotate). The installer does not create its role, set its password
or create its key directory, so `mmctl rotate POSTGRES_FLEET_RUNNER_PASS` refuses
(`the fleet runner is not enabled (MM_FLEET_RUNNER is not true); see runbook C`)
before it writes a backup or touches `.env.secrets` until the switch is on.

To enable it, as root on the host, in this order:

```bash
: "${MM_ROOT:=/opt/mm}"
# the same compose call mmctl builds (its DC array)
DC=(docker compose --env-file "$MM_ROOT/versions.env" --env-file "$MM_ROOT/.env"
    --env-file "$MM_ROOT/.env.secrets" -f "$MM_ROOT/docker-compose.yml" -p matrixmedia)

# 1. After mm-core has started once (it runs the migrations), from the checkout you
#    installed from: create the role and its grants (idempotent), then set the password
#    from .env.secrets. The password goes in on stdin (printf is a shell builtin), the
#    same way `mmctl rotate` does it, never on a command line.
"${DC[@]}" exec -T mm-postgres psql -q -v ON_ERROR_STOP=1 -U postgres \
  -d matrixmedia -f - < deploy/sql/mm_fleet_runner_role.sql
pw="$(grep '^POSTGRES_FLEET_RUNNER_PASS=' "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
printf "ALTER ROLE mm_fleet_runner PASSWORD '%s';\n" "$pw" \
  | "${DC[@]}" exec -T mm-postgres psql -q -v ON_ERROR_STOP=1 -U postgres -d postgres -f -
unset pw

# 2. The key directory: mode 0700, owned by the image's `matrixmedia` uid, on a
#    filesystem with hard links (the runner writes its key via a temp file plus a hard
#    link, so no FAT/NFS).
uid="$("${DC[@]}" run --rm -T --no-deps --entrypoint id mm-core -u matrixmedia)"
gid="$("${DC[@]}" run --rm -T --no-deps --entrypoint id mm-core -g matrixmedia)"
install -d -m 0700 -o "$uid" -g "$gid" "$MM_ROOT/secrets/fleet-runner"

# 3. Switch it on (edit the line instead if .env already has one).
echo 'MM_FLEET_RUNNER=true' >> "$MM_ROOT/.env"

# 4. Start it.
mmctl start
```

The runner creates its private key (`secrets/fleet-runner/key.json`) on first start.
Rotation backups skip `secrets/fleet-runner/` on purpose; see its row in
`secrets-inventory.md` for how to back it up.

**Interrupted mid-rotation?** env/DB mismatch → the consumer crash-loops on
reconnect. Rollback: `ALTER ROLE ... PASSWORD` back to the old value via the
container-local superuser socket (which never depends on the rotated value),
restore `.env.secrets` from the backup, recreate the consumer — the uniform
rollback below, whose step 2 has the command.

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
- `MM_SETTINGS_ENCRYPTION_KEY`: current value → `MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS`,
  new value generated (a copy of `.env.secrets` holding it is kept as
  `rotate-backups/<ts>/.env.secrets.after-generate`) → recreate mm-core (it
  re-encrypts every stored secret at startup) → wait until
  `GET /_mm/admin/v1/settings` reports `safe_mode: false`,
  `encryption_key_configured: true`, `rows_on_previous_key: 0` and an empty
  `secret_problems` → drop `_PREVIOUS` → recreate mm-core again. Nothing user-visible.
  Any secret problem stops the run with `_PREVIOUS` kept (see Settings in the Operator
  Console). So does safe mode: with `MM_SETTINGS_SAFE_MODE` set, mm-core skips the
  stored settings and cannot confirm they decrypt, so remove it from `.env` before
  rotating (or before re-running to resume). A failed recreate, whether compose itself
  fails or the stack comes back unhealthy, stops with the reverse procedure below.
  `mmctl rotate` refuses to start unless the rendered `docker-compose.yml` passes both
  the key and `_PREVIOUS` to mm-core (re-run `install.sh` first).
  - **Interrupted?** Re-run `mmctl rotate MM_SETTINGS_ENCRYPTION_KEY`. While
    `_PREVIOUS` is set the tool resumes: it generates no new key, keeps both values,
    and continues from the mm-core recreate. Never delete `_PREVIOUS` by hand while
    stored secrets may still be on it.
  - **Rollback:** do **not** restore the phase-0 `rotate-backups/<ts>/.env.secrets`.
    mm-core may already have re-encrypted the stored secrets under the new key, and
    that file does not hold it. Reverse the rotation instead: in `.env.secrets` set
    `MM_SETTINGS_ENCRYPTION_KEY` to the old value (from `_PREVIOUS` if it is still
    set, else from the phase-0 backup) and `MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS` to
    the current value (also in `.env.secrets.after-generate`), run
    `mmctl start` (it recreates mm-core with the edited file, passing compose the
    same env files and project as every other mmctl command), and let mm-core
    re-encrypt back.
    `mmctl rotate MM_SETTINGS_ENCRYPTION_KEY` then resumes and drops `_PREVIOUS` once
    nothing is left on it.
- `REDIS_PASSWORD`: upsert → re-render (`livekit.yaml`) → recreate lk-redis,
  then livekit + egress + ingress together. Active calls drop.
- `TURN_PASS`: upsert → recreate coturn, then mm-switch. Established relays
  drop and ICE-restart.
- `MM_SYNAPSE_ADMIN_TOKEN`: not generated — `mmctl rotate` prompts for the
  owner's credentials and re-logs-in (`capture_admin_token`), then recreates
  mm-core. If you are rotating because the token leaked, also log out that
  device via the Synapse admin API.
- Paired literals (`LK_API_KEY`, `TURN_USER`): rotate only
  together with their paired secret; the tool refuses them with a pointer.
- Operator-supplied (`MM_STRIPE_SECRET_KEY`, `MM_STRIPE_WEBHOOK_SECRET`,
  `MM_LNBITS_INVOICE_KEY`, `MM_LNBITS_ADMIN_KEY`, S3 keys): rotate at the provider, then
  in Operator Console → System → **Settings** press **Replace**, enter the new value,
  **Save**, then **Apply & restart**. `.env` only seeds these: once mm-core has stored
  one (it does at the first start with `MM_SETTINGS_ENCRYPTION_KEY` and a non-empty
  value), the database value wins and a new value in `.env` is ignored (the dashboard
  says so), so until it is replaced in the dashboard mm-core keeps using the old,
  revoked key. Only without `MM_SETTINGS_ENCRYPTION_KEY` do they stay in
  `$MM_ROOT/.env`: paste the new value there and recreate mm-core (`mmctl start`).

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
(`secrets/` without `fleet-runner/`: the fleet runner's private key is not a rotation
artifact; mode 700 — it contains the OLD values; purge after the soak window). Right after
Phase 1 the same directory also gets `.env.secrets.after-generate`, holding the NEW
value; purge it with the rest.

Rollback:

1. Copy the three back into `$MM_ROOT`.
2. Postgres passwords only (the other secrets have no external store): step 1
   restored the files, not the database, so set the role's password back to the
   old value — before step 3, while the database container is still running. The
   old value is read from the restored `.env.secrets` and reaches `psql` on stdin
   over the container-local superuser socket, never on a command line (`printf`
   is a shell builtin). Pick the line for the rotated secret:

   ```bash
   # POSTGRES_APP_ADMIN_PASS: role=mm_admin svc=mm-postgres su=postgres db=postgres
   # POSTGRES_APP_PASS:       role=mm_app   svc=mm-postgres su=postgres db=postgres
   # POSTGRES_FLEET_RUNNER_PASS: role=mm_fleet_runner svc=mm-postgres su=postgres db=postgres
   # POSTGRES_SYNAPSE_PASS:   role=synapse  svc=postgres    su=synapse  db=synapse
   key=POSTGRES_APP_ADMIN_PASS role=mm_admin svc=mm-postgres su=postgres db=postgres
   : "${MM_ROOT:=/opt/mm}"
   old="$(grep "^$key=" "$MM_ROOT/.env.secrets" | head -1 | cut -d= -f2-)"
   printf "ALTER ROLE %s PASSWORD '%s';\n" "$role" "$old" \
     | docker exec -i "$(docker ps -q --filter label=com.docker.compose.project=matrixmedia \
         --filter label=com.docker.compose.service=$svc)" \
       psql -q -v ON_ERROR_STOP=1 -U "$su" -d "$db" -f -
   unset old
   ```

   This is the same statement `mmctl rotate` runs, with the old value instead of
   the new one. Re-running `mmctl rotate <secret>` instead is not a rollback: it
   generates yet another value.
3. Recreate the stack, so every container re-reads the restored env files, secret
   files and rendered config:

   ```bash
   mmctl stop && mmctl start
   ```

   mmctl passes compose every env file and the project name, as the rotation did.
   This takes the whole stack down briefly; the data volumes are kept. Do not use
   `mmctl restart` instead (a restarted container keeps its old environment), nor a
   bare `docker compose up` (it misses `versions.env`, `.env.secrets` and the
   project name `matrixmedia`).
4. Wait until the stack is healthy — `mmctl start` returns as soon as the
   containers are created — then run `mmctl doctor`. This waits up to five
   minutes for no container to be starting, unhealthy or restarting (the check
   `mmctl rotate` itself waits on):

   ```bash
   for i in $(seq 60); do
     docker ps -a --filter label=com.docker.compose.project=matrixmedia --format '{{.Status}}' \
       | grep -qE 'health: starting|unhealthy|Exited|Restarting' || break
     sleep 5
   done
   mmctl doctor
   ```

   If `mmctl doctor` still fails, `mmctl status` shows which container is not
   healthy and `mmctl logs <service>` why.

For `MM_JWT_SIGNING_KEY`, rollback re-invalidates the sessions issued since
rotation — acceptable, since rollback implies the rotation was faulty.

**Exception — `MM_SETTINGS_ENCRYPTION_KEY`:** never roll it back by restoring the
phase-0 `.env.secrets`; that can destroy the only copy of the key the stored secrets
are encrypted under. Use the reverse procedure in its entry above.

## Kubernetes / Helm path

The charts consume pre-existing named Secrets (`mm-core-secrets`, etc.)
emitted by the platform. Rotation there = update the value in the platform's
secret store, let it re-emit the Secret, then **explicitly**
`kubectl rollout restart deploy/<consumer>` — the Deployment checksum
annotation covers only the ConfigMap, so a refreshed Secret does not roll pods
by itself. A managed story (External Secrets Operator) is planned; until then
the manual rollout restart is mandatory.
